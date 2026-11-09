//! Blocking client for the podd pipe, plus "start the daemon if absent".
//!
//! Deliberately free of tokio and the PTY stack so the `pod` CLI stays small.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::protocol::*;

const ERROR_PIPE_BUSY: i32 = 231;

/// Environment override for the daemon executable used by auto-start.
pub const ENV_DAEMON_EXE: &str = "POD_DAEMON";

#[derive(Debug)]
pub enum ClientError {
    /// No daemon listens on the pipe.
    NotRunning,
    Io(std::io::Error),
    /// The daemon answered with a JSON-RPC error.
    Rpc(RpcError),
    Protocol(String),
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientError::NotRunning => f.write_str("podd is not running"),
            ClientError::Io(e) => write!(f, "pipe I/O error: {e}"),
            ClientError::Rpc(e) => write!(f, "{}", e.message),
            ClientError::Protocol(m) => write!(f, "protocol error: {m}"),
        }
    }
}

impl std::error::Error for ClientError {}

impl From<std::io::Error> for ClientError {
    fn from(e: std::io::Error) -> Self {
        ClientError::Io(e)
    }
}

pub struct Client {
    pipe: String,
    reader: BufReader<File>,
    writer: File,
    next_id: u64,
}

impl Client {
    /// Connect to a running daemon. Returns [`ClientError::NotRunning`] when the pipe does not exist.
    pub fn connect(pipe: &str) -> Result<Self, ClientError> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match OpenOptions::new().read(true).write(true).open(pipe) {
                Ok(file) => {
                    let writer = file.try_clone()?;
                    return Ok(Self {
                        pipe: pipe.to_string(),
                        reader: BufReader::new(file),
                        writer,
                        next_id: 1,
                    });
                }
                Err(e) if e.kind() == ErrorKind::NotFound => return Err(ClientError::NotRunning),
                // All instances are momentarily taken; the server creates a new one per accept.
                Err(e)
                    if e.raw_os_error() == Some(ERROR_PIPE_BUSY) && Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(e) => return Err(e.into()),
            }
        }
    }

    /// Connect, starting the daemon detached first if the pipe is absent.
    pub fn connect_or_start(pipe: &str, daemon_exe: Option<&Path>) -> Result<Self, ClientError> {
        match Self::connect(pipe) {
            Err(ClientError::NotRunning) => {}
            other => return other,
        }
        let exe = match daemon_exe {
            Some(p) => p.to_path_buf(),
            None => default_daemon_exe()?,
        };
        spawn_daemon(&exe, pipe)?;
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match Self::connect(pipe) {
                Err(ClientError::NotRunning) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(25));
                }
                other => return other,
            }
        }
    }

    pub fn pipe(&self) -> &str {
        &self.pipe
    }

    /// Typed call.
    pub fn call<M: Method>(&mut self, params: &M::Params) -> Result<M::Result, ClientError> {
        let params =
            serde_json::to_value(params).map_err(|e| ClientError::Protocol(e.to_string()))?;
        let result = self.call_raw(M::NAME, params)?;
        serde_json::from_value(result)
            .map_err(|e| ClientError::Protocol(format!("bad {} result: {e}", M::NAME)))
    }

    /// Untyped call; returns the `result` value.
    pub fn call_raw(&mut self, method: &str, params: Value) -> Result<Value, ClientError> {
        let id = self.next_id;
        self.next_id += 1;
        let req = Request {
            jsonrpc: JSONRPC_VERSION.into(),
            id: Some(Value::from(id)),
            method: method.into(),
            params,
        };
        let mut line =
            serde_json::to_string(&req).map_err(|e| ClientError::Protocol(e.to_string()))?;
        line.push('\n');
        self.writer.write_all(line.as_bytes())?;
        self.writer.flush()?;

        let mut buf = String::new();
        let n = self.reader.read_line(&mut buf)?;
        if n == 0 {
            return Err(ClientError::Protocol("daemon closed the connection".into()));
        }
        let resp: Response = serde_json::from_str(&buf)
            .map_err(|e| ClientError::Protocol(format!("bad response: {e}")))?;
        if resp.id != id {
            return Err(ClientError::Protocol(format!(
                "response id {} does not match request id {id}",
                resp.id
            )));
        }
        if let Some(err) = resp.error {
            return Err(ClientError::Rpc(err));
        }
        Ok(resp.result.unwrap_or(Value::Null))
    }
}

