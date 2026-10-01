//! The Debug Adapter Protocol server (`python --dap`), driven end to end over
//! its stdio the way an editor drives it.

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{ChildStdin, ChildStdout, Command, Stdio};

use serde_json::{json, Value};

struct Adapter {
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    seq: i64,
}

impl Adapter {
    fn send(&mut self, command: &str, arguments: Value) -> i64 {
        self.seq += 1;
        let body = json!({
            "seq": self.seq,
            "type": "request",
            "command": command,
            "arguments": arguments,
        })
        .to_string();
        write!(self.stdin, "Content-Length: {}\r\n\r\n{body}", body.len()).expect("write request");
        self.stdin.flush().expect("flush request");
        self.seq
    }

    fn read(&mut self) -> Value {
        let mut len = 0usize;
        loop {
            let mut line = String::new();
            assert!(
                self.stdout.read_line(&mut line).expect("read header") > 0,
                "adapter closed"
            );
            let line = line.trim_end();
            if line.is_empty() {
                break;
            }
            if let Some(n) = line.strip_prefix("Content-Length: ") {
                len = n.parse().expect("content length");
            }
        }
        let mut body = vec![0u8; len];
        self.stdout.read_exact(&mut body).expect("read body");
        serde_json::from_slice(&body).expect("json body")
    }

    /// Read until the response to request `seq`, collecting what came before.
    fn response(&mut self, seq: i64, seen: &mut Vec<Value>) -> Value {
        loop {
            let msg = self.read();
            if msg["type"] == "response" && msg["request_seq"] == seq {
                return msg;
            }
            seen.push(msg);
        }
    }

    fn until_event(&mut self, event: &str, seen: &mut Vec<Value>) {
        loop {
            let msg = self.read();
            let hit = msg["type"] == "event" && msg["event"] == event;
            seen.push(msg);
            if hit {
                return;
            }
        }
    }

    fn evaluate(&mut self, expression: &str, seen: &mut Vec<Value>) -> Value {
        let seq = self.send(
            "evaluate",
            json!({ "expression": expression, "frameId": 0, "context": "watch" }),
        );
        self.response(seq, seen)
    }
}

/// A watch expression is any expression, evaluated in the stopped frame: its
/// locals, the module's globals, a user `__repr__`, a call into a function that
/// itself carries the breakpoint (which must not stop again), and a raise that
/// comes back as a failed request without disturbing the program, which then
/// runs on to print what it would have printed anyway.
#[test]
fn evaluate_runs_watch_expressions_in_the_paused_frame() {
    let dir = std::env::temp_dir().join(format!("pythonrs-dap-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    let program = dir.join("prog.py");
    std::fs::write(
        &program,
        "def f(n):\n\
        \x20   total = n * 2\n\
        \x20   return total\n\
         class R:\n\
        \x20   def __repr__(self): return 'R!'\n\
         r = R()\n\
         acc = 0\n\
         for i in range(3):\n\
        \x20   acc += i\n\
         print(f(20), acc)\n",
    )
    .expect("write program");
    let program = program.to_str().expect("utf-8 path").to_string();

    let mut child = Command::new(env!("CARGO_BIN_EXE_python"))
        .arg("--dap")
        .env("HOME", &dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn python --dap");
    let mut dap = Adapter {
        stdin: child.stdin.take().expect("stdin"),
        stdout: BufReader::new(child.stdout.take().expect("stdout")),
        seq: 0,
    };
    let mut seen = Vec::new();

    let seq = dap.send("initialize", json!({ "adapterID": "pythonrs" }));
    dap.response(seq, &mut seen);
    let seq = dap.send(
        "setBreakpoints",
        json!({ "source": { "path": program }, "breakpoints": [{ "line": 3 }] }),
    );
    let bps = dap.response(seq, &mut seen);
    assert_eq!(bps["body"]["breakpoints"][0]["verified"], true);
    dap.send("launch", json!({ "program": program }));
    dap.until_event("stopped", &mut seen);

    let ok = |r: &Value| {
        assert_eq!(r["success"], true, "{r}");
        r["body"]["result"].as_str().expect("result").to_string()
    };
    // Every local of the stopped frame is visible by name, the ones assigned
    // after entry included.
    let seq = dap.send("variables", json!({ "variablesReference": 1 }));
    let vars = dap.response(seq, &mut seen);
    let mut names: Vec<(String, String)> = vars["body"]["variables"]
        .as_array()
        .expect("variables")
        .iter()
        .map(|v| {
            (
                v["name"].as_str().unwrap_or("").to_string(),
                v["value"].as_str().unwrap_or("").to_string(),
            )
        })
        .collect();
    names.sort();
    assert_eq!(
        names,
        [
            ("n".to_string(), "20".to_string()),
            ("total".to_string(), "40".to_string())
        ]
    );
    assert_eq!(ok(&dap.evaluate("total + n", &mut seen)), "60");
    assert_eq!(
        ok(&dap.evaluate("[k * n for k in range(3)]", &mut seen)),
        "[0, 20, 40]"
    );
    assert_eq!(ok(&dap.evaluate("r", &mut seen)), "R!");
    assert_eq!(ok(&dap.evaluate("f(1)", &mut seen)), "2");
    let failed = dap.evaluate("nope + 1", &mut seen);
    assert_eq!(failed["success"], false, "{failed}");
    assert_eq!(failed["message"], "NameError: name 'nope' is not defined");
    // The bare-name path still answers after a failed evaluation.
    assert_eq!(ok(&dap.evaluate("n", &mut seen)), "20");

    seen.clear();
    dap.send("continue", json!({ "threadId": 1 }));
    dap.until_event("terminated", &mut seen);
    let output: String = seen
        .iter()
        .filter(|m| m["event"] == "output")
        .filter_map(|m| m["body"]["output"].as_str())
        .collect();
    assert_eq!(output, "40 3\n");
    let stops = seen.iter().filter(|m| m["event"] == "stopped").count();
    assert_eq!(
        stops, 0,
        "the evaluated call to f() must not leave a stop behind"
    );

    let seq = dap.send("disconnect", json!({}));
    dap.response(seq, &mut seen);
    drop(dap);
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&dir);
}
