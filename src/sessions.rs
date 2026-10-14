// Session behavior ported from slepp/ssh-mcp (MIT), pinned upstream revision
// 7aec8545080145ad115d74c2ddb9d798cbdcc9e2. See the repository LICENSE.
use crate::{Error, Result, core};
use parking_lot::{Condvar, Mutex};
use serde_json::{Value, json};
use std::collections::{BTreeMap, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const UNREAD_CAP: usize = 1_048_576;
const DEFAULT_OUTPUT: i64 = 65_536;

type Shared = Arc<(Mutex<State>, Condvar)>;
struct State {
    child: Child,
    status: Option<ExitStatus>,
    started: Instant,
    ended: Option<Instant>,
    last_output: Option<Instant>,
    unread: VecDeque<char>,
    dropped: u64,
    total: u64,
    eof: bool,
    error: Option<String>,
}
impl State {
    fn refresh(&mut self) {
        if self.status.is_none() {
            match self.child.try_wait() {
                Ok(Some(status)) => {
                    self.status = Some(status);
                    self.ended = Some(Instant::now());
                    // A completed session must not leave background children
                    // holding its PTY or observer open. Signal only at reap,
                    // never later when this PID may have been recycled.
                    unsafe {
                        libc::kill(-(self.child.id() as libc::pid_t), libc::SIGKILL);
                    }
                }
                Ok(None) => {}
                Err(e) => self.error = Some(format!("Cannot poll session process: {e}")),
            }
        }
    }
    fn append(&mut self, text: &str) {
        self.last_output = Some(Instant::now());
        for c in text.chars() {
            self.total += 1;
            if self.unread.len() == UNREAD_CAP {
                self.unread.pop_front();
                self.dropped += 1;
            }
            self.unread.push_back(c);
        }
    }
}

struct Observer {
    requested: String,
    command: String,
    transcript: PathBuf,
    name: Option<String>,
    binary: Option<String>,
    started: bool,
    warning: Option<String>,
}
impl Observer {
    fn snapshot(&self) -> Value {
        let mut v = json!({"requested_mode":self.requested,"mode":if self.started {"tmux"} else {"transcript"},
            "command":self.command,"transcript_path":self.transcript,"tmux_started":self.started});
        if let Some(name) = &self.name {
            v["tmux_session_name"] = json!(name);
        }
        if let (Some(bin), Some(name)) = (&self.binary, &self.name) {
            v["tmux_launch_command"] = json!(join_argv(&[
                bin.clone(),
                "new-session".into(),
                "-d".into(),
                "-s".into(),
                name.clone(),
                self.command.clone()
            ]));
            if self.started {
                v["tmux_attach_command"] = json!(join_argv(&[
                    bin.clone(),
                    "attach".into(),
                    "-t".into(),
                    name.clone()
                ]));
            }
        }
        if let Some(warning) = &self.warning {
            v["warning"] = json!(warning);
        }
        v
    }
    fn ensure(&mut self, mode: &str) {
        if mode != "tmux" || self.started {
            return;
        }
        let binary = match core::binary("SSH_MCP_TMUX_BIN", "tmux") {
            Ok(binary) => binary,
            Err(e) => {
                self.warning = Some(format!(
                    "Requested tmux observer, but {e}. The transcript observer is still available."
                ));
                return;
            }
        };
        let Some(name) = self.name.clone() else {
            return;
        };
        self.binary = Some(binary.clone());
        let argv = vec![
            binary,
            "new-session".into(),
            "-d".into(),
            "-s".into(),
            name,
            self.command.clone(),
        ];
        match core::run(&argv, None, Some(5.0), false) {
            Ok(v) if v["ok"] == true => {
                self.started = true;
                self.warning = None;
            }
            Ok(v) => {
                self.warning = Some(format!(
                    "Failed to launch the tmux observer: {}. The transcript observer is still available.",
                    if v["timed_out"] == true {
                        "Timed out".to_owned()
                    } else {
                        v["stderr"].as_str().unwrap_or("").trim().to_owned()
                    }
                ))
            }
            Err(e) => {
                self.warning = Some(format!(
                    "Failed to launch the tmux observer: {e}. The transcript observer is still available."
                ))
            }
        }
    }
    fn stop(&mut self) {
        if !self.started {
            return;
        }
        if let (Some(binary), Some(name)) = (&self.binary, &self.name) {
            let argv = vec![
                binary.clone(),
                "kill-session".into(),
                "-t".into(),
                name.clone(),
            ];
            let result = core::run(&argv, None, Some(5.0), false);
            match result {
                Ok(v) => {
                    let detail = v["stderr"].as_str().unwrap_or("").trim();
                    if v["ok"] != true
                        && !detail.contains("can't find session")
                        && !detail.contains("no server running")
                    {
                        self.warning = Some(format!(
                            "Failed to close the tmux observer: {}. The transcript is still available at {}.",
                            if v["timed_out"] == true {
                                "Timed out"
                            } else {
                                detail
                            },
                            self.transcript.display()
                        ));
                        if v["timed_out"] != true {
                            // Keep ownership so explicit stop/Drop can retry
                            // an observer that tmux has not actually closed.
                            return;
                        }
                    }
                }
                Err(e) => {
                    self.warning = Some(format!(
                        "Failed to close the tmux observer: {e}. The transcript is still available at {}.",
                        self.transcript.display()
                    ))
                }
            }
        }
        self.started = false;
        self.binary = None;
        self.name = None;
    }
}
fn join_argv(argv: &[String]) -> String {
    argv.iter()
        .map(|s| core::quote(s))
        .collect::<Vec<_>>()
        .join(" ")
}
fn slug(s: &str) -> String {
    let mut out = String::new();
    let mut invalid = false;
    for c in s.chars() {
        if c.is_ascii_alphanumeric() || "_.-".contains(c) {
            out.push(c);
            invalid = false;
        } else if !invalid {
            out.push('-');
            invalid = true;
        }
    }
    out.trim_matches('-').chars().take(30).collect()
}
fn tmux_name(id: &str, target: &str, name: Option<&str>) -> String {
    format!(
        "ssh-mcp-{}-{}",
        slug(target),
        name.map(slug).unwrap_or_else(|| id[..8].into())
    )
}

// Keep an incomplete UTF-8 sequence between PTY reads, replacing invalid bytes
// exactly once rather than corrupting a character split across kernel chunks.
#[derive(Default)]
struct Decoder {
    pending: Vec<u8>,
}
impl Decoder {
    fn decode(&mut self, bytes: &[u8], eof: bool) -> String {
        self.pending.extend_from_slice(bytes);
        let mut out = String::new();
        let mut used = 0;
        loop {
            match std::str::from_utf8(&self.pending[used..]) {
                Ok(s) => {
                    out.push_str(s);
                    used = self.pending.len();
                    break;
                }
                Err(e) => {
                    let valid = e.valid_up_to();
                    // from_utf8 has already checked this prefix.
                    out.push_str(unsafe {
                        std::str::from_utf8_unchecked(&self.pending[used..used + valid])
                    });
                    used += valid;
                    match e.error_len() {
                        Some(n) => {
                            out.push('\u{fffd}');
                            used += n;
                        }
                        None => {
                            if eof {
                                out.push('\u{fffd}');
                                used = self.pending.len();
                            }
                            break;
                        }
                    }
                }
            }
        }
        self.pending.drain(..used);
        out
    }
}

struct Session {
    id: String,
    name: Option<String>,
    target: String,
    auto_close: bool,
    shared: Shared,
    writer: Arc<Mutex<Option<File>>>,
    observer: Arc<Mutex<Observer>>,
    cancelled: Arc<AtomicBool>,
    reader: Option<JoinHandle<()>>,
    transcript: PathBuf,
}
impl Session {
    fn spawn(
        id: String,
        name: Option<String>,
        target: String,
        argv: &[String],
        state_dir: &Path,
        mode: &str,
        auto_close: bool,
    ) -> Result<Self> {
        let directory = state_dir.join(&id);
        fs::DirBuilder::new().mode(0o700).create(&directory)?;
        let transcript = directory.join("transcript.log");
        let transcript_file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&transcript)?;
        let mut master = -1;
        let mut slave = -1;
        if unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        } != 0
        {
            return Err(io::Error::last_os_error().into());
        }
        // Files own every descriptor immediately, including failed setup paths.
        let master_file = unsafe { File::from_raw_fd(master) };
        let slave_file = unsafe { File::from_raw_fd(slave) };
        for fd in [master, slave] {
            if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
                return Err(io::Error::last_os_error().into());
            }
        }
        let flags = unsafe { libc::fcntl(master, libc::F_GETFL) };
        if flags < 0 || unsafe { libc::fcntl(master, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
        {
            return Err(io::Error::last_os_error().into());
        }
        let read_file = master_file.try_clone()?;
        let program = argv
            .first()
            .ok_or_else(|| Error::internal("Empty session argv"))?;
        let mut command = Command::new(program);
        command
            .args(&argv[1..])
            .stdin(Stdio::from(slave_file.try_clone()?))
            .stdout(Stdio::from(slave_file.try_clone()?))
            .stderr(Stdio::from(slave_file));
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(io::Error::last_os_error());
                }
                if libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command.spawn()?;
        let shared = Arc::new((
            Mutex::new(State {
                child,
                status: None,
                started: Instant::now(),
                ended: None,
                last_output: None,
                unread: VecDeque::new(),
                dropped: 0,
                total: 0,
                eof: false,
                error: None,
            }),
            Condvar::new(),
        ));
        let writer = Arc::new(Mutex::new(Some(master_file)));
        let observer = Arc::new(Mutex::new(Observer {
            requested: mode.into(),
            command: format!(
                "tail -n +1 -f {}",
                core::quote(&transcript.to_string_lossy())
            ),
            transcript: transcript.clone(),
            name: Some(tmux_name(&id, &target, name.as_deref())),
            binary: None,
            started: false,
            warning: None,
        }));
        let cancelled = Arc::new(AtomicBool::new(false));
        let (thread_shared, thread_writer, thread_observer, thread_cancelled) = (
            shared.clone(),
            writer.clone(),
            observer.clone(),
            cancelled.clone(),
        );
        let reader = match thread::Builder::new()
            .name(format!("ssh-session-{id}"))
            .spawn(move || {
                reader_loop(
                    read_file,
                    transcript_file,
                    thread_shared,
                    thread_writer,
                    thread_observer,
                    thread_cancelled,
                )
            }) {
            Ok(reader) => reader,
            Err(e) => {
                let mut state = shared.0.lock();
                signal_group(&mut state, libc::SIGKILL);
                let _ = state.child.wait();
                return Err(e.into());
            }
        };
        let session = Self {
            id,
            name,
            target,
            auto_close,
            shared,
            writer,
            observer,
            cancelled,
            reader: Some(reader),
            transcript,
        };
        session.ensure_observer(mode);
        Ok(session)
    }
    fn ensure_observer(&self, mode: &str) {
        let mut state = self.shared.0.lock();
        state.refresh();
        let running = state.status.is_none() && !state.eof;
        drop(state);
        if running {
            self.observer.lock().ensure(mode);
        }
    }
    fn snapshot(&self, state: &mut State, output: String, truncated: bool) -> Value {
        state.refresh();
        let running = state.status.is_none();
        let exit_code = state.status.and_then(|s| s.code());
        let signal = state.status.and_then(|s| s.signal());
        let reason = signal
            .map(|s| match s {
                9 => "killed".into(),
                13 => "broken-pipe".into(),
                15 => "terminated".into(),
                _ => format!("signal-{s}"),
            })
            .or_else(|| {
                exit_code.map(|c| match c {
                    0 => "clean-exit".into(),
                    255 => "ssh-connection-failed".into(),
                    _ => format!("exit-{c}"),
                })
            });
        let observer = self.observer.lock().snapshot();
        let mut result = json!({"session_id":self.id,"session_name":self.name,"target":self.target,"running":running,
            "exit_code":exit_code,"signal":signal,"exit_reason":reason,"output":output,"truncated":truncated,
            "pending_output_chars":state.unread.len(),"uptime_seconds":round_tenth(state.ended.unwrap_or_else(Instant::now).duration_since(state.started).as_secs_f64()),
            "idle_seconds":state.last_output.map(|t|round_tenth(t.elapsed().as_secs_f64())),"observer":observer,
            "unread_dropped_chars":state.dropped,"total_output_chars":state.total,
            "transcript_size_bytes":fs::metadata(&self.transcript).map(|m|m.len()).unwrap_or(0)});
        if let Some(name) = observer.get("tmux_session_name") {
            result["tmux_session_name"] = name.clone();
        }
        if self.auto_close {
            result["auto_close"] = json!(true);
        }
        if let Some(error) = &state.error {
            result["warning"] = json!(error);
        }
        result
    }
    fn read(&self, wait: Duration, max: usize, wait_for_new: bool) -> Value {
        let began = Instant::now();
        let (mutex, condvar) = &*self.shared;
        let mut state = mutex.lock();
        state.refresh();
        let had_buffer = !wait_for_new && !state.unread.is_empty();
        loop {
            state.refresh();
            if had_buffer || began.elapsed() >= wait || state.eof {
                break;
            }
            // Wait for reader EOF after exit so its last chunk is never lost.
            let duration = wait
                .saturating_sub(began.elapsed())
                .min(Duration::from_millis(100));
            condvar.wait_for(&mut state, duration);
        }
        let amount = max.min(state.unread.len());
        let output: String = state.unread.drain(..amount).collect();
        let truncated = !state.unread.is_empty();
        self.snapshot(&mut state, output, truncated)
    }
    fn write(&self, input: &str, wait: Duration, max: usize) -> Result<Value> {
        {
            let mut state = self.shared.0.lock();
            state.refresh();
            if state.status.is_some() || state.eof {
                return Err(Error::validation(format!(
                    "Session '{}' has already exited or closed.",
                    self.id
                )));
            }
        }
        // Duplicate while holding the fd lock; do not hold the state lock during
        // writes, since the reader must drain a full PTY to make write progress.
        let mut file = self
            .writer
            .lock()
            .as_ref()
            .ok_or_else(|| Error::validation("Session is no longer writable"))?
            .try_clone()?;
        let mut remaining = input.as_bytes();
        while !remaining.is_empty() {
            match file.write(remaining) {
                Ok(0) => return Err(Error::validation("Short write to session PTY")),
                Ok(n) => remaining = &remaining[n..],
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    let mut poll = libc::pollfd {
                        fd: file.as_raw_fd(),
                        events: libc::POLLOUT,
                        revents: 0,
                    };
                    if unsafe { libc::poll(&mut poll, 1, 100) } < 0
                        && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted
                    {
                        return Err(Error::validation(io::Error::last_os_error().to_string()));
                    }
                    let mut state = self.shared.0.lock();
                    state.refresh();
                    if state.status.is_some() || state.eof {
                        return Err(Error::validation("Session closed while writing"));
                    }
                }
                Err(e) => {
                    return Err(Error::validation(format!(
                        "Failed to write to session '{}': {e}",
                        self.id
                    )));
                }
            }
        }
        let mut result = self.read(wait, max, true);
        result["bytes_written"] = json!(input.len());
        Ok(result)
    }
    fn stop(&mut self, force: bool, wait: Duration, max: usize) -> Value {
        let mut state = self.shared.0.lock();
        state.refresh();
        let was_running = state.status.is_none();
        if was_running {
            signal_group(
                &mut state,
                if force { libc::SIGKILL } else { libc::SIGTERM },
            );
        }
        drop(state);
        let began = Instant::now();
        loop {
            let mut state = self.shared.0.lock();
            state.refresh();
            if state.status.is_some() {
                break;
            }
            if began.elapsed() >= wait {
                signal_group(&mut state, libc::SIGKILL);
                // SIGKILL cannot be caught; reap before reporting termination.
                match state.child.wait() {
                    Ok(s) => {
                        state.status = Some(s);
                        state.ended = Some(Instant::now());
                    }
                    Err(e) => state.error = Some(e.to_string()),
                }
                break;
            }
            drop(state);
            thread::sleep(Duration::from_millis(10));
        }
        // The force-kill path already signalled the full process group;
        // refresh also settles descendants when it first observes exit.
        let drain = Instant::now();
        while !self.shared.0.lock().eof && drain.elapsed() < Duration::from_secs(1) {
            thread::sleep(Duration::from_millis(5));
        }
        self.cancelled.store(true, Ordering::Release);
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
        self.writer.lock().take();
        self.observer.lock().stop();
        let mut result = self.read(Duration::ZERO, max, false);
        result["was_running"] = json!(was_running);
        result["termination_signal"] = if was_running {
            json!(if force { "SIGKILL" } else { "SIGTERM" })
        } else {
            Value::Null
        };
        result
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        self.stop(true, Duration::from_millis(500), 1);
    }
}
fn round_tenth(n: f64) -> f64 {
    (n * 10.0).round() / 10.0
}
fn signal_group(state: &mut State, signal: i32) {
    let pid = state.child.id() as libc::pid_t;
    if unsafe { libc::kill(-pid, signal) } != 0 && state.status.is_none() {
        let _ = unsafe { libc::kill(pid, signal) };
    }
}
fn reader_loop(
    mut reader: File,
    mut transcript: File,
    shared: Shared,
    writer: Arc<Mutex<Option<File>>>,
    observer: Arc<Mutex<Observer>>,
    cancelled: Arc<AtomicBool>,
) {
    let mut decoder = Decoder::default();
    let mut buffer = [0u8; 8192];
    loop {
        let mut poll = libc::pollfd {
            fd: reader.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut poll, 1, 100) };
        if ready < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            shared.0.lock().error = Some(format!("PTY poll failed: {e}"));
            break;
        }
        if ready == 0 {
            let mut state = shared.0.lock();
            state.refresh();
            if cancelled.load(Ordering::Acquire) || state.status.is_some() {
                break;
            }
            continue;
        }
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => publish(
                &decoder.decode(&buffer[..n], false),
                &mut transcript,
                &shared,
            ),
            Err(e)
                if e.kind() == io::ErrorKind::Interrupted
                    || e.kind() == io::ErrorKind::WouldBlock =>
            {
                if cancelled.load(Ordering::Acquire) {
                    break;
                }
            }
            Err(e) if e.raw_os_error() == Some(libc::EIO) => break,
            Err(e) => {
                shared.0.lock().error = Some(format!("PTY read failed: {e}"));
                break;
            }
        }
    }
    publish(&decoder.decode(&[], true), &mut transcript, &shared);
    // Close files before publishing EOF; an EOF response sees the full transcript.
    drop(transcript);
    drop(reader);
    writer.lock().take();
    {
        let mut state = shared.0.lock();
        state.eof = true;
        state.refresh();
        shared.1.notify_all();
    }
    observer.lock().stop();
    // PTY EOF may precede process exit. Keep reaping in the background so
    // an idle MCP server does not accumulate zombies between tool calls.
    loop {
        let mut state = shared.0.lock();
        state.refresh();
        if state.status.is_some() || cancelled.load(Ordering::Acquire) {
            shared.1.notify_all();
            break;
        }
        drop(state);
        thread::sleep(Duration::from_millis(10));
    }
}
fn publish(text: &str, transcript: &mut File, shared: &Shared) {
    if text.is_empty() {
        return;
    }
    let written = transcript.write_all(text.as_bytes());
    let mut state = shared.0.lock();
    if let Err(e) = written {
        state.error = Some(format!("Transcript write failed: {e}"));
    }
    state.append(text);
    shared.1.notify_all();
}