/// `POD_DAEMON`, else `podd.exe` next to the current executable.
pub fn default_daemon_exe() -> Result<PathBuf, ClientError> {
    if let Ok(p) = std::env::var(ENV_DAEMON_EXE)
        && !p.is_empty()
    {
        return Ok(PathBuf::from(p));
    }
    let me = std::env::current_exe()?;
    let exe = me.with_file_name(if cfg!(windows) { "podd.exe" } else { "podd" });
    if exe.exists() {
        Ok(exe)
    } else {
        Err(ClientError::Io(std::io::Error::new(
            ErrorKind::NotFound,
            format!(
                "daemon executable not found at {} (set {ENV_DAEMON_EXE})",
                exe.display()
            ),
        )))
    }
}

/// Log file used when the CLI starts the daemon: `%LOCALAPPDATA%\pod\podd.log`.
pub fn default_log_path() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join("pod").join("podd.log")
}

/// Start the daemon with no console and no window, outside our job object if
/// allowed, so it outlives the CLI and the terminal the CLI ran in.
pub fn spawn_daemon(exe: &Path, pipe: &str) -> Result<(), ClientError> {
    let log = default_log_path();
    if let Some(dir) = log.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let mut cmd = Command::new(exe);
    cmd.arg("--pipe").arg(pipe).arg("--log").arg(&log);
    // Start the daemon somewhere that never blocks a directory from being removed.
    if let Some(home) = std::env::var_os("USERPROFILE") {
        cmd.current_dir(home);
    }
    spawn_detached(&mut cmd)?;
    Ok(())
}

/// Spawn with no console, no window, `NUL` stdio and **no other inherited
/// handles**, in a new process group and outside our Job Object when the job
/// allows breakaway (so the process outlives the terminal / test harness that
/// started us). See [`crate::detach`].
pub fn spawn_detached(cmd: &mut Command) -> std::io::Result<crate::detach::DetachedChild> {
    crate::detach::spawn_detached(cmd)
}

/// One event on an attached connection.
#[derive(Debug, Clone, PartialEq)]
pub enum AttachEvent {
    /// Raw output bytes starting at absolute offset `seq`.
    Output {
        seq: u64,
        data: Vec<u8>,
    },
    Resized {
        seq: u64,
        cols: u16,
        rows: u16,
    },
    Exited(ExitedEvent),
    /// v7: this client fell behind and was dropped (last event). Re-attach to
    /// continue from a fresh snapshot.
    Lagged(LaggedEvent),
}

/// The notification stream of a `terminal.attach` connection. Dropping it detaches.
pub struct AttachStream {
    reader: BufReader<File>,
    _writer: File,
}

impl AttachStream {
    /// Next event; `Ok(None)` when the daemon closed the stream.
    pub fn next_event(&mut self) -> Result<Option<AttachEvent>, ClientError> {
        loop {
            let mut buf = String::new();
            if self.reader.read_line(&mut buf)? == 0 {
                return Ok(None);
            }
            let n: Notification = serde_json::from_str(&buf)
                .map_err(|e| ClientError::Protocol(format!("bad notification: {e}")))?;
            let bad =
                |e: serde_json::Error| ClientError::Protocol(format!("bad {}: {e}", n.method));
            match n.method.as_str() {
                NOTIFY_OUTPUT => {
                    let o: OutputEvent = serde_json::from_value(n.params.clone()).map_err(bad)?;
                    let data = b64_decode(&o.data).map_err(ClientError::Protocol)?;
                    return Ok(Some(AttachEvent::Output { seq: o.seq, data }));
                }
                NOTIFY_RESIZED => {
                    let r: ResizedEvent = serde_json::from_value(n.params.clone()).map_err(bad)?;
                    return Ok(Some(AttachEvent::Resized {
                        seq: r.seq,
                        cols: r.cols,
                        rows: r.rows,
                    }));
                }
                NOTIFY_EXITED => {
                    let e: ExitedEvent = serde_json::from_value(n.params.clone()).map_err(bad)?;
                    return Ok(Some(AttachEvent::Exited(e)));
                }
                NOTIFY_LAGGED => {
                    let e: LaggedEvent = serde_json::from_value(n.params.clone()).map_err(bad)?;
                    return Ok(Some(AttachEvent::Lagged(e)));
                }
                _ => continue, // unknown notifications are ignored (forward compatible)
            }
        }
    }
}

impl Client {
    /// Attach: returns the snapshot reply and turns this connection into the
    /// event stream. Send input/resizes on a different connection.
    pub fn attach(
        mut self,
        params: &AttachParams,
    ) -> Result<(AttachResult, Vec<u8>, AttachStream), ClientError> {
        let result = self.call::<TerminalAttach>(params)?;
        let snapshot = b64_decode(&result.snapshot).map_err(ClientError::Protocol)?;
        Ok((
            result,
            snapshot,
            AttachStream {
                reader: self.reader,
                _writer: self.writer,
            },
        ))
    }
}

