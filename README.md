# ssh-mcp-rs

A native Rust rewrite of [slepp/ssh-mcp](https://github.com/slepp/ssh-mcp), backed by your local OpenSSH client. It implements the same 17 MCP tools, input schemas, structured results, and stdio JSON-RPC interface. No Python interpreter is required to run the server.

## Install

```sh
cargo install --git https://github.com/Alarak-Alex/ssh-mcp-rs --locked
```

Or build this checkout:

```sh
cargo build --release --locked
```

Local requirements: macOS or Linux, OpenSSH (`ssh`, `scp`), and `rsync` for synchronization. `tmux` is optional for watching interactive sessions. Remote file and shell tools require a POSIX remote shell plus `find` and `grep`.

## MCP configuration

Use the absolute executable path if your client does not inherit your shell's PATH:

```json
{
  "mcpServers": {
    "ssh-mcp": {
      "type": "stdio",
      "command": "ssh-mcp-rs",
      "args": []
    }
  }
}
```

The server uses your existing `~/.ssh/config`, host aliases, keys, jump hosts and SSH agent. Verify `ssh my-server` independently, including host fingerprints, before giving the agent access. Password-based authentication can use an existing `SSH_ASKPASS` helper; preserve your client's `SSH_ASKPASS`, `SSH_ASKPASS_REQUIRE`, `DISPLAY`, and `SSH_AUTH_SOCK` environment settings when switching from the Python server. Passwords are not stored by this server.

The process waits for newline-delimited JSON-RPC requests on stdin. It does not show an interactive prompt. Supported MCP protocol versions: `2025-06-18` and `2024-11-05`.

## Tools

| Area | Tools |
| --- | --- |
| Commands | `ssh_exec` |
| Transfers | `ssh_scp`, `ssh_sync` |
| Remote files | `ssh_view`, `ssh_create`, `ssh_edit`, `ssh_grep`, `ssh_glob` |
| Interactive sessions | `ssh_start_session`, `ssh_ensure_session`, `ssh_read_session`, `ssh_write_session`, `ssh_stop_session`, `ssh_list_sessions` |
| Port forwarding | `ssh_forward`, `ssh_list_forwards`, `ssh_stop_forward` |

A command invocation takes arguments such as:

```json
{"target":"my-server","command":"pwd && uname -s","timeout":10}
```

For multi-step work, use `ssh_ensure_session` with a descriptive `session_name`. Commands sent through `ssh_write_session` need a trailing newline. Working directory and exported variables remain in that shell. Use `observer_mode: "transcript"` to avoid tmux, or `"tmux"` for an optional detached observer. Reading output drains the session's unread buffer; the transcript keeps the complete stream.

Session names survive conversation resets only while the same MCP server process remains alive. They do not survive server restarts. Close sessions and forwards when finished; normal stdin EOF also cleans up managed processes. Hard process termination cannot guarantee cleanup.

## Environment

| Variable | Default |
| --- | --- |
| `SSH_MCP_SSH_BIN` | `ssh` |
| `SSH_MCP_SCP_BIN` | `scp` |
| `SSH_MCP_RSYNC_BIN` | `rsync` |
| `SSH_MCP_TMUX_BIN` | `tmux` |
| `SSH_MCP_STATE_DIR` | `~/.local/state/ssh-mcp` |

The process inherits SSH authentication environment variables. It does not read Termius data or require a separate host database.

## Safety and limitations

- This is a single-user local tool, not a multi-tenant access gateway. `ssh_exec` can execute arbitrary remote commands with your account's permissions. Host allowlisting and destructive-command approval belong in the MCP client and SSH account permissions.
- Tool arguments reject local command-execution SSH options and forwarding flag injection. Port forwards use the explicit forwarding tools. Existing trusted SSH configuration can still contain proxy commands and forwarding settings.
- Forwarding binds to loopback by default. Exposing other interfaces requires explicit arguments.
- Transcript directories/files use private permissions. Transcripts can contain secrets and grow without bound; manage their retention yourself.
- The in-memory unread session buffer is bounded; dropped output is reported, while the transcript retains it.
- One-off commands open separate SSH connections unless your SSH configuration enables `ControlMaster`. Persistent sessions avoid repeated connection setup.
- PTY output includes command echo, terminal formatting and line wrapping; stdout and stderr are merged. Non-PTY capture preserves CRLF bytes in decoded output.
- Remote file tools are UTF-8 text tools, not binary transfer tools. Use scp/rsync for binary files. Exact edits fail before writing if a replacement is ambiguous or missing, but an external concurrent writer can still race the read/write round trips.
- SCP uses the SFTP transport default in contemporary OpenSSH (9.0+). Remote paths are passed literally, so spaces and apostrophes work without accidental extra quoting. Legacy shell-based SCP clients are not the tested transport; rsync still uses separately shell-quoted remote paths.
- Glob/grep enumerate files under the selected remote path; narrow that path for large trees. Grep regex support depends on the remote grep implementation.
- Numeric inputs must be finite and valid; unsafe target/option injection is rejected. Some invalid inputs accepted accidentally by the Python implementation are deliberately refused.
- Windows local execution is unsupported. macOS has been exercised with real SSH; Linux support is covered by the same POSIX implementation and CI checks, not a Windows compatibility layer.

## Checks

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
cargo build
python3 tests/e2e.py target/debug/ssh-mcp-rs
```

Python is used only by the optional integration test driver, not by the server. The driver starts an isolated real localhost sshd with ephemeral keys, exercises all 17 tools, and removes its temporary data. It verifies command exit/error/timeout handling, CRLF-safe file editing and failed-edit preservation, binary upload/download, rsync exclusions, persistent shell state, and actual HTTP traffic through local and remote forwards. It does not read saved SSH credentials or contact external servers. The test host must permit an sshd process under the current user; Linux installations may need the system sshd privilege-separation directory prepared.

## Provenance and license

Based on upstream revision `7aec8545080145ad115d74c2ddb9d798cbdcc9e2`. Tool descriptions/input schemas and remote POSIX scripts are derived from the upstream MIT-licensed implementation. The MCP server, subprocess transport, PTY/session lifecycle, file operations, transfer handling and forwarding manager are implemented in Rust. The original MIT copyright/license notice is retained in [LICENSE](LICENSE).
