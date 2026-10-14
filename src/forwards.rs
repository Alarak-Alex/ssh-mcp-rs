//! Dedicated OpenSSH forwarding processes. Behavior ported from slepp/ssh-mcp (MIT).
use crate::{Error, Result, core};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::fs::File;
use std::io::{self, Read};
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Child, Command, Stdio};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const STDERR_LIMIT: usize = 65_536;
const STARTUP_WAIT: Duration = Duration::from_millis(500);

struct ForwardSpec {
    target: String,
    direction: String,
    local_port: i64,
    remote_host: String,
    remote_port: i64,
    bind_address: String,
}

impl ForwardSpec {
    fn from_args(args: &Value) -> Result<(Self, Vec<String>)> {
        let connection = core::Connection::from_args(args)?;
        let direction = core::req_str(args, "direction")?;
        if !matches!(direction, "local" | "remote") {
            return Err(Error::validation(
                "'direction' must be one of: local, remote.",
            ));
        }
        let local_port = port(args, "local_port")?;
        let remote_port = port(args, "remote_port")?;
        let remote_host = core::req_str(args, "remote_host")?;
        let bind_address = core::opt_str(args, "bind_address")?.unwrap_or("127.0.0.1");
        let forward_spec = format!(
            "{}:{local_port}:{}:{remote_port}",
            address(bind_address, "bind_address", true)?,
            address(remote_host, "remote_host", false)?
        );
        let mut argv = connection.argv(None, false, true)?;
        // First-value-wins SSH options prevent config/extra args from detaching,
        // multiplexing, or masking failed binds on this owned connection.
        argv.splice(
            1..1,
            [
                "-oExitOnForwardFailure=yes",
                "-oForkAfterAuthentication=no",
                "-oControlMaster=no",
                "-oControlPath=none",
            ]
            .map(String::from),
        );
        let target_index = argv
            .len()
            .checked_sub(1)
            .ok_or_else(|| Error::internal("Missing SSH target."))?;
        if argv[target_index] != connection.target {
            return Err(Error::internal(
                "SSH target must be the final forwarding argument.",
            ));
        }
        argv.splice(
            target_index..target_index,
            [
                "-N".to_owned(),
                if direction == "local" { "-L" } else { "-R" }.to_owned(),
                forward_spec,
            ],
        );
        Ok((
            Self {
                target: connection.target,
                direction: direction.to_owned(),
                local_port,
                remote_host: remote_host.to_owned(),
                remote_port,
                bind_address: bind_address.to_owned(),
            },
            argv,
        ))
    }
}

fn port(args: &Value, key: &str) -> Result<i64> {
    if args.get(key).is_none_or(Value::is_null) {
        return Err(Error::validation(format!("'{key}' is required.")));
    }
    let value = core::integer(args, key, 0)?;
    if !(1..=65535).contains(&value) {
        return Err(Error::validation(format!(
            "'{key}' must be between 1 and 65535."
        )));
    }
    Ok(value)
}

// Colons delimit the SSH forwarding grammar; IPv6 needs brackets, while socket
// paths and embedded specifications must not be accepted as host addresses.
fn address(value: &str, key: &str, wildcard: bool) -> Result<String> {
    let invalid = || Error::validation(format!("'{key}' must be a host name or IP address."));
    let bare = if value.starts_with('[') && value.ends_with(']') {
        &value[1..value.len() - 1]
    } else {
        value
    };
    if bare.contains(':') {
        let (ip, zone) = bare
            .split_once('%')
            .map_or((bare, None), |(ip, zone)| (ip, Some(zone)));
        if ip.parse::<std::net::Ipv6Addr>().is_err()
            || zone.is_some_and(|zone| {
                zone.is_empty()
                    || !zone
                        .bytes()
                        .all(|c| c.is_ascii_alphanumeric() || b"_.-".contains(&c))
            })
        {
            return Err(invalid());
        }
        return Ok(format!("[{bare}]"));
    }
    if bare != value
        || value.is_empty()
        || (!wildcard || value != "*")
            && !value
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"_.-".contains(&c))
    {
        return Err(invalid());
    }
    Ok(value.to_owned())
}

