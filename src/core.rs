// Adapted from slepp/ssh-mcp (MIT); see LICENSE for original attribution.
use crate::{Error, Result};
use serde_json::{Value, json};
use std::ffi::CString;
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn value<'a>(args: &'a Value, key: &str) -> Result<Option<&'a Value>> {
    let map = args
        .as_object()
        .ok_or_else(|| Error::validation("Tool arguments must be a JSON object."))?;
    Ok(map.get(key).filter(|v| !v.is_null()))
}
pub fn raw_str<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    let s = value(args, key)?
        .ok_or_else(|| Error::validation(format!("'{key}' is required.")))?
        .as_str()
        .ok_or_else(|| Error::validation(format!("'{key}' must be a string.")))?;
    if s.contains('\0') || s.trim().is_empty() {
        return Err(Error::validation(format!(
            "'{key}' must be non-empty and cannot contain NUL bytes."
        )));
    }
    Ok(s)
}
pub fn req_str<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    Ok(raw_str(args, key)?.trim())
}
pub fn opt_str<'a>(args: &'a Value, key: &str) -> Result<Option<&'a str>> {
    if value(args, key)?.is_none() {
        Ok(None)
    } else {
        Ok(Some(req_str(args, key)?))
    }
}
pub fn boolean(args: &Value, key: &str, default: bool) -> Result<bool> {
    match value(args, key)? {
        None => Ok(default),
        Some(Value::Bool(b)) => Ok(*b),
        Some(Value::String(s)) => match s.trim().to_ascii_lowercase().as_str() {
            "true" | "yes" | "on" => Ok(true),
            "false" | "no" | "off" => Ok(false),
            _ => Err(Error::validation(format!("'{key}' must be a boolean."))),
        },
        _ => Err(Error::validation(format!("'{key}' must be a boolean."))),
    }
}
pub fn number(args: &Value, key: &str, default: f64) -> Result<f64> {
    let n = match value(args, key)? {
        None => default,
        Some(Value::Number(n)) => n
            .as_f64()
            .ok_or_else(|| Error::validation(format!("'{key}' must be a number.")))?,
        Some(Value::String(s)) => s
            .trim()
            .parse()
            .map_err(|_| Error::validation(format!("'{key}' must be a number.")))?,
        _ => return Err(Error::validation(format!("'{key}' must be a number."))),
    };
    if !n.is_finite() || n < 0.0 {
        return Err(Error::validation(format!(
            "'{key}' must be a finite number >= 0."
        )));
    }
    Ok(n)
}
pub fn integer(args: &Value, key: &str, default: i64) -> Result<i64> {
    let n = match value(args, key)? {
        None => Some(default),
        Some(Value::Number(n)) => n.as_i64().or_else(|| {
            n.as_f64()
                .filter(|n| n.is_finite() && *n >= i64::MIN as f64 && *n < i64::MAX as f64)
                .map(|n| n.trunc() as i64)
        }),
        Some(Value::String(s)) => s.trim().parse().ok(),
        _ => None,
    }
    .ok_or_else(|| Error::validation(format!("'{key}' must be an integer.")))?;
    Ok(n)
}
pub fn strings(args: &Value, key: &str) -> Result<Vec<String>> {
    let Some(v) = value(args, key)? else {
        return Ok(Vec::new());
    };
    v.as_array()
        .ok_or_else(|| Error::validation(format!("'{key}' must be an array of strings.")))?
        .iter()
        .enumerate()
        .map(|(i, v)| {
            let s = v
                .as_str()
                .ok_or_else(|| Error::validation(format!("'{key}[{i}]' must be a string.")))?;
            if s.trim().is_empty() || s.contains('\0') {
                return Err(Error::validation(format!(
                    "'{key}[{i}]' must be non-empty and cannot contain NUL bytes."
                )));
            }
            Ok(s.to_owned())
        })
        .collect()
}
pub fn timeout(args: &Value) -> Result<Option<f64>> {
    if value(args, "timeout")?.is_none() {
        return Ok(None);
    }
    let n = number(args, "timeout", 0.0)?;
    if n <= 0.0 || Duration::try_from_secs_f64(n).is_err() {
        return Err(Error::validation(
            "'timeout' must be a finite positive duration.",
        ));
    }
    Ok(Some(n))
}
pub fn expand(path: &str) -> String {
    if !path.starts_with('~') {
        return path.to_owned();
    }
    let (user, tail) = path[1..]
        .split_once('/')
        .map_or((&path[1..], ""), |(u, t)| {
            (u, &path[path.len() - t.len() - 1..])
        });
    if user.is_empty()
        && let Ok(home) = std::env::var("HOME")
    {
        return format!("{home}{tail}");
    }
    let Ok(name) = CString::new(user) else {
        return path.to_owned();
    };
    let mut storage = vec![0u8; 16384];
    let mut pw = std::mem::MaybeUninit::<libc::passwd>::uninit();
    let mut out = std::ptr::null_mut();
    let rc = unsafe {
        if user.is_empty() {
            libc::getpwuid_r(
                libc::getuid(),
                pw.as_mut_ptr(),
                storage.as_mut_ptr().cast(),
                storage.len(),
                &mut out,
            )
        } else {
            libc::getpwnam_r(
                name.as_ptr(),
                pw.as_mut_ptr(),
                storage.as_mut_ptr().cast(),
                storage.len(),
                &mut out,
            )
        }
    };
    if rc == 0 && !out.is_null() {
        let home = unsafe { std::ffi::CStr::from_ptr((*out).pw_dir) }.to_string_lossy();
        format!("{home}{tail}")
    } else {
        path.to_owned()
    }
}
pub fn quote(s: &str) -> String {
    if !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_@%+=:,./-".contains(&b))
    {
        return s.to_owned();
    }
    format!("'{}'", s.replace('\'', "'\"'\"'"))
}
pub fn remote_command(args: &Value, command: &str, interactive: bool) -> Result<String> {
    if command.contains('\0') {
        return Err(Error::validation("'command' cannot contain NUL bytes."));
    }
    let cwd = opt_str(args, "cwd")?;
    let shell = opt_str(args, "shell")?;
    let mut exports = Vec::new();
    if let Some(env) = value(args, "env")? {
        for (key, v) in env
            .as_object()
            .ok_or_else(|| Error::validation("'env' must map variable names to strings."))?
        {
            let mut b = key.bytes();
            if !b
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic() || c == b'_')
                || !b.all(|c| c.is_ascii_alphanumeric() || c == b'_')
            {
                return Err(Error::validation(
                    "Environment variable names must match ^[A-Za-z_][A-Za-z0-9_]*$.",
                ));
            }
            let s = v.as_str().ok_or_else(|| {
                Error::validation(format!(
                    "Environment variable '{key}' must have a string value."
                ))
            })?;
            if s.contains('\0') {
                return Err(Error::validation(format!(
                    "Environment variable '{key}' cannot contain NUL bytes."
                )));
            }
            exports.push(format!("export {key}={}", quote(s)));
        }
    }
    if cwd.is_none() && shell.is_none() && exports.is_empty() {
        return Ok(if interactive {
            String::new()
        } else {
            command.to_owned()
        });
    }
    let mut body = if interactive {
        shell.map_or_else(
            || "exec \"${SHELL:-/bin/sh}\" -i".to_owned(),
            |s| format!("exec {} -i", quote(s)),
        )
    } else {
        command.to_owned()
    };
    if let Some(cwd) = cwd {
        let cwd = if cwd.starts_with('-') {
            format!("./{cwd}")
        } else {
            cwd.to_owned()
        };
        body = format!("cd {} && {body}", quote(&cwd));
    }
    exports.push(body);
    Ok(format!(
        "exec {} -c {}",
        quote(if interactive {
            "sh"
        } else {
            shell.unwrap_or("sh")
        }),
        quote(&exports.join("; "))
    ))
}
pub fn binary(env_key: &str, default: &str) -> Result<String> {
    let candidate = std::env::var(env_key).unwrap_or_else(|_| default.to_owned());
    let candidate = candidate.trim();
    if candidate.is_empty() || candidate.contains('\0') {
        return Err(Error::validation(format!(
            "{env_key} must name a non-empty executable without NUL bytes."
        )));
    }
    let executable = |p: &Path| {
        p.is_file()
            && p.metadata()
                .is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
    };
    if candidate.contains('/') || candidate.starts_with('.') {
        let p = expand(candidate);
        if executable(Path::new(&p)) {
            return Ok(p);
        }
    } else if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            let p = dir.join(candidate);
            if executable(&p) {
                return Ok(p.to_string_lossy().into_owned());
            }
        }
    }
    Err(Error::validation(format!(
        "Executable '{candidate}' was not found or is not executable."
    )))
}

