use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

#[test]
fn protocol_errors_and_tool_boundaries() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_ssh-mcp-rs"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut output = BufReader::new(child.stdout.take().unwrap());
    let mut request = |raw: &str| -> Value {
        writeln!(input, "{raw}").unwrap();
        input.flush().unwrap();
        let mut line = String::new();
        assert!(output.read_line(&mut line).unwrap() > 0);
        serde_json::from_str(&line).unwrap()
    };
    assert_eq!(request("not-json")["error"]["code"], -32700);
    assert_eq!(
        request(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#)["error"]["code"],
        -32002
    );
    let init = request(&json!({"jsonrpc":"2.0","id":2,"method":"initialize","params":{"protocolVersion":"2024-11-05"}}).to_string());
    assert_eq!(init["result"]["protocolVersion"], "2024-11-05");
    // A notification must not produce a response; the next request owns the next line.
    let ping = request(&format!(
        "{}\n{}",
        json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
        json!({"jsonrpc":"2.0","id":3,"method":"ping"})
    ));
    assert_eq!(ping["id"], 3);
    let invalid_args = request(&json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"ssh_exec","arguments":[]}}).to_string());
    assert_eq!(invalid_args["error"]["code"], -32602);
    let unknown = request(
        &json!({"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"missing"}})
            .to_string(),
    );
    assert_eq!(unknown["result"]["isError"], true);
    assert_eq!(
        unknown["result"]["structuredContent"]["error_type"],
        "unknown_tool"
    );
    let invalid_target = request(&json!({"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"ssh_exec","arguments":{"target":"-oProxyCommand=touch /tmp/not-authorized","command":"true"}}}).to_string());
    assert_eq!(invalid_target["result"]["isError"], true);
    assert_eq!(
        invalid_target["result"]["structuredContent"]["error_type"],
        "validation_error"
    );
    drop(input);
    assert!(child.wait().unwrap().success());
}
