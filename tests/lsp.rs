//! The Language Server Protocol server (`python --lsp`), driven over its stdio
//! the way an editor drives it.

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{ChildStdin, ChildStdout, Command, Stdio};

use serde_json::{json, Value};

struct Server {
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    id: i64,
}

impl Server {
    fn write(&mut self, msg: Value) {
        let body = msg.to_string();
        write!(self.stdin, "Content-Length: {}\r\n\r\n{body}", body.len()).expect("write");
        self.stdin.flush().expect("flush");
    }

    fn notify(&mut self, method: &str, params: Value) {
        self.write(json!({ "jsonrpc": "2.0", "method": method, "params": params }));
    }

    /// Send a request and return its result, skipping notifications.
    fn request(&mut self, method: &str, params: Value) -> Value {
        self.id += 1;
        let id = self.id;
        self.write(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        loop {
            let msg = self.read();
            if msg["id"] == id {
                assert!(msg.get("error").is_none(), "{msg}");
                return msg["result"].clone();
            }
        }
    }

    fn read(&mut self) -> Value {
        let mut len = 0usize;
        loop {
            let mut line = String::new();
            assert!(
                self.stdout.read_line(&mut line).expect("header") > 0,
                "server closed"
            );
            let line = line.trim_end();
            if line.is_empty() {
                break;
            }
            if let Some(n) = line.strip_prefix("Content-Length: ") {
                len = n.parse().expect("length");
            }
        }
        let mut body = vec![0u8; len];
        self.stdout.read_exact(&mut body).expect("body");
        serde_json::from_slice(&body).expect("json")
    }
}

/// Go-to-definition and signature help are advertised and answer from the
/// open document: a call to a function defined further down jumps to its
/// `def`, and an open call shows that function's parameters as written with
/// the one being typed active.
#[test]
fn definition_and_signature_help_answer_from_the_open_document() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_python"))
        .arg("--lsp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn python --lsp");
    let mut lsp = Server {
        stdin: child.stdin.take().expect("stdin"),
        stdout: BufReader::new(child.stdout.take().expect("stdout")),
        id: 0,
    };

    let init = lsp.request("initialize", json!({ "capabilities": {} }));
    assert_eq!(init["capabilities"]["definitionProvider"], true);
    assert_eq!(
        init["capabilities"]["signatureHelpProvider"]["triggerCharacters"],
        json!(["(", ","])
    );
    lsp.notify("initialized", json!({}));

    let uri = "file:///tmp/nav.py";
    let text = "def main():\n    return scale(2, \n\ndef scale(value, factor=10):\n    return value * factor\n";
    lsp.notify(
        "textDocument/didOpen",
        json!({ "textDocument": { "uri": uri, "languageId": "python", "version": 1, "text": text } }),
    );

    let def = lsp.request(
        "textDocument/definition",
        json!({ "textDocument": { "uri": uri }, "position": { "line": 1, "character": 12 } }),
    );
    assert_eq!(def["uri"], uri);
    assert_eq!(
        def["range"],
        json!({ "start": { "line": 3, "character": 4 }, "end": { "line": 3, "character": 9 } })
    );

    let sig = lsp.request(
        "textDocument/signatureHelp",
        json!({ "textDocument": { "uri": uri }, "position": { "line": 1, "character": 20 } }),
    );
    assert_eq!(sig["signatures"][0]["label"], "scale(value, factor=10)");
    assert_eq!(
        sig["signatures"][0]["parameters"],
        json!([{ "label": "value" }, { "label": "factor=10" }])
    );
    assert_eq!(sig["activeParameter"], 1);

    lsp.request("shutdown", Value::Null);
    lsp.notify("exit", Value::Null);
    drop(lsp);
    let _ = child.wait();
}

/// Definition follows an import into the module beside the document, and the
/// answer names that file by its `file:` URI — a directory with a space in it
/// round-trips through the percent-escapes both ways. Signature help reads the
/// imported definition.
#[test]
fn definition_and_signature_help_follow_imports_to_another_file() {
    let dir = std::env::temp_dir().join(format!("pythonrs lsp {}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    std::fs::write(
        dir.join("helpers.py"),
        "def scale(value, factor=10):\n    return value * factor\n",
    )
    .expect("write module");
    let doc = dir.join("main.py");
    let uri = format!("file://{}", doc.to_str().unwrap().replace(' ', "%20"));
    let helpers_uri = format!(
        "file://{}",
        dir.join("helpers.py").to_str().unwrap().replace(' ', "%20")
    );

    let mut child = Command::new(env!("CARGO_BIN_EXE_python"))
        .arg("--lsp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn python --lsp");
    let mut lsp = Server {
        stdin: child.stdin.take().expect("stdin"),
        stdout: BufReader::new(child.stdout.take().expect("stdout")),
        id: 0,
    };
    lsp.request("initialize", json!({ "capabilities": {} }));
    lsp.notify("initialized", json!({}));
    let text = "from helpers import scale\nscale(1, \n";
    lsp.notify(
        "textDocument/didOpen",
        json!({ "textDocument": { "uri": uri, "languageId": "python", "version": 1, "text": text } }),
    );

    let def = lsp.request(
        "textDocument/definition",
        json!({ "textDocument": { "uri": uri }, "position": { "line": 1, "character": 2 } }),
    );
    assert_eq!(def["uri"], helpers_uri);
    assert_eq!(
        def["range"],
        json!({ "start": { "line": 0, "character": 4 }, "end": { "line": 0, "character": 9 } })
    );
    let sig = lsp.request(
        "textDocument/signatureHelp",
        json!({ "textDocument": { "uri": uri }, "position": { "line": 1, "character": 9 } }),
    );
    assert_eq!(sig["signatures"][0]["label"], "scale(value, factor=10)");
    assert_eq!(sig["activeParameter"], 1);

    lsp.request("shutdown", Value::Null);
    lsp.notify("exit", Value::Null);
    drop(lsp);
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&dir);
}