#[derive(Clone, Debug)]
pub struct Connection {
    pub target: String,
    port: Option<i64>,
    identity_file: Option<String>,
    known_hosts_file: Option<String>,
    strict_host_key_checking: Option<String>,
    extra_ssh_args: Vec<String>,
}
fn check_ssh_args(args: &[String]) -> Result<()> {
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        if !arg.starts_with('-') || arg == "-" || arg.starts_with("--") {
            return Err(Error::validation(
                "extra_ssh_args must contain SSH options, not destinations or commands.",
            ));
        }
        for (offset, flag) in arg[1..].char_indices() {
            if "LRDWFSMOfwI".contains(flag) {
                return Err(Error::validation(format!(
                    "Blocked SSH flag '-{flag}' in extra_ssh_args."
                )));
            }
            if "BbcEIiJlmopQ".contains(flag) {
                let rest = &arg[1 + offset + flag.len_utf8()..];
                let operand = if rest.is_empty() {
                    i += 1;
                    args.get(i)
                        .ok_or_else(|| {
                            Error::validation(format!("SSH flag '-{flag}' requires a value."))
                        })?
                        .as_str()
                } else {
                    rest
                };
                if flag == 'o' {
                    let key = operand
                        .trim_start()
                        .split(['=', ' ', '\t'])
                        .next()
                        .unwrap_or("")
                        .to_ascii_lowercase();
                    if [
                        "proxycommand",
                        "localcommand",
                        "permitlocalcommand",
                        "localforward",
                        "remoteforward",
                        "dynamicforward",
                        "knownhostscommand",
                        "include",
                        "pkcs11provider",
                        "securitykeyprovider",
                        "forkafterauthentication",
                        "controlmaster",
                        "controlpersist",
                    ]
                    .contains(&key.as_str())
                        && !(key == "forkafterauthentication"
                            && operand
                                .trim()
                                .split_once('=')
                                .is_some_and(|(_, value)| value.trim().eq_ignore_ascii_case("no")))
                    {
                        return Err(Error::validation(format!(
                            "Blocked SSH option '{key}' in extra_ssh_args."
                        )));
                    }
                    if key.is_empty() || operand.contains(['\n', '\r']) {
                        return Err(Error::validation("Invalid SSH configuration option."));
                    }
                }
                break;
            }
            if !"46AaCfGgKkNnqsTtVvXxYy".contains(flag) {
                return Err(Error::validation(format!(
                    "Unsupported SSH flag '-{flag}' in extra_ssh_args."
                )));
            }
        }
        i += 1;
    }
    Ok(())
}
impl Connection {
    pub fn from_args(args: &Value) -> Result<Self> {
        let target = req_str(args, "target")?;
        if target.starts_with('-') || target.bytes().any(|b| b.is_ascii_whitespace()) {
            return Err(Error::validation(
                "'target' cannot begin with '-' or contain whitespace.",
            ));
        }
        let port = if value(args, "port")?.is_some() {
            let n = integer(args, "port", 0)?;
            if !(1..=65535).contains(&n) {
                return Err(Error::validation("'port' must be between 1 and 65535."));
            }
            Some(n)
        } else {
            None
        };
        let identity_file = opt_str(args, "identity_file")?.map(expand);
        if let Some(p) = &identity_file
            && !Path::new(p).exists()
        {
            return Err(Error::validation(format!(
                "'identity_file' does not exist: {p}"
            )));
        }
        let known_hosts_file = opt_str(args, "known_hosts_file")?.map(expand);
        if let Some(p) = &known_hosts_file
            && !Path::new(p)
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(Path::new("."))
                .exists()
        {
            return Err(Error::validation(format!(
                "Parent directory for 'known_hosts_file' does not exist: {p}"
            )));
        }
        let strict_host_key_checking = match value(args, "strict_host_key_checking")? {
            None => None,
            Some(Value::Bool(b)) => Some(if *b { "yes" } else { "no" }.to_owned()),
            Some(Value::String(s))
                if ["yes", "no", "ask", "accept-new", "off"]
                    .contains(&s.trim().to_ascii_lowercase().as_str()) =>
            {
                Some(s.trim().to_ascii_lowercase())
            }
            _ => {
                return Err(Error::validation(
                    "'strict_host_key_checking' must be a boolean or one of yes, no, ask, accept-new, off.",
                ));
            }
        };
        let extra_ssh_args = strings(args, "extra_ssh_args")?;
        check_ssh_args(&extra_ssh_args)?;
        Ok(Self {
            target: target.to_owned(),
            port,
            identity_file,
            known_hosts_file,
            strict_host_key_checking,
            extra_ssh_args,
        })
    }
    pub fn transport(&self, program: &str, port_flag: &str, tty: Option<bool>) -> Vec<String> {
        let mut argv = vec![program.to_owned()];
        if let Some(tty) = tty {
            argv.push(if tty { "-tt" } else { "-T" }.to_owned());
        }
        if let Some(port) = self.port {
            argv.extend([port_flag.to_owned(), port.to_string()]);
        }
        if let Some(path) = &self.identity_file {
            argv.extend(["-i".to_owned(), path.clone()]);
        }
        if let Some(path) = &self.known_hosts_file {
            argv.extend([
                "-o".to_owned(),
                format!("UserKnownHostsFile={}", quote(path)),
            ]);
        }
        if let Some(value) = &self.strict_host_key_checking {
            argv.extend(["-o".to_owned(), format!("StrictHostKeyChecking={value}")]);
        }
        argv.extend(self.extra_ssh_args.iter().cloned());
        argv
    }
    pub fn argv(&self, remote: Option<&str>, tty: bool, keepalive: bool) -> Result<Vec<String>> {
        let mut argv = self.transport(&binary("SSH_MCP_SSH_BIN", "ssh")?, "-p", Some(tty));
        if keepalive {
            for (key, default) in [("ServerAliveInterval", "30"), ("ServerAliveCountMax", "3")] {
                if !self
                    .extra_ssh_args
                    .iter()
                    .any(|v| v.to_ascii_lowercase().contains(&key.to_ascii_lowercase()))
                {
                    argv.extend(["-o".to_owned(), format!("{key}={default}")]);
                }
            }
        }
        argv.push(self.target.clone());
        if let Some(remote) = remote {
            if remote.contains('\0') {
                return Err(Error::validation(
                    "Remote command cannot contain NUL bytes.",
                ));
            }
            argv.push(remote.to_owned());
        }
        Ok(argv)
    }
}

