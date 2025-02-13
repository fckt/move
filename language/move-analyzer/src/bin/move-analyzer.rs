// Copyright (c) The Diem Core Contributors
// Copyright (c) The Move Contributors
// SPDX-License-Identifier: Apache-2.0

use anyhow::Result;
use clap::Parser;
use crossbeam::channel::{bounded, select};
use lsp_server::{Connection, Message, Notification, Request, Response};
use lsp_types::{
    notification::Notification as _, request::Request as _, CompletionOptions, Diagnostic,
    HoverProviderCapability, OneOf, SaveOptions, TextDocumentSyncCapability, TextDocumentSyncKind,
    TextDocumentSyncOptions, TypeDefinitionProviderCapability, WorkDoneProgressOptions,
};
use std::{
    collections::BTreeMap,
    path::Path,
    sync::{Arc, Mutex},
};

use move_analyzer::{
    completion::on_completion_request,
    context::Context,
    symbols,
    vfs::{on_text_document_sync_notification, VirtualFileSystem},
};
use move_symbol_pool::Symbol;
use url::Url;

#[derive(Parser)]
#[clap(author, version, about)]
struct Options {}

fn main() {
    // For now, move-analyzer only responds to options built-in to clap,
    // such as `--help` or `--version`.
    Options::parse();

    // stdio is used to communicate Language Server Protocol requests and responses.
    // stderr is used for logging (and, when Visual Studio Code is used to communicate with this
    // server, it captures this output in a dedicated "output channel").
    let exe = std::env::current_exe()
        .unwrap()
        .to_string_lossy()
        .to_string();
    eprintln!(
        "Starting language server '{}' communicating via stdio...",
        exe
    );

    let (connection, io_threads) = Connection::stdio();
    let mut context = Context {
        connection,
        files: VirtualFileSystem::default(),
        symbols: Arc::new(Mutex::new(symbols::Symbolicator::empty_symbols())),
    };
    let capabilities = serde_json::to_value(lsp_types::ServerCapabilities {
        // The server receives notifications from the client as users open, close,
        // and modify documents.
        text_document_sync: Some(TextDocumentSyncCapability::Options(
            TextDocumentSyncOptions {
                open_close: Some(true),
                // TODO: We request that the language server client send us the entire text of any
                // files that are modified. We ought to use the "incremental" sync kind, which would
                // have clients only send us what has changed and where, thereby requiring far less
                // data be sent "over the wire." However, to do so, our language server would need
                // to be capable of applying deltas to its view of the client's open files. See the
                // 'move_analyzer::vfs' module for details.
                change: Some(TextDocumentSyncKind::Full),
                will_save: None,
                will_save_wait_until: None,
                save: Some(
                    SaveOptions {
                        include_text: Some(true),
                    }
                    .into(),
                ),
            },
        )),
        selection_range_provider: None,
        hover_provider: Some(HoverProviderCapability::Simple(true)),
        // The server provides completions as a user is typing.
        completion_provider: Some(CompletionOptions {
            resolve_provider: None,
            // In Move, `foo::` and `foo.` should trigger completion suggestions for after
            // the `:` or `.`
            // (Trigger characters are just that: characters, such as `:`, and not sequences of
            // characters, such as `::`. So when the language server encounters a completion
            // request, it checks whether completions are being requested for `foo:`, and returns no
            // completions in that case.)
            trigger_characters: Some(vec![":".to_string(), ".".to_string()]),
            all_commit_characters: None,
            work_done_progress_options: WorkDoneProgressOptions {
                work_done_progress: None,
            },
        }),
        definition_provider: Some(OneOf::Left(symbols::DEFS_AND_REFS_SUPPORT)),
        type_definition_provider: Some(TypeDefinitionProviderCapability::Simple(
            symbols::DEFS_AND_REFS_SUPPORT,
        )),
        references_provider: Some(OneOf::Left(symbols::DEFS_AND_REFS_SUPPORT)),
        ..Default::default()
    })
    .expect("could not serialize server capabilities");

    let client_response: serde_json::Value = context
        .connection
        .initialize(capabilities)
        .expect("could not initialize the connection");

    let initialize_params: lsp_types::InitializeParams =
        serde_json::from_value(client_response).expect("could not deserialize client capabilities");

    let (diag_sender, diag_receiver) = bounded::<Result<BTreeMap<Symbol, Vec<Diagnostic>>>>(0);
    let mut symbolicator_runner = symbols::SymbolicatorRunner::idle();
    if symbols::DEFS_AND_REFS_SUPPORT {
        if let Some(uri) = initialize_params.root_uri {
            symbolicator_runner =
                symbols::SymbolicatorRunner::new(&uri, context.symbols.clone(), diag_sender);
            symbolicator_runner.run();
        }
    };

    let mut missing_manifest_reported = false;
    loop {
        select! {
            recv(diag_receiver) -> message => {
                match message {
                    Ok(result) => {
                        match result {
                            Ok(diags) => {
                                for (k, v) in diags {
                                    let url = Url::from_file_path(Path::new(&k.to_string())).unwrap();
                                    let params = lsp_types::PublishDiagnosticsParams::new(url, v, None);
                                    let notification = Notification::new(lsp_types::notification::PublishDiagnostics::METHOD.to_string(), params);
                                    if let Err(err) = context
                                        .connection
                                        .sender
                                        .send(lsp_server::Message::Notification(notification)) {
                                            eprintln!("could not send diagnostics response: {:?}", err);
                                        };
                                }
                            },
                            Err(err) => {
                                let typ = lsp_types::MessageType::Error;
                                let message = format!("{err}");
                                let missing_manifest = message.starts_with("Unable to find package manifest");
                                if !missing_manifest || !missing_manifest_reported {
                                    // report missing manifest only once to avoid re-generating
                                    // user-visible error in cases when the developer decides to
                                    // keep editing a file that does not belong to a packages
                                    let params = lsp_types::ShowMessageParams { typ, message };
                                    let notification = Notification::new(lsp_types::notification::ShowMessage::METHOD.to_string(), params);
                                    if let Err(err) = context
                                        .connection
                                        .sender
                                        .send(lsp_server::Message::Notification(notification)) {
                                            eprintln!("could not send compiler error response: {:?}", err);
                                        };
                                }
                                if missing_manifest {
                                    missing_manifest_reported = true;
                                }
                            },
                        }
                    },
                    Err(error) => eprintln!("symbolicator message error: {:?}", error),
                }
            },
            recv(context.connection.receiver) -> message => {
                match message {
                    Ok(Message::Request(request)) => on_request(&context, &request),
                    Ok(Message::Response(response)) => on_response(&context, &response),
                    Ok(Message::Notification(notification)) => {
                        match notification.method.as_str() {
                            lsp_types::notification::Exit::METHOD => break,
                            lsp_types::notification::Cancel::METHOD => {
                                // TODO: Currently the server does not implement request cancellation.
                                // It ought to, especially once it begins processing requests that may
                                // take a long time to respond to.
                            }
                            _ => on_notification(&mut context, &symbolicator_runner, &notification),
                        }
                    }
                    Err(error) => eprintln!("IDE message error: {:?}", error),
                }
            }
        };
    }

    io_threads.join().expect("I/O threads could not finish");
    symbolicator_runner.quit();
    eprintln!("Shut down language server '{}'.", exe);
}