/// What a `status.subscribe` connection delivers.
#[derive(Debug, Clone)]
pub enum StatusStreamEvent {
    Changed(Box<StatusChangedEvent>),
    Lagged(u64),
}

/// The notification stream of a `status.subscribe` connection. Dropping it unsubscribes.
pub struct StatusStream {
    reader: BufReader<File>,
    _writer: File,
}

/// Every kind of event on a `status.subscribe` connection (v5 adds gates,
/// v6 background `group.check` progress).
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum SubscribeEvent {
    Status(StatusStreamEvent),
    Gate(Box<GateChangedEvent>),
    GroupCheck(Box<GroupCheckEvent>),
    /// A review delivery was recorded or changed state (`review.delivery`).
    Delivery(Box<ReviewDeliveryEvent>),
    /// v9 lifecycle events (subscribe with `lifecycle: true`).
    Terminal(Box<TerminalChangedEvent>),
    Task(Box<TaskChangedEvent>),
    TaskRemoving(Box<TaskRemovingEvent>),
}

impl StatusStream {
    /// Next status event; `Ok(None)` when the daemon closed the stream.
    /// Gate events are skipped (see [`StatusStream::next_any`]).
    pub fn next_event(&mut self) -> Result<Option<StatusStreamEvent>, ClientError> {
        loop {
            match self.next_any()? {
                None => return Ok(None),
                Some(SubscribeEvent::Status(e)) => return Ok(Some(e)),
                Some(_) => continue,
            }
        }
    }

    /// Next status or gate event; `Ok(None)` when the daemon closed the stream.
    pub fn next_any(&mut self) -> Result<Option<SubscribeEvent>, ClientError> {
        loop {
            let mut buf = String::new();
            if self.reader.read_line(&mut buf)? == 0 {
                return Ok(None);
            }
            let n: Notification = serde_json::from_str(&buf)
                .map_err(|e| ClientError::Protocol(format!("bad notification: {e}")))?;
            let bad =
                |e: serde_json::Error| ClientError::Protocol(format!("bad {}: {e}", n.method));
            match n.method.as_str() {
                NOTIFY_STATUS_CHANGED => {
                    let e: StatusChangedEvent =
                        serde_json::from_value(n.params.clone()).map_err(bad)?;
                    return Ok(Some(SubscribeEvent::Status(StatusStreamEvent::Changed(
                        Box::new(e),
                    ))));
                }
                NOTIFY_STATUS_LAGGED => {
                    let e: StatusLaggedEvent =
                        serde_json::from_value(n.params.clone()).map_err(bad)?;
                    return Ok(Some(SubscribeEvent::Status(StatusStreamEvent::Lagged(
                        e.missed,
                    ))));
                }
                NOTIFY_GATE_CHANGED => {
                    let e: GateChangedEvent =
                        serde_json::from_value(n.params.clone()).map_err(bad)?;
                    return Ok(Some(SubscribeEvent::Gate(Box::new(e))));
                }
                NOTIFY_GROUP_CHECK => {
                    let e: GroupCheckEvent =
                        serde_json::from_value(n.params.clone()).map_err(bad)?;
                    return Ok(Some(SubscribeEvent::GroupCheck(Box::new(e))));
                }
                NOTIFY_REVIEW_DELIVERY => {
                    let e: ReviewDeliveryEvent =
                        serde_json::from_value(n.params.clone()).map_err(bad)?;
                    return Ok(Some(SubscribeEvent::Delivery(Box::new(e))));
                }
                NOTIFY_TERMINAL_CHANGED => {
                    let e: TerminalChangedEvent =
                        serde_json::from_value(n.params.clone()).map_err(bad)?;
                    return Ok(Some(SubscribeEvent::Terminal(Box::new(e))));
                }
                NOTIFY_TASK_CHANGED => {
                    let e: TaskChangedEvent =
                        serde_json::from_value(n.params.clone()).map_err(bad)?;
                    return Ok(Some(SubscribeEvent::Task(Box::new(e))));
                }
                NOTIFY_TASK_REMOVING => {
                    let e: TaskRemovingEvent =
                        serde_json::from_value(n.params.clone()).map_err(bad)?;
                    return Ok(Some(SubscribeEvent::TaskRemoving(Box::new(e))));
                }
                _ => continue,
            }
        }
    }
}

impl Client {
    /// Subscribe to status changes: returns the current statuses and turns
    /// this connection into the event stream.
    pub fn subscribe_status(
        mut self,
        params: &StatusSubscribeParams,
    ) -> Result<(StatusSubscribeResult, StatusStream), ClientError> {
        let result = self.call::<StatusSubscribe>(params)?;
        Ok((
            result,
            StatusStream {
                reader: self.reader,
                _writer: self.writer,
            },
        ))
    }
}