// Guard every error path: descendants are signalled as a group and the direct child is reaped.
struct Process(Child, bool);
impl Drop for Process {
    fn drop(&mut self) {
        if !self.1 {
            unsafe {
                libc::kill(-(self.0.id() as i32), libc::SIGKILL);
            }
            let _ = self.0.wait();
        }
    }
}
fn nonblocking(fd: i32) -> io::Result<()> {
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags < 0 || libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}
fn read_available(fd: i32, output: &mut Vec<u8>, pty: bool) -> io::Result<bool> {
    let mut buf = [0u8; 16384];
    for _ in 0..64 {
        let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
        if n > 0 {
            output.extend_from_slice(&buf[..n as usize]);
        } else if n == 0 {
            return Ok(false);
        } else {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::WouldBlock {
                return Ok(true);
            }
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            if pty && err.raw_os_error() == Some(libc::EIO) {
                return Ok(false);
            }
            return Err(err);
        }
    }
    Ok(true)
}

/// Capture raw subprocess bytes without newline normalization. A PTY merges stderr into stdout.
pub fn run(
    argv: &[String],
    input: Option<&[u8]>,
    timeout: Option<f64>,
    tty: bool,
) -> Result<Value> {
    let Some(program) = argv.first() else {
        return Err(Error::validation("Subprocess argv cannot be empty."));
    };
    if argv.iter().any(|s| s.contains('\0')) {
        return Err(Error::validation(
            "Subprocess arguments cannot contain NUL bytes.",
        ));
    }
    let timeout = timeout
        .map(|n| {
            if !n.is_finite() || n <= 0.0 {
                return Err(Error::validation("Timeout must be finite and positive."));
            }
            Duration::try_from_secs_f64(n)
                .map_err(|_| Error::validation("Timeout exceeds supported duration."))
        })
        .transpose()?;
    let start = Instant::now();
    let mut cmd = Command::new(program);
    cmd.args(&argv[1..]); // Inherit SSH_ASKPASS, DISPLAY, HOME and all existing OpenSSH environment.
    let master = if tty {
        let mut m = -1;
        let mut s = -1;
        if unsafe {
            libc::openpty(
                &mut m,
                &mut s,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        } < 0
        {
            return Err(io::Error::last_os_error().into());
        }
        let master_file = unsafe { File::from_raw_fd(m) };
        let slave = unsafe { File::from_raw_fd(s) };
        // openpty does not set close-on-exec; no master descriptor may survive in the child.
        if unsafe { libc::fcntl(m, libc::F_SETFD, libc::FD_CLOEXEC) } < 0
            || unsafe { libc::fcntl(s, libc::F_SETFD, libc::FD_CLOEXEC) } < 0
        {
            return Err(io::Error::last_os_error().into());
        }
        nonblocking(m)?;
        cmd.stdin(Stdio::from(slave.try_clone()?))
            .stdout(Stdio::from(slave.try_clone()?))
            .stderr(Stdio::from(slave));
        Some(master_file)
    } else {
        cmd.stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
        None
    };
    unsafe {
        cmd.pre_exec(move || {
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            if tty && libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut process = Process(cmd.spawn()?, false);
    let mut stdin = process.0.stdin.take();
    let stdout = process.0.stdout.take();
    let stderr = process.0.stderr.take();
    let out_fd = master
        .as_ref()
        .map(AsRawFd::as_raw_fd)
        .or_else(|| stdout.as_ref().map(AsRawFd::as_raw_fd))
        .unwrap();
    let err_fd = stderr.as_ref().map(AsRawFd::as_raw_fd);
    let mut input_fd = if tty {
        input.map(|_| out_fd)
    } else {
        stdin.as_ref().map(AsRawFd::as_raw_fd)
    };
    nonblocking(out_fd)?;
    if let Some(fd) = err_fd {
        nonblocking(fd)?;
    }
    if let Some(fd) = input_fd {
        nonblocking(fd)?;
    }
    let mut out = Vec::new();
    let mut err = Vec::new();
    let mut out_open = true;
    let mut err_open = err_fd.is_some();
    let mut offset = 0;
    let input = input.unwrap_or_default();
    let mut status = None;
    let mut timed_out = false;
    let mut term_at = None;
    let mut killed = false;
    loop {
        if status.is_none() {
            status = process.0.try_wait()?;
        }
        if !timed_out
            && timeout.is_some_and(|limit| start.elapsed() >= limit)
            && (status.is_none() || out_open || err_open)
        {
            timed_out = true;
            term_at = Some(Instant::now());
            unsafe {
                libc::kill(-(process.0.id() as i32), libc::SIGTERM);
            }
        }
        // Kill surviving grandchildren even when the leader already exited after SIGTERM.
        if !killed && term_at.is_some_and(|t| t.elapsed() >= Duration::from_millis(500)) {
            unsafe {
                libc::kill(-(process.0.id() as i32), libc::SIGKILL);
            }
            killed = true;
        }
        if input_fd.is_some() && offset == input.len() {
            input_fd = None;
            stdin.take();
        }
        let mut polls = [
            libc::pollfd {
                fd: if out_open { out_fd } else { -1 },
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: if err_open { err_fd.unwrap() } else { -1 },
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: input_fd.unwrap_or(-1),
                events: libc::POLLOUT,
                revents: 0,
            },
        ];
        if unsafe { libc::poll(polls.as_mut_ptr(), polls.len() as _, 20) } < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error.into());
        }
        if polls[0].revents != 0 {
            out_open = read_available(out_fd, &mut out, tty)?;
        }
        if polls[1].revents != 0 {
            err_open = read_available(err_fd.unwrap(), &mut err, false)?;
        }
        if let Some(fd) = input_fd {
            if polls[2].revents & libc::POLLOUT != 0 {
                let n = unsafe {
                    libc::write(fd, input[offset..].as_ptr().cast(), input.len() - offset)
                };
                if n >= 0 {
                    offset += n as usize;
                } else {
                    let error = io::Error::last_os_error();
                    if error.kind() == io::ErrorKind::BrokenPipe
                        || (tty && error.raw_os_error() == Some(libc::EIO))
                    {
                        input_fd = None;
                        stdin.take();
                    } else if ![io::ErrorKind::WouldBlock, io::ErrorKind::Interrupted]
                        .contains(&error.kind())
                    {
                        return Err(error.into());
                    }
                }
            } else if polls[2].revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
                input_fd = None;
                stdin.take();
            }
        }
        if status.is_some() && (!out_open && !err_open || tty) {
            read_available(out_fd, &mut out, tty)?;
            if let Some(fd) = err_fd {
                read_available(fd, &mut err, false)?;
            }
            // A process can leave a child holding the PTY: do not leak it on timeout.
            if timed_out {
                unsafe {
                    libc::kill(-(process.0.id() as i32), libc::SIGKILL);
                }
            }
            break;
        }
    }
    let status = match status {
        Some(status) => status,
        None => process.0.wait()?,
    };
    process.1 = true;
    Ok(
        json!({"exit_code":status.code(), "signal":status.signal(), "stdout":String::from_utf8_lossy(&out),
        "stderr":String::from_utf8_lossy(&err), "timed_out":timed_out, "duration_ms":start.elapsed().as_millis(),
        "ok":status.success() && !timed_out}),
    )
}
pub fn ssh_exec(args: &Value) -> Result<Value> {
    let connection = Connection::from_args(args)?;
    let command = raw_str(args, "command")?;
    let remote = remote_command(args, command, false)?;
    let tty = boolean(args, "tty", false)?;
    let argv = connection.argv(Some(&remote), tty, false)?;
    let mut result = run(&argv, None, timeout(args)?, tty)?;
    result["target"] = json!(connection.target);
    result["command"] = json!(command);
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn shell(script: &str, timeout: f64, tty: bool) -> Value {
        run(
            &["/bin/sh".to_owned(), "-c".to_owned(), script.to_owned()],
            None,
            Some(timeout),
            tty,
        )
        .unwrap()
    }
    #[test]
    fn quoting_and_remote_environment_are_literal() {
        for text in [
            "",
            "safe/@host-123",
            "one ' two",
            "$(echo injected);\n\"\\",
            "日本語\r\n",
        ] {
            let out = shell(&format!("printf %s {}", quote(text)), 2.0, false);
            assert_eq!(out["stdout"], text);
            assert_eq!(out["ok"], true);
        }
        let args = json!({"env":{"VALUE":"';$(echo bad)\r\n"},"cwd":"/", "shell":"/bin/sh"});
        let command = remote_command(&args, "printf %s \"$VALUE\"", false).unwrap();
        assert_eq!(shell(&command, 2.0, false)["stdout"], "';$(echo bad)\r\n");
        assert!(remote_command(&json!({"env":{"EVIL;echo bad":"x"}}), "true", false).is_err());
        assert!(remote_command(&json!({"env":{"GOOD":"\u{0}"}}), "true", false).is_err());
        assert!(remote_command(&json!({"env":{"GOOD":false}}), "true", false).is_err());
        assert_eq!(remote_command(&json!({}), "", true).unwrap(), "");
    }
    #[test]
    fn injection_flags_and_numeric_boundaries() {
        for flags in [
            vec!["-L9000:host:80"],
            vec!["-vD", "9000"],
            vec!["-o", "ProxyCommand=evil"],
            vec!["-oLoCaLcOmMaNd evil"],
            vec!["-voKnownHostsCommand=evil"],
            vec!["-Ievil.so"],
            vec!["-oForkAfterAuthentication=yes"],
            vec!["-F", "evil.conf"],
            vec!["--", "-oevil"],
            vec!["evil-host", "command"],
        ] {
            assert!(
                Connection::from_args(&json!({"target":"host","extra_ssh_args":flags})).is_err()
            );
        }
        let c = Connection::from_args(&json!({"target":"user@host", "port":2222, "extra_ssh_args":["-vC","-J","jump","-o","ConnectTimeout=4"]})).unwrap();
        assert!(
            Connection::from_args(
                &json!({"target":"host", "extra_ssh_args":["-oForkAfterAuthentication=no"]})
            )
            .is_ok()
        );
        let a = c.transport("ssh", "-p", Some(false));
        assert_eq!(&a[..4], &["ssh", "-T", "-p", "2222"]);
        for target in ["-oProxyCommand=evil", "host\u{0}", "host name"] {
            assert!(Connection::from_args(&json!({"target":target})).is_err());
        }
        for n in ["NaN", "inf", "-1", "0", "1e100"] {
            assert!(timeout(&json!({"timeout":n})).is_err());
        }
        assert!(number(&json!({"n":true}), "n", 0.0).is_err());
        assert_eq!(integer(&json!({"n":2.9}), "n", 0).unwrap(), 2);
        assert_eq!(integer(&json!({"n":-1}), "n", 0).unwrap(), -1);
        assert_eq!(integer(&json!({}), "n", 7).unwrap(), 7);
        assert!(integer(&json!({"n":"2.9"}), "n", 0).is_err());
        assert!(Connection::from_args(&json!({"target":"host","port":65536})).is_err());
        assert!(opt_str(&json!({"cwd":"\u{0}"}), "cwd").is_err());
    }
    #[test]
    fn binary_capture_and_streamed_input_preserve_crlf() {
        let bytes = b"a\r\nb\rc\n\0".repeat(20000);
        let out = run(&["/bin/cat".to_owned()], Some(&bytes), Some(3.0), false).unwrap();
        assert_eq!(out["stdout"].as_str().unwrap().as_bytes(), bytes);
        assert_eq!(out["ok"], true);
        let out = shell("printf 'a\\r\\nb\\r'; printf 'err\\r\\n' >&2", 2.0, false);
        assert_eq!(out["stdout"], "a\r\nb\r");
        assert_eq!(out["stderr"], "err\r\n");
        assert_eq!(shell("exit 7", 2.0, false)["exit_code"], 7);
    }
    #[test]
    fn pty_is_real_and_timeout_escalates() {
        let out = shell(
            "test -t 0 && test -t 1 && test -t 2 || exit 90; stty -opost; printf 'tty\\r\\n'; printf error >&2",
            2.0,
            true,
        );
        assert_eq!(out["ok"], true);
        assert_eq!(out["stdout"], "tty\r\nerror");
        assert_eq!(out["stderr"], "");
        for tty in [false, true] {
            let out = shell("trap '' TERM; printf before; sleep 30", 0.03, tty);
            assert_eq!(out["timed_out"], true);
            assert_eq!(out["ok"], false);
            assert_eq!(out["signal"], 9);
            assert_eq!(out["stdout"], "before");
            assert!(out["duration_ms"].as_u64().unwrap() < 2000);
        }
    }
    #[test]
    fn timeout_kills_descendants_after_leader_exits() {
        let path = std::env::temp_dir().join(format!("ssh-mcp-timeout-{}", uuid::Uuid::new_v4()));
        let script = format!(
            "(trap '' TERM; sleep 1; printf survived > {}) & exit 0",
            quote(path.to_str().unwrap())
        );
        let result = shell(&script, 0.03, false);
        assert_eq!(result["timed_out"], true);
        assert_eq!(result["ok"], false);
        std::thread::sleep(Duration::from_millis(1100));
        let survived = path.exists();
        let _ = std::fs::remove_file(path);
        assert!(!survived, "a descendant survived subprocess-group timeout");
    }
}
