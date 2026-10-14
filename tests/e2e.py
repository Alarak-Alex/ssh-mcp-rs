#!/usr/bin/env python3
"""Exercise all tools against an isolated real localhost sshd (no saved credentials)."""
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import sys
import tempfile
import time
import urllib.request


def free_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def run(binary):
    for executable in ("sshd", "ssh", "ssh-keygen", "scp", "rsync"):
        if not shutil.which(executable) and not Path("/usr/sbin", executable).exists():
            raise SystemExit(f"Missing integration prerequisite: {executable}")
    with tempfile.TemporaryDirectory(prefix="ssh-mcp-rs-e2e-") as directory:
        root = Path(directory)
        key = root / "key"
        subprocess.run(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-f", str(key)], check=True)
        port = free_port()
        (root / "known_hosts").write_text(f"[127.0.0.1]:{port} {key.with_suffix('.pub').read_text()}")
        username = subprocess.check_output(["id", "-un"], text=True).strip()
        config = root / "sshd_config"
        config.write_text(f"Port {port}\nListenAddress 127.0.0.1\nHostKey {key}\nPidFile {root}/sshd.pid\nAuthorizedKeysFile {key}.pub\nStrictModes no\nPasswordAuthentication no\nKbdInteractiveAuthentication no\nUsePAM no\nAllowUsers {username}\nSubsystem sftp internal-sftp\n")
        with (root / "sshd.log").open("w+") as log:
            daemon = subprocess.Popen([shutil.which("sshd") or "/usr/sbin/sshd", "-D", "-e", "-f", str(config)], stdout=log, stderr=log)
            server = None
            try:
                deadline = time.monotonic() + 5
                while True:
                    if daemon.poll() is not None:
                        log.seek(0)
                        raise AssertionError("sshd failed: " + log.read())
                    try:
                        with socket.create_connection(("127.0.0.1", port), timeout=.2):
                            break
                    except OSError:
                        if time.monotonic() >= deadline:
                            raise AssertionError("local sshd did not start")
                        time.sleep(.05)
                env = dict(os.environ, SSH_MCP_STATE_DIR=str(root / "state"))
                server = subprocess.Popen([str(binary)], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, env=env)
                request_id = 0
                exercised = set()
                common = {"target": f"{username}@127.0.0.1", "port": port, "identity_file": str(key), "known_hosts_file": str(root / "known_hosts"), "strict_host_key_checking": "yes", "extra_ssh_args": ["-o", "IdentitiesOnly=yes", "-o", "ConnectTimeout=5"]}

                def rpc(method, params):
                    nonlocal request_id
                    request_id += 1
                    server.stdin.write(json.dumps({"jsonrpc": "2.0", "id": request_id, "method": method, "params": params}) + "\n")
                    server.stdin.flush()
                    line = server.stdout.readline()
                    assert line, "MCP closed stdout unexpectedly"
                    response = json.loads(line)
                    assert response["id"] == request_id
                    assert "error" not in response, response
                    return response["result"]

                def call(name, args=None, *, connected=True, error=False):
                    exercised.add(name)
                    args = {**(common if connected else {}), **(args or {})}
                    response = rpc("tools/call", {"name": name, "arguments": args})
                    assert response["isError"] == error, response
                    return response["structuredContent"]

                rpc("initialize", {"protocolVersion": "2025-06-18"})
                names = {tool["name"] for tool in rpc("tools/list", {})["tools"]}
                execution = call("ssh_exec", {"command": "printf 'out\\n'; printf 'err' >&2; exit 7"})
                assert execution["stdout"] == "out\n" and execution["stderr"] == "err" and execution["exit_code"] == 7, execution
                timeout = call("ssh_exec", {"command": "sleep 5", "timeout": .15})
                assert timeout["timed_out"] and not timeout["ok"], timeout
                rejection = call("ssh_exec", {"command": "true", "extra_ssh_args": ["-oProxyCommand=touch /tmp/unauthorized"]}, error=True)
                assert rejection["error_type"] == "validation_error"
                remote = root / "remote"
                remote.mkdir()
                path = str(remote / "file.txt")
                call("ssh_create", {"path": path, "content": "first\r\nsecond\r\n"})
                assert Path(path).read_bytes() == b"first\r\nsecond\r\n"
                call("ssh_create", {"path": path, "content": "overwrite"}, error=True)
                view = call("ssh_view", {"path": path, "view_range": [2, 2]})
                assert "second" in view["content"] and "first" not in view["content"], view
                call("ssh_edit", {"path": path, "edits": [{"old_str": "second", "new_str": "changed"}]})
                assert Path(path).read_bytes() == b"first\r\nchanged\r\n"
                call("ssh_edit", {"path": path, "edits": [{"old_str": "first", "new_str": "uncommitted"}, {"old_str": "missing", "new_str": "bad"}]}, error=True)
                assert Path(path).read_bytes() == b"first\r\nchanged\r\n"
                glob = call("ssh_glob", {"path": str(remote), "pattern": "**/*.txt"})
                assert "file.txt" in json.dumps(glob), glob
                grep = call("ssh_grep", {"path": str(remote), "pattern": "changed", "output_mode": "content"})
                assert "changed" in json.dumps(grep), grep
                upload = root / "upload space's.txt"
                upload.write_bytes(b"transfer\x00bytes\n")
                copied = str(remote / "copy space's.txt")
                scp_upload = call("ssh_scp", {"direction": "upload", "sources": [str(upload)], "destination": copied})
                assert scp_upload["ok"], scp_upload
                download = root / "download.txt"
                assert call("ssh_scp", {"direction": "download", "sources": [copied], "destination": str(download)})["ok"]
                assert download.read_bytes() == upload.read_bytes()
                source = root / "sync-source"
                source.mkdir()
                (source / "keep").write_text("synced")
                (source / "skip.log").write_text("excluded")
                destination = remote / "synced"
                assert call("ssh_sync", {"direction": "upload", "source": str(source) + "/", "destination": str(destination), "exclude": ["*.log"]})["ok"]
                assert (destination / "keep").read_text() == "synced" and not (destination / "skip.log").exists()
                session_args = {"session_name": "e2e-state", "observer_mode": "transcript", "shell": "/bin/sh", "wait_seconds": .1}
                session = call("ssh_start_session", session_args)
                sid = session["session_id"]
                reused = call("ssh_ensure_session", session_args)
                assert reused["session_id"] == sid and reused["reused"]
                call("ssh_write_session", {"session_id": sid, "input": f"cd '{remote}'; export MCP_CHECK=preserved\n", "wait_seconds": .1}, connected=False)
                output = call("ssh_write_session", {"session_id": sid, "input": "printf '\\nstate:%s:%s\\n' \"$MCP_CHECK\" \"$PWD\"\n", "wait_seconds": .3}, connected=False)["output"]
                output += call("ssh_read_session", {"session_id": sid, "wait_seconds": .2}, connected=False)["output"]
                assert f"state:preserved:{remote}" in output.splitlines(), output
                listing = call("ssh_list_sessions", {"session_name": "e2e-state"}, connected=False)
                assert sid in json.dumps(listing)
                call("ssh_stop_session", {"session_id": sid, "wait_seconds": .2}, connected=False)
                # Exercise real forwarded HTTP bytes, not just process metadata.
                import http.server
                import threading
                listener = http.server.ThreadingHTTPServer(("127.0.0.1", 0), http.server.SimpleHTTPRequestHandler)
                worker = threading.Thread(target=listener.serve_forever, daemon=True)
                worker.start()
                try:
                    for direction in ("local", "remote"):
                        forwarded_port = free_port()
                        forward = call("ssh_forward", {"direction": direction, "local_port": forwarded_port, "remote_host": "127.0.0.1", "remote_port": listener.server_port})
                        assert forward["running"], forward
                        with urllib.request.urlopen(f"http://127.0.0.1:{forwarded_port}/", timeout=5) as response:
                            assert response.status == 200
                        listing = call("ssh_list_forwards", {}, connected=False)
                        assert forward["forward_id"] in json.dumps(listing)
                        call("ssh_stop_forward", {"forward_id": forward["forward_id"]}, connected=False)
                        with socket.socket() as sock:
                            sock.settimeout(1)
                            assert sock.connect_ex(("127.0.0.1", forwarded_port)) != 0
                finally:
                    listener.shutdown()
                    listener.server_close()
                assert exercised == names, (exercised, names)
                server.stdin.close()
                assert server.wait(timeout=10) == 0
                print(f"PASS: {len(exercised)} tools; real SSH, binary transfers, CRLF-safe edits, persistent PTY state, local/remote forwarding")
            finally:
                if server is not None and server.poll() is None:
                    server.terminate()
                    try:
                        server.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        server.kill()
                        server.wait()
                daemon.terminate()
                daemon.wait(timeout=5)


if __name__ == "__main__":
    run(Path(sys.argv[1]).resolve() if len(sys.argv) > 1 else Path("target/debug/ssh-mcp-rs").resolve())