fn on_request(context: &Context, request: &Request) {
    match request.method.as_str() {
        lsp_types::request::Completion::METHOD => on_completion_request(context, request),
        lsp_types::request::GotoDefinition::METHOD => {
            symbols::on_go_to_def_request(context, request, &context.symbols.lock().unwrap());
        }
        lsp_types::request::GotoTypeDefinition::METHOD => {
            symbols::on_go_to_type_def_request(context, request, &context.symbols.lock().unwrap());
        }
        lsp_types::request::References::METHOD => {
            symbols::on_references_request(context, request, &context.symbols.lock().unwrap());
        }
        lsp_types::request::HoverRequest::METHOD => {
            symbols::on_hover_request(context, request, &context.symbols.lock().unwrap());
        }
        _ => eprintln!("handle request '{}' from client", request.method),
    }
}

fn on_response(_context: &Context, _response: &Response) {
    eprintln!("handle response from client");
}

fn on_notification(
    context: &mut Context,
    symbolicator_runner: &symbols::SymbolicatorRunner,
    notification: &Notification,
) {
    match notification.method.as_str() {
        lsp_types::notification::DidOpenTextDocument::METHOD
        | lsp_types::notification::DidChangeTextDocument::METHOD
        | lsp_types::notification::DidSaveTextDocument::METHOD
        | lsp_types::notification::DidCloseTextDocument::METHOD => {
            on_text_document_sync_notification(
                &mut context.files,
                symbolicator_runner,
                notification,
            )
        }
        _ => eprintln!("handle notification '{}' from client", notification.method),
    }
}
