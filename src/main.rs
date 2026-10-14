use serde_json::{Value, json};
use ssh_mcp_rs::{Error, Result, core, files, forwards::Forwards, sessions::Sessions, transfers};
use std::io::{self, BufRead, Write};

struct Server {
    initialized: bool,
    tools: Value,
    sessions: Sessions,
    forwards: Forwards,
}

impl Server {
    fn new() -> Result<Self> {
        Ok(Self {
            initialized: false,
            tools: serde_json::from_str(include_str!("tools.json")).expect("embedded tools schema"),
            sessions: Sessions::new()?,
            forwards: Forwards::new(),
        })
    }

    fn tool(&mut self, name: &str, args: &Value) -> Result<Value> {
        match name {
            "ssh_exec" => core::ssh_exec(args),
            "ssh_scp" | "ssh_sync" => transfers::call(name, args),
            "ssh_view" | "ssh_create" | "ssh_edit" | "ssh_grep" | "ssh_glob" => {
                files::call(name, args)
            }
            "ssh_start_session" | "ssh_ensure_session" | "ssh_read_session"
            | "ssh_write_session" | "ssh_stop_session" | "ssh_list_sessions" => {
                self.sessions.call(name, args)
            }
            "ssh_forward" | "ssh_list_forwards" | "ssh_stop_forward" => {
                self.forwards.call(name, args)
            }
            _ => Err(Error::new(
                "unknown_tool",
                format!("Unknown tool '{name}'."),
            )),
        }
    }

    fn dispatch(
        &mut self,
        method: &str,
        params: &Value,
    ) -> std::result::Result<Value, (i64, String)> {
        let empty = json!({});
        let params = if params.is_null() { &empty } else { params };
        if method == "initialize" {
            if !params.is_object() {
                return Err((-32602, "'initialize' params must be an object.".into()));
            }
            let version = match params.get("protocolVersion") {
                None | Some(Value::Null) => "2025-06-18",
                Some(Value::String(v)) if !v.trim().is_empty() => {
                    if v == "2024-11-05" || v == "2025-06-18" {
                        v
                    } else {
                        "2025-06-18"
                    }
                }
                _ => {
                    return Err((
                        -32602,
                        "'protocolVersion' must be a non-empty string when provided.".into(),
                    ));
                }
            };
            self.initialized = true;
            return Ok(json!({
                "protocolVersion": version,
                "capabilities": {"tools": {"listChanged": false}},
                "serverInfo": {"name": "ssh-mcp", "version": env!("CARGO_PKG_VERSION")}
            }));
        }
        if method == "notifications/initialized" || method == "ping" {
            return Ok(empty);
        }
        if !self.initialized {
            return Err((
                -32002,
                "Server has not been initialized yet. Send 'initialize' first.".into(),
            ));
        }
        match method {
            "tools/list" => {
                if !params.is_object()
                    || params
                        .get("cursor")
                        .is_some_and(|v| !v.is_null() && !v.is_string())
                {
                    return Err((-32602, "Invalid 'tools/list' params or cursor.".into()));
                }
                Ok(json!({"tools": &self.tools}))
            }
            "tools/call" => {
                let Some(name) = params
                    .get("name")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                else {
                    return Err((
                        -32602,
                        "'tools/call' requires a non-empty string 'name'.".into(),
                    ));
                };
                let args = params
                    .get("arguments")
                    .filter(|v| !v.is_null())
                    .unwrap_or(&empty);
                if !args.is_object() {
                    return Err((
                        -32602,
                        "'tools/call.arguments' must be an object when provided.".into(),
                    ));
                }
                Ok(match self.tool(name, args) {
                    Ok(payload) => json!({
                        "content": [{"type": "text", "text": serde_json::to_string_pretty(&payload).expect("JSON value")}],
                        "structuredContent": payload,
                        "isError": false
                    }),
                    Err(error) => json!({
                        "content": [{"type": "text", "text": &error.message}],
                        "structuredContent": {"ok": false, "error_type": error.kind, "message": &error.message},
                        "isError": true
                    }),
                })
            }
            _ => Err((-32601, format!("Unknown method '{method}'."))),
        }
    }

    fn handle(&mut self, message: Value) -> Option<Value> {
        let id = message.get("id").cloned().unwrap_or(Value::Null);
        if !message.is_object() {
            return Some(rpc_error(id, -32600, "JSON-RPC messages must be objects."));
        }
        if message.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
            return Some(rpc_error(id, -32600, "Only JSON-RPC 2.0 is supported."));
        }
        let Some(method) = message
            .get("method")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        else {
            return Some(rpc_error(
                id,
                -32600,
                "Request method must be a non-empty string.",
            ));
        };
        let result = self.dispatch(method, message.get("params").unwrap_or(&Value::Null));
        message.get("id")?;
        Some(match result {
            Ok(value) => json!({"jsonrpc": "2.0", "id": id, "result": value}),
            Err((code, message)) => rpc_error(id, code, &message),
        })
    }
}

fn rpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

fn serve() -> Result<()> {
    let mut server = Server::new()?;
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut output = stdout.lock();
    let mut input = stdin.lock();
    let mut line = Vec::new();
    loop {
        line.clear();
        if input.read_until(b'\n', &mut line)? == 0 {
            break;
        }
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let response = match serde_json::from_slice::<Value>(&line) {
            Ok(message) => server.handle(message),
            Err(error) => Some(rpc_error(
                Value::Null,
                -32700,
                &format!("Invalid JSON: {error}"),
            )),
        };
        if let Some(response) = response {
            serde_json::to_writer(&mut output, &response)
                .map_err(|e| Error::internal(e.to_string()))?;
            output.write_all(b"\n")?;
            output.flush()?;
        }
    }
    Ok(())
}

fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("--version") | Some("-V") => println!("ssh-mcp-rs {}", env!("CARGO_PKG_VERSION")),
        Some("--help") | Some("-h") => println!(
            "ssh-mcp-rs\nOpenSSH MCP server over newline-delimited JSON-RPC on stdin/stdout.\nEnvironment: SSH_MCP_SSH_BIN, SSH_MCP_SCP_BIN, SSH_MCP_RSYNC_BIN, SSH_MCP_TMUX_BIN, SSH_MCP_STATE_DIR."
        ),
        Some(arg) => {
            eprintln!("Unknown argument: {arg}");
            std::process::exit(2);
        }
        None => {
            if let Err(error) = serve() {
                eprintln!("{error}");
                std::process::exit(1);
            }
        }
    }
}