struct Drain {
    stop: Arc<AtomicBool>,
    stderr: Arc<Mutex<VecDeque<u8>>>,
    thread: Option<JoinHandle<()>>,
}

impl Drain {
    fn start(child: &mut Child) -> Result<Self> {
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| Error::internal("Missing forwarding stdout pipe."))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| Error::internal("Missing forwarding stderr pipe."))?;
        let mut pipes = [
            Some(unsafe { File::from_raw_fd(stdout.into_raw_fd()) }),
            Some(unsafe { File::from_raw_fd(stderr.into_raw_fd()) }),
        ];
        for file in pipes.iter().flatten() {
            let fd = file.as_raw_fd();
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
            if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
            {
                return Err(io::Error::last_os_error().into());
            }
        }
        let stop = Arc::new(AtomicBool::new(false));
        let stderr = Arc::new(Mutex::new(VecDeque::with_capacity(STDERR_LIMIT)));
        let thread_stop = Arc::clone(&stop);
        let thread_stderr = Arc::clone(&stderr);
        let thread = thread::Builder::new()
            .name("ssh-forward-output".into())
            .spawn(move || {
                let mut buffer = [0u8; 8192];
                while !thread_stop.load(Ordering::Acquire) && pipes.iter().any(Option::is_some) {
                    let mut pollfds = pipes.each_ref().map(|pipe| libc::pollfd {
                        fd: pipe.as_ref().map_or(-1, AsRawFd::as_raw_fd),
                        events: libc::POLLIN,
                        revents: 0,
                    });
                    let ready = unsafe { libc::poll(pollfds.as_mut_ptr(), 2, 20) };
                    if ready <= 0 {
                        continue;
                    }
                    for index in 0..2 {
                        if pollfds[index].revents == 0 {
                            continue;
                        }
                        if let Some(pipe) = &mut pipes[index] {
                            match pipe.read(&mut buffer) {
                                Ok(0) => pipes[index] = None,
                                Ok(count) if index == 1 => {
                                    let mut tail =
                                        thread_stderr.lock().unwrap_or_else(|p| p.into_inner());
                                    let excess = (tail.len() + count).saturating_sub(STDERR_LIMIT);
                                    tail.drain(..excess);
                                    tail.extend(&buffer[..count]);
                                }
                                Ok(_) => {}
                                Err(error)
                                    if matches!(
                                        error.kind(),
                                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                                    ) => {}
                                Err(_) => pipes[index] = None,
                            }
                        }
                    }
                }
            })?;
        Ok(Self {
            stop,
            stderr,
            thread: Some(thread),
        })
    }

    fn stderr(&self) -> String {
        let bytes: Vec<_> = self
            .stderr
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .copied()
            .collect();
        String::from_utf8_lossy(&bytes).trim().to_owned()
    }

    fn close(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for Drain {
    fn drop(&mut self) {
        self.close();
    }
}

struct Forward {
    id: String,
    spec: ForwardSpec,
    child: Child,
    drain: Option<Drain>,
    started: Instant,
    group_closed: bool,
}

impl Forward {
    fn spawn(id: String, spec: ForwardSpec, argv: &[String]) -> Result<Self> {
        let (program, arguments) = argv
            .split_first()
            .ok_or_else(|| Error::internal("Empty forwarding command."))?;
        let mut command = Command::new(program);
        command
            .args(arguments)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut entry = Self {
            id,
            spec,
            child: command.spawn()?,
            drain: None,
            started: Instant::now(),
            group_closed: false,
        };
        entry.drain = Some(Drain::start(&mut entry.child)?);
        Ok(entry)
    }

    fn summary(&mut self) -> Result<Value> {
        let status = self.child.try_wait()?;
        if status.is_some()
            && !self.group_closed
            && unsafe { libc::kill(-(self.child.id() as i32), 0) } < 0
            && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
        {
            self.group_closed = true;
        }
        let uptime = status
            .is_none()
            .then(|| (self.started.elapsed().as_secs_f64() * 10.0).round() / 10.0);
        Ok(json!({ "forward_id": self.id, "target": self.spec.target,
            "direction": self.spec.direction, "local_port": self.spec.local_port,
            "remote_host": self.spec.remote_host, "remote_port": self.spec.remote_port,
            "bind_address": self.spec.bind_address, "running": status.is_none(),
            "exit_code": status.and_then(|s| s.code()), "signal": status.and_then(|s| s.signal()),
            "uptime_seconds": uptime }))
    }

    fn signal_group(&self, signal: i32) -> Result<()> {
        if unsafe { libc::kill(-(self.child.id() as i32), signal) } < 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ESRCH) {
                return Err(error.into());
            }
        }
        Ok(())
    }

    fn terminate(&mut self, grace: Duration) -> Result<bool> {
        if self.group_closed {
            if let Some(drain) = &mut self.drain {
                drain.close();
            }
            return Ok(false);
        }
        self.signal_group(libc::SIGTERM)?;
        let deadline = Instant::now() + grace;
        let mut forced = false;
        loop {
            self.child.try_wait()?; // Reap the leader even when descendants survive it.
            let group_alive = unsafe { libc::kill(-(self.child.id() as i32), 0) } == 0;
            if !group_alive {
                break;
            }
            if Instant::now() >= deadline {
                self.signal_group(libc::SIGKILL)?;
                forced = true;
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        self.child.wait()?;
        self.group_closed = true;
        if let Some(drain) = &mut self.drain {
            drain.close();
        }
        Ok(forced)
    }

    fn stop(&mut self) -> Result<Value> {
        let was_running = self.child.try_wait()?.is_none();
        let forced = self.terminate(Duration::from_secs(2))?;
        let mut result = self.summary()?;
        result["was_running"] = json!(was_running);
        result["termination_signal"] = if was_running {
            json!(if forced { "SIGKILL" } else { "SIGTERM" })
        } else {
            Value::Null
        };
        result["forced_kill"] = json!(forced);
        Ok(result)
    }
}

impl Drop for Forward {
    fn drop(&mut self) {
        if self.terminate(Duration::from_millis(500)).is_err() {
            let _ = self.signal_group(libc::SIGKILL);
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

#[derive(Default)]
pub struct Forwards {
    entries: Vec<Forward>,
}

impl Forwards {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn call(&mut self, tool: &str, args: &Value) -> Result<Value> {
        match tool {
            "ssh_forward" => {
                let (spec, argv) = ForwardSpec::from_args(args)?;
                let id = loop {
                    let id = uuid::Uuid::new_v4().simple().to_string()[..12].to_owned();
                    if !self.entries.iter().any(|entry| entry.id == id) {
                        break id;
                    }
                };
                self.entries.push(Forward::spawn(id, spec, &argv)?);
                let entry = self.entries.last_mut().unwrap();
                thread::sleep(STARTUP_WAIT);
                let mut result = entry.summary()?;
                if result["running"] == false {
                    // A failed SSH startup is a stopped forward, not a tool exception.
                    // Give the nonblocking reader time to consume the final diagnostics.
                    let deadline = Instant::now() + Duration::from_millis(100);
                    if let Some(drain) = &mut entry.drain {
                        while drain
                            .thread
                            .as_ref()
                            .is_some_and(|thread| !thread.is_finished())
                            && Instant::now() < deadline
                        {
                            thread::sleep(Duration::from_millis(5));
                        }
                        let stderr = drain.stderr();
                        if !stderr.is_empty() {
                            result["stderr"] = json!(stderr);
                        }
                    }
                    entry.terminate(Duration::from_millis(500))?;
                }
                Ok(result)
            }
            "ssh_list_forwards" => {
                let include_stopped = core::boolean(args, "include_stopped", false)?;
                let target = core::opt_str(args, "target")?;
                let mut forwards = Vec::new();
                for entry in &mut self.entries {
                    let summary = entry.summary()?;
                    if (!include_stopped && summary["running"] == false)
                        || target.is_some_and(|target| target != entry.spec.target)
                    {
                        continue;
                    }
                    forwards.push(summary);
                }
                Ok(json!({ "count": forwards.len(), "forwards": forwards }))
            }
            "ssh_stop_forward" => {
                let id = core::req_str(args, "forward_id")?;
                self.entries
                    .iter_mut()
                    .find(|entry| entry.id == id)
                    .ok_or_else(|| {
                        Error::new("forward_not_found", format!("Unknown forward_id '{id}'."))
                    })?
                    .stop()
            }
            _ => Err(Error::validation(format!(
                "Unknown forwarding tool '{tool}'."
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    fn args() -> Value {
        json!({ "target": "127.0.0.1", "direction": "local", "local_port": 15432,
            "remote_host": "localhost", "remote_port": 5432 })
    }

    fn process(script: &str) -> Forward {
        let (spec, _) = ForwardSpec::from_args(&args()).unwrap();
        Forward::spawn(
            "test-forward".into(),
            spec,
            &["/bin/sh".into(), "-c".into(), script.into()],
        )
        .unwrap()
    }

    fn exited(entry: &mut Forward) -> Value {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let summary = entry.summary().unwrap();
            if summary["running"] == false {
                return summary;
            }
            assert!(Instant::now() < deadline, "child failed to exit");
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn forwards_validate_and_build_owned_connections() {
        for direction in ["local", "remote"] {
            let mut value = args();
            value["direction"] = json!(direction);
            value["remote_host"] = json!("::1");
            value["extra_ssh_args"] = json!(["-oExitOnForwardFailure=no"]);
            let (spec, argv) = ForwardSpec::from_args(&value).unwrap();
            assert_eq!(spec.bind_address, "127.0.0.1");
            let flag = if direction == "local" { "-L" } else { "-R" };
            let index = argv.iter().position(|item| item == flag).unwrap();
            assert_eq!(argv[index + 1], "127.0.0.1:15432:[::1]:5432");
            assert!(argv.contains(&"-N".into()));
            assert_eq!(argv[1], "-oExitOnForwardFailure=yes");
            assert_eq!(argv[2], "-oForkAfterAuthentication=no");
            assert_eq!(argv[3], "-oControlMaster=no");
            assert_eq!(argv[4], "-oControlPath=none");
            assert_eq!(argv.last().unwrap(), "127.0.0.1");
        }
        for forbidden in [
            "-oForkAfterAuthentication=yes",
            "-oControlMaster=yes",
            "-oControlPersist=yes",
        ] {
            let mut value = args();
            value["extra_ssh_args"] = json!([forbidden]);
            let mut manager = Forwards::new();
            let error = manager.call("ssh_forward", &value).unwrap_err();
            assert_eq!(error.kind, "validation_error");
            assert!(error.message.contains("Blocked SSH option"), "{error}");
            assert!(
                manager.entries.is_empty(),
                "unsafe option spawned a forwarding process"
            );
        }
        for (key, value) in [
            ("local_port", json!(0)),
            ("remote_port", json!(65536)),
            ("direction", json!("dynamic")),
            ("remote_host", json!("host:80:other")),
            ("bind_address", json!("/tmp/socket")),
            ("remote_host", json!("host\0")),
            ("local_port", json!(true)),
        ] {
            let mut arguments = args();
            arguments[key] = value;
            assert_eq!(
                ForwardSpec::from_args(&arguments).err().unwrap().kind,
                "validation_error"
            );
        }
        let mut numeric = args();
        numeric["local_port"] = json!(1.5);
        assert_eq!(ForwardSpec::from_args(&numeric).unwrap().0.local_port, 1);
        assert_eq!(address("[::1]", "host", false).unwrap(), "[::1]");
        assert_eq!(
            address("fe80::1%lo0", "host", false).unwrap(),
            "[fe80::1%lo0]"
        );
        assert_eq!(address("*", "host", true).unwrap(), "*");
        assert!(address("*", "host", false).is_err());
        let error = Forwards::new()
            .call("ssh_stop_forward", &json!({"forward_id":"missing"}))
            .unwrap_err();
        assert_eq!(error.kind, "forward_not_found");
    }

    #[test]
    fn forwards_drain_bounded_output_and_reap_natural_exit() {
        let mut entry = process(
            "head -c 131072 /dev/zero; head -c 131072 /dev/zero >&2; printf diagnostic >&2; exit 7",
        );
        let summary = exited(&mut entry);
        assert_eq!(summary["exit_code"], 7);
        assert_eq!(summary["signal"], Value::Null);
        assert_eq!(summary["uptime_seconds"], Value::Null);
        let drain = entry.drain.as_mut().unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while !drain.thread.as_ref().unwrap().is_finished() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        assert!(drain.thread.as_ref().unwrap().is_finished());
        assert_eq!(drain.stderr.lock().unwrap().len(), STDERR_LIMIT);
        assert!(drain.stderr().ends_with("diagnostic"));
        let stopped = entry.stop().unwrap();
        assert_eq!(stopped["was_running"], false);
        assert_eq!(stopped["forced_kill"], false);
        assert_eq!(stopped["termination_signal"], Value::Null);
        let mut status = 0;
        assert_eq!(
            unsafe { libc::waitpid(entry.child.id() as i32, &mut status, libc::WNOHANG) },
            -1
        );
        assert_eq!(
            io::Error::last_os_error().raw_os_error(),
            Some(libc::ECHILD)
        );
    }

    #[test]
    fn forwards_stop_list_and_drop_real_process_groups() {
        let mut manager = Forwards {
            entries: vec![process("exec sleep 30")],
        };
        assert_eq!(
            manager.call("ssh_list_forwards", &json!({})).unwrap()["count"],
            1
        );
        assert_eq!(
            manager
                .call("ssh_list_forwards", &json!({"target":"other"}))
                .unwrap()["count"],
            0
        );
        let stopped = manager
            .call("ssh_stop_forward", &json!({"forward_id":"test-forward"}))
            .unwrap();
        assert_eq!(stopped["running"], false);
        assert_eq!(stopped["was_running"], true);
        assert_eq!(stopped["termination_signal"], "SIGTERM");
        assert_eq!(stopped["signal"], libc::SIGTERM);
        assert_eq!(
            manager.call("ssh_list_forwards", &json!({})).unwrap()["count"],
            0
        );
        assert_eq!(
            manager
                .call("ssh_list_forwards", &json!({"include_stopped":true}))
                .unwrap()["count"],
            1
        );
        assert_eq!(
            manager
                .call("ssh_stop_forward", &json!({"forward_id":"test-forward"}))
                .unwrap()["was_running"],
            false
        );
        let mut stubborn = process("trap '' TERM; exec sleep 30");
        thread::sleep(Duration::from_millis(50));
        assert!(stubborn.terminate(Duration::from_millis(50)).unwrap());
        assert_eq!(stubborn.summary().unwrap()["signal"], libc::SIGKILL);
        let entry = process("trap 'wait; exit 0' TERM; sleep 30 & wait");
        let pid = entry.child.id() as i32;
        thread::sleep(Duration::from_millis(50));
        drop(Forwards {
            entries: vec![entry],
        });
        let mut status = 0;
        assert_eq!(
            unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) },
            -1
        );
        assert_eq!(
            io::Error::last_os_error().raw_os_error(),
            Some(libc::ECHILD)
        );
        assert_eq!(
            unsafe { libc::kill(-pid, 0) },
            -1,
            "descendant process group survived Drop"
        );
    }

    #[test]
    fn forwards_real_openssh_reports_immediate_connection_failure() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let mut value = args();
        value["port"] = json!(port);
        value["extra_ssh_args"] = json!(["-oBatchMode=yes", "-oConnectTimeout=1"]);
        let mut manager = Forwards::new();
        let started = manager.call("ssh_forward", &value).unwrap();
        assert_eq!(started["running"], false);
        assert_eq!(started["exit_code"], 255);
        assert!(!started["stderr"].as_str().unwrap().is_empty());
        assert_eq!(
            manager
                .call("ssh_list_forwards", &json!({"include_stopped":true}))
                .unwrap()["count"],
            1
        );
    }
}