pub struct Sessions {
    sessions: BTreeMap<String, Session>,
    state_dir: PathBuf,
    last_prune: Option<Instant>,
}
impl Sessions {
    pub fn new() -> Result<Self> {
        let directory = std::env::var("SSH_MCP_STATE_DIR")
            .ok()
            .filter(|s| !s.is_empty())
            .map(|s| core::expand(&s))
            .or_else(|| {
                std::env::var("XDG_STATE_HOME")
                    .ok()
                    .filter(|s| !s.is_empty())
                    .map(|s| format!("{}/ssh-mcp", core::expand(&s)))
            })
            .unwrap_or_else(|| core::expand("~/.local/state/ssh-mcp"));
        let state_dir = PathBuf::from(directory);
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&state_dir)?;
        Ok(Self {
            sessions: BTreeMap::new(),
            state_dir,
            last_prune: None,
        })
    }
    fn prune(&mut self) {
        if self
            .last_prune
            .is_some_and(|t| t.elapsed() < Duration::from_secs(60))
        {
            return;
        }
        self.last_prune = Some(Instant::now());
        self.sessions.retain(|_, session| {
            let mut state = session.shared.0.lock();
            state.refresh();
            !state.ended.is_some_and(|t| {
                t.elapsed() > Duration::from_secs(if session.auto_close { 300 } else { 3600 })
            })
        });
    }
    pub fn call(&mut self, tool: &str, args: &Value) -> Result<Value> {
        self.prune();
        if tool == "ssh_list_sessions" {
            let include = core::boolean(args, "include_exited", true)?;
            let target = core::opt_str(args, "target")?;
            let name = core::opt_str(args, "session_name")?;
            let mut sessions = Vec::new();
            for session in self.sessions.values() {
                if target.is_some_and(|t| t != session.target)
                    || name.is_some_and(|n| Some(n) != session.name.as_deref())
                {
                    continue;
                }
                let mut state = session.shared.0.lock();
                state.refresh();
                if !include && state.status.is_some() {
                    continue;
                }
                sessions.push(session.snapshot(&mut state, String::new(), false));
            }
            return Ok(json!({"count":sessions.len(),"sessions":sessions}));
        }
        let wait = wait_argument(args, if tool == "ssh_stop_session" { 2.0 } else { 1.0 })?;
        let max = core::integer(args, "max_output_chars", DEFAULT_OUTPUT)?;
        if max < 1 {
            return Err(Error::validation("'max_output_chars' must be >= 1."));
        }
        let max = usize::try_from(max)
            .map_err(|_| Error::validation("'max_output_chars' is too large."))?;
        if tool == "ssh_start_session" || tool == "ssh_ensure_session" {
            let connection = core::Connection::from_args(args)?;
            let name = core::opt_str(args, "session_name")?.map(str::to_owned);
            let mode = core::opt_str(args, "observer_mode")?
                .unwrap_or("tmux")
                .to_ascii_lowercase();
            if mode != "tmux" && mode != "transcript" {
                return Err(Error::validation(
                    "'observer_mode' must be one of: transcript, tmux.",
                ));
            }
            let auto_close = core::boolean(args, "auto_close", false)?;
            let remote = core::remote_command(args, "", true)?;
            let argv = connection.argv(
                if remote.is_empty() {
                    None
                } else {
                    Some(&remote)
                },
                true,
                true,
            )?;
            let mut matching = Vec::new();
            for (id, session) in &self.sessions {
                let mut state = session.shared.0.lock();
                state.refresh();
                if state.status.is_none()
                    && session.target == connection.target
                    && name
                        .as_ref()
                        .is_none_or(|n| session.name.as_ref() == Some(n))
                {
                    matching.push(id.clone());
                }
            }
            let reused = tool == "ssh_ensure_session" && !matching.is_empty();
            if tool == "ssh_start_session" && name.is_some() && !matching.is_empty() {
                return Err(Error::validation(
                    "A running session with this name already exists for this target. Use 'ssh_ensure_session' to reuse it.",
                ));
            }
            if tool == "ssh_ensure_session" && matching.len() > 1 {
                return Err(Error::validation(
                    "Multiple running sessions already exist for this target. Provide a stable 'session_name' or inspect them with 'ssh_list_sessions'.",
                ));
            }
            let id = if reused {
                matching.remove(0)
            } else {
                let id = loop {
                    let candidate = uuid::Uuid::new_v4().simple().to_string()[..12].to_owned();
                    if !self.sessions.contains_key(&candidate) {
                        break candidate;
                    }
                };
                let session = Session::spawn(
                    id.clone(),
                    name.clone(),
                    connection.target,
                    &argv,
                    &self.state_dir,
                    &mode,
                    auto_close,
                )?;
                self.sessions.insert(id.clone(), session);
                id
            };
            let session = self.sessions.get(&id).unwrap();
            if reused {
                session.ensure_observer(&mode);
            }
            let mut result = session.read(wait, max, false);
            if tool == "ssh_ensure_session" {
                result["created"] = json!(!reused);
                result["reused"] = json!(reused);
                result["matched_by"] = if reused {
                    json!(if name.is_some() {
                        "session_name"
                    } else {
                        "target"
                    })
                } else {
                    Value::Null
                };
            }
            return Ok(result);
        }
        let id = core::req_str(args, "session_id")?;
        let session = self.sessions.get_mut(id).ok_or_else(|| {
            Error::new("session_not_found", format!("Unknown session_id '{id}'."))
        })?;
        match tool {
            "ssh_read_session" => Ok(session.read(wait, max, false)),
            "ssh_write_session" => {
                // PTY input is raw, including empty strings, whitespace and NUL.
                let input = args.get("input").and_then(Value::as_str).ok_or_else(|| {
                    Error::validation("'input' is required and must be a string.")
                })?;
                session.write(input, wait, max)
            }
            "ssh_stop_session" => Ok(session.stop(core::boolean(args, "force", false)?, wait, max)),
            _ => Err(Error::validation(format!("Unknown session tool '{tool}'"))),
        }
    }
}
impl Drop for Sessions {
    fn drop(&mut self) {
        self.sessions.clear();
    }
}
fn wait_argument(args: &Value, default: f64) -> Result<Duration> {
    let wait = core::number(args, "wait_seconds", default)?;
    Duration::try_from_secs_f64(wait)
        .map_err(|_| Error::validation("'wait_seconds' must be finite and >= 0."))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    fn directory() -> PathBuf {
        let path = std::env::temp_dir().join(format!("ssh-mcp-sessions-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&path).unwrap();
        path
    }
    fn shell(directory: &Path) -> Session {
        Session::spawn(
            uuid::Uuid::new_v4().simple().to_string()[..12].to_owned(),
            Some("test".into()),
            "local".into(),
            &["/bin/sh".into(), "-i".into()],
            directory,
            "transcript",
            false,
        )
        .unwrap()
    }
    #[test]
    fn real_pty_preserves_shell_state_unicode_and_eof() {
        let directory = directory();
        let mut session = shell(&directory);
        session.read(Duration::from_millis(100), 4096, false);
        let result = session
            .write(
                "stty -echo; VALUE='世界'; printf 'first:%s\\n' \"$VALUE\"\n",
                Duration::from_millis(150),
                4096,
            )
            .unwrap();
        assert!(result["output"].as_str().unwrap().contains("first:世界"));
        let result = session
            .write(
                "printf 'second:%s\\n' \"$VALUE\"; exit 4\n",
                Duration::from_secs(1),
                4096,
            )
            .unwrap();
        assert!(result["output"].as_str().unwrap().contains("second:世界"));
        assert_eq!(result["exit_code"], 4);
        assert_eq!(
            fs::metadata(&session.transcript)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(session.transcript.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert!(
            fs::read_to_string(&session.transcript)
                .unwrap()
                .contains("second:世界")
        );
        assert!(session.writer.lock().is_none());
        assert!(session.write("x", Duration::ZERO, 1).is_err());
        assert_eq!(
            session.stop(false, Duration::ZERO, 4096)["was_running"],
            false
        );
        drop(session);
        fs::remove_dir_all(directory).unwrap();
    }
    #[test]
    fn decoder_and_unicode_ring_are_character_based() {
        let mut decoder = Decoder::default();
        assert_eq!(decoder.decode(&[0xe4, 0xb8], false), "");
        assert_eq!(decoder.decode(&[0x96, 0xff], false), "世�");
        assert_eq!(decoder.decode(&[0xe4], true), "�");
        let directory = directory();
        let mut session = shell(&directory);
        session.read(Duration::from_millis(100), 4096, false);
        {
            let mut state = session.shared.0.lock();
            state.unread.clear();
            state.total = 0;
            state.dropped = 0;
            state.append(&"世".repeat(UNREAD_CAP + 3));
        }
        let v = session.read(Duration::from_secs(1), 2, false);
        assert_eq!(v["output"], "世世");
        assert_eq!(v["unread_dropped_chars"], 3);
        assert_eq!(v["pending_output_chars"], UNREAD_CAP - 2);
        assert_eq!(v["total_output_chars"], UNREAD_CAP + 3);
        assert_eq!(v["truncated"], true);
        session.stop(true, Duration::from_millis(200), 1);
        drop(session);
        fs::remove_dir_all(directory).unwrap();
    }
    #[test]
    fn stop_kills_process_group_and_drop_reaps_child() {
        let directory = directory();
        let mut session = shell(&directory);
        session.read(Duration::from_millis(50), 4096, false);
        session
            .write("sleep 30\n", Duration::from_millis(50), 4096)
            .unwrap();
        let pid = session.shared.0.lock().child.id() as i32;
        let result = session.stop(false, Duration::from_millis(100), 4096);
        assert_eq!(result["running"], false);
        assert_eq!(result["was_running"], true);
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        assert!(session.writer.lock().is_none());
        drop(session);
        fs::remove_dir_all(directory).unwrap();
    }
    #[test]
    fn names_and_wait_validation() {
        assert_eq!(
            tmux_name("0123456789ab", "user@host", Some("deploy api")),
            "ssh-mcp-user-host-deploy-api"
        );
        assert!(wait_argument(&json!({"wait_seconds":-1}), 1.0).is_err());
        assert_eq!(
            wait_argument(&json!({"wait_seconds":"1"}), 1.0).unwrap(),
            Duration::from_secs(1)
        );
        assert!(wait_argument(&json!({"wait_seconds":true}), 1.0).is_err());
        assert!(wait_argument(&json!({"wait_seconds":"NaN"}), 1.0).is_err());
    }
    #[test]
    fn ctrl_d_and_drop_close_real_pty_children() {
        let directory = directory();
        let mut session = shell(&directory);
        session.read(Duration::from_millis(50), 4096, false);
        let result = session
            .write("\u{4}", Duration::from_secs(1), 4096)
            .unwrap();
        assert_eq!(result["running"], false);
        assert_eq!(result["exit_code"], 0);
        session.stop(false, Duration::ZERO, 4096);
        drop(session);
        let session = shell(&directory);
        let pid = session.shared.0.lock().child.id() as i32;
        drop(session);
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        fs::remove_dir_all(directory).unwrap();
    }
    #[test]
    fn auto_close_prunes_exited_sessions_without_deleting_transcripts() {
        let directory = directory();
        let mut session = shell(&directory);
        session.auto_close = true;
        session.read(Duration::from_millis(50), 4096, false);
        session
            .write("exit\n", Duration::from_secs(1), 4096)
            .unwrap();
        let transcript = session.transcript.clone();
        {
            let mut state = session.shared.0.lock();
            state.refresh();
            assert!(state.status.is_some());
            state.ended = Some(Instant::now() - Duration::from_secs(301));
        }
        let mut manager = Sessions {
            sessions: BTreeMap::from([(session.id.clone(), session)]),
            state_dir: directory.clone(),
            last_prune: None,
        };
        manager.prune();
        assert!(manager.sessions.is_empty());
        assert!(transcript.is_file());
        drop(manager);
        fs::remove_dir_all(directory).unwrap();
    }
}
