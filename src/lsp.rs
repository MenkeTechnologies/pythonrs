//! Language Server Protocol server (`python --lsp`) over stdio.
//!
//! A minimal but real server: it completes the initialize/shutdown handshake and
//! answers `textDocument/completion` with the Python builtin/keyword corpus and
//! `textDocument/hover` with a short description. Diagnostics, go-to-definition
//! and signature help are a later wave (see BUGS.md).

use lsp_server::{Connection, Message, Response};
use lsp_types::{
    CompletionItem, CompletionItemKind, CompletionOptions, ServerCapabilities,
    TextDocumentSyncKind,
};
use serde_json::json;

/// Run the LSP server, blocking until the client disconnects.
pub fn run() -> Result<(), String> {
    let (connection, io_threads) = Connection::stdio();
    let capabilities = ServerCapabilities {
        text_document_sync: Some(lsp_types::TextDocumentSyncCapability::Kind(
            TextDocumentSyncKind::FULL,
        )),
        completion_provider: Some(CompletionOptions::default()),
        hover_provider: Some(lsp_types::HoverProviderCapability::Simple(true)),
        ..Default::default()
    };
    let init_params = connection
        .initialize(serde_json::to_value(capabilities).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    let _ = init_params;

    for msg in &connection.receiver {
        match msg {
            Message::Request(req) => {
                if connection.handle_shutdown(&req).unwrap_or(false) {
                    break;
                }
                let resp = match req.method.as_str() {
                    "textDocument/completion" => Response {
                        id: req.id,
                        result: Some(json!(completions())),
                        error: None,
                    },
                    "textDocument/hover" => Response {
                        id: req.id,
                        result: Some(json!({ "contents": "pythonrs — Python on fusevm" })),
                        error: None,
                    },
                    _ => Response {
                        id: req.id,
                        result: Some(json!(null)),
                        error: None,
                    },
                };
                let _ = connection.sender.send(Message::Response(resp));
            }
            Message::Response(_) | Message::Notification(_) => {}
        }
    }
    io_threads.join().map_err(|e| e.to_string())?;
    Ok(())
}

fn completions() -> Vec<CompletionItem> {
    const NAMES: &[&str] = &[
        "print", "len", "range", "int", "str", "float", "bool", "list", "tuple", "dict", "set",
        "sum", "min", "max", "sorted", "enumerate", "zip", "map", "filter", "any", "all", "abs",
        "round", "type", "isinstance", "input", "open", "def", "class", "return", "import", "from",
        "if", "elif", "else", "while", "for", "in", "try", "except", "finally", "with", "lambda",
        "None", "True", "False", "and", "or", "not", "is", "yield", "raise", "assert",
    ];
    NAMES
        .iter()
        .map(|n| CompletionItem {
            label: (*n).to_string(),
            kind: Some(CompletionItemKind::KEYWORD),
            ..Default::default()
        })
        .collect()
}
