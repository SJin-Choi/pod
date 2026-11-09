//! Daemon-side terminal: a connection to the terminal's holder plus a VT
//! mirror fed from the holder's byte stream.
//!
//! The mirror answers `read`/`wait` and produces attach snapshots. Everything
//! that touches the stream (feeding the mirror, fanning out to attached
//! clients, taking a snapshot + subscribing) happens under one lock, so a
//! subscriber's first output byte is exactly the snapshot's `seq`.

use std::collections::VecDeque;
use std::io;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::net::windows::named_pipe::ClientOptions;
use tokio::sync::{Notify, mpsc};

use super::query_dedup::{Expectations, Filtered, QueryScanner};
use crate::holder_proto::{
    DaemonHello, ExitInfo, Frame, HOLDER_PROTOCOL, HolderHello, check_holder_version, read_frame,
};
use crate::protocol::{ReadMode, TerminalInfo, TerminalState};
use crate::vt::Vt;

const ERROR_PIPE_BUSY: i32 = 231;
/// How many recently sent texts are remembered for echo filtering.
const RECENT_SENDS: usize = 32;

/// Default per-client attach queue limit (bytes of output not yet written
/// to the client). Overridden by `podd --attach-queue-limit`.
pub const DEFAULT_ATTACH_QUEUE_LIMIT: usize = 16 * 1024 * 1024;
static ATTACH_QUEUE_LIMIT: AtomicUsize = AtomicUsize::new(DEFAULT_ATTACH_QUEUE_LIMIT);

pub fn set_attach_queue_limit(bytes: usize) {
    ATTACH_QUEUE_LIMIT.store(bytes.max(1), Ordering::Relaxed);
}

pub fn attach_queue_limit() -> usize {
    ATTACH_QUEUE_LIMIT.load(Ordering::Relaxed)
}

/// Why an attach subscription was dropped: the client fell behind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Lag {
    /// First output offset the client did not get.
    pub seq: u64,
    pub queued_bytes: u64,
}

/// One attached client's queue accounting (shared with its stream task).
#[derive(Default)]
pub struct SubShared {
    /// Output bytes sent into the channel and not yet taken by the stream task.
    pub queued: AtomicUsize,
    /// Set (once) when the subscription was dropped for lagging.
    pub lag: Mutex<Option<Lag>>,
}

struct Sub {
    tx: mpsc::UnboundedSender<StreamMsg>,
    shared: Arc<SubShared>,
}

/// The receiving side of an attach subscription.
pub struct AttachRx {
    pub rx: mpsc::UnboundedReceiver<StreamMsg>,
    pub shared: Arc<SubShared>,
}

impl AttachRx {
    /// Account a message taken from the queue.
    pub fn taken(&self, msg: &StreamMsg) {
        if let StreamMsg::Output { data, .. } = msg {
            self.shared.queued.fetch_sub(data.len(), Ordering::Relaxed);
        }
    }

    /// The lag that ended this subscription, if any.
    pub fn lag(&self) -> Option<Lag> {
        *self.shared.lag.lock().unwrap()
    }
}

/// What attached clients receive, in stream order.
#[derive(Clone, Debug)]
pub enum StreamMsg {
    Output { seq: u64, data: Arc<[u8]> },
    Resized { seq: u64, cols: u16, rows: u16 },
    Exited(ExitInfo),
}

struct TState {
    vt: Vt,
    exit: Option<ExitInfo>,
    lost: bool,
    subs: Vec<Sub>,
    /// Terminal queries seen in the output (for attach reply de-duplication).
    queries: QueryScanner,
}

impl TState {
    /// Send to every attached client. A client whose queue would pass the
    /// limit is dropped (its stream task reports `terminal.lagged`).
    fn broadcast(&mut self, msg: StreamMsg) {
        let limit = attach_queue_limit();
        self.subs.retain(|s| {
            if let StreamMsg::Output { seq, data } = &msg {
                let queued = s.shared.queued.load(Ordering::Relaxed);
                if queued + data.len() > limit {
                    *s.shared.lag.lock().unwrap() = Some(Lag {
                        seq: *seq,
                        queued_bytes: queued as u64,
                    });
                    return false; // dropping the sender ends the stream
                }
                s.shared.queued.fetch_add(data.len(), Ordering::Relaxed);
            }
            s.tx.send(msg.clone()).is_ok()
        });
    }
}

/// The JSON of a parsed hello (for the incompatible path).
fn foreign_hello(h: &HolderHello) -> serde_json::Value {
    serde_json::to_value(h).unwrap_or_default()
}

pub struct Terminal {
    pub id: String,
    pub name: Option<String>,
    pub argv: Vec<String>,
    pub cwd: String,
    pub created_ms: u64,
    pub pid: u32,
    pub holder_pid: u32,
    pub pipe: String,
    pub conpty: String,
    /// Parent terminal / task (v7), see `TerminalInfo.parent_terminal`.
    pub parent_terminal: Option<String>,
    pub parent_task: Option<String>,
    /// Holder protocol of this terminal's holder.
    pub holder_protocol: u32,
    /// Set when the holder speaks an unsupported protocol (v7).
    pub incompatible: Option<String>,
    /// Fired on every output batch, resize and exit.
    pub changed: Notify,
    /// Agent status engine (fed from the same stream, see `status.rs`).
    pub status: super::status::TermStatus,
    state: Mutex<TState>,
    to_holder: mpsc::UnboundedSender<Frame>,
    recent_sends: Mutex<VecDeque<String>>,
    /// Unix ms of the last output batch (0 = none since connect).
    last_output_ms: AtomicU64,
    /// Replies to terminal queries the attached clients are expected to send.
    expect: Mutex<Expectations>,
}

impl Terminal {
    /// Connect to a holder pipe and start mirroring it. `NotFound` means no
    /// holder serves that pipe (not started yet, or gone).
    pub async fn connect(pipe: &str) -> io::Result<Arc<Terminal>> {
        let client = loop {
            match ClientOptions::new().open(pipe) {
                Ok(c) => break c,
                Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY) => {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                Err(e) => return Err(e),
            }
        };
        let (mut rd, mut wr) = tokio::io::split(client);
        let handshake = async {
            let hello = match read_frame(&mut rd).await? {
                Some(Frame::Hello(h)) => match check_holder_version(h.version) {
                    Ok(()) => h,
                    Err(why) => return Ok(Err((foreign_hello(&h), why))),
                },
                Some(Frame::ForeignHello(v)) => {
                    let version = v.get("version").and_then(|x| x.as_u64());
                    let why = match version {
                        Some(n) => check_holder_version(n as u32)
                            .err()
                            .unwrap_or_else(|| format!("unreadable hello for holder protocol {n}")),
                        None => "holder hello without a version".to_string(),
                    };
                    return Ok(Err((v, why)));
                }
                other => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("expected hello, got {other:?}"),
                    ));
                }
            };
            let (seq, snapshot) = match read_frame(&mut rd).await? {
                Some(Frame::Snapshot { seq, data }) => (seq, data),
                other => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("expected snapshot, got {other:?}"),
                    ));
                }
            };
            Ok(Ok((hello, seq, snapshot)))
        };
        let handshake = tokio::time::timeout(Duration::from_secs(10), handshake)
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "holder handshake timed out"))??;
        let (hello, seq, snapshot) = match handshake {
            Ok(x) => x,
            // Dropping the connection leaves the holder (and its child) running.
            Err((hello, why)) => return Terminal::incompatible(pipe, &hello, why),
        };

        let (tx, mut rx) = mpsc::unbounded_channel::<Frame>();
        if hello.version >= 2 {
            // Version exchange; holders < 2 would reject the unknown frame kind.
            let _ = tx.send(Frame::DaemonHello(DaemonHello {
                version: HOLDER_PROTOCOL,
            }));
        }
        tokio::spawn(async move {
            while let Some(f) = rx.recv().await {
                if wr.write_all(&f.encode()).await.is_err() {
                    break;
                }
            }
        });
        let status =
            super::status::TermStatus::new(&hello.id, &hello.argv, hello.child_pid, &snapshot);
        let term = Arc::new(Terminal {
            status,
            id: hello.id,
            name: hello.name,
            argv: hello.argv,
            cwd: hello.cwd,
            created_ms: hello.created_ms,
            pid: hello.child_pid,
            holder_pid: hello.holder_pid,
            pipe: pipe.to_string(),
            conpty: hello.conpty,
            parent_terminal: hello.parent_terminal,
            parent_task: hello.parent_task,
            holder_protocol: hello.version,
            incompatible: None,
            changed: Notify::new(),
            state: Mutex::new(TState {
                vt: Vt::from_snapshot(hello.cols, hello.rows, seq, &snapshot),
                exit: None,
                lost: false,
                subs: Vec::new(),
                queries: QueryScanner::default(),
            }),
            to_holder: tx,
            recent_sends: Mutex::new(VecDeque::new()),
            last_output_ms: AtomicU64::new(0),
            expect: Mutex::new(Expectations::default()),
        });
        let t = term.clone();
        tokio::spawn(async move {
            loop {
                match read_frame(&mut rd).await {
                    Ok(Some(f)) => t.on_frame(f),
                    Ok(None) => break,
                    Err(e) => {
                        super::log(&format!("{}: holder stream error: {e}", t.id));
                        break;
                    }
                }
            }
            t.on_holder_gone();
        });
        Ok(term)
    }

    /// A terminal whose holder speaks a protocol this daemon cannot drive:
    /// listed as exited with `incompatible_holder`; nothing is sent to it.
    fn incompatible(
        pipe: &str,
        hello: &serde_json::Value,
        why: String,
    ) -> io::Result<Arc<Terminal>> {
        let str_of = |k: &str| hello.get(k).and_then(|v| v.as_str()).map(str::to_string);
        let u64_of = |k: &str| hello.get(k).and_then(|v| v.as_u64());
        let Some(id) = str_of("id") else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("incompatible holder without an id ({why})"),
            ));
        };
        let version = u64_of("version").unwrap_or(0) as u32;
        let argv: Vec<String> = hello
            .get("argv")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default();
        let cols = u64_of("cols").unwrap_or(120).clamp(2, 1000) as u16;
        let rows = u64_of("rows").unwrap_or(30).clamp(2, 500) as u16;
        let child_pid = u64_of("child_pid").unwrap_or(0) as u32;
        let reason = format!("incompatible holder: {why}");
        super::log(&format!("{id}: {reason} (pipe {pipe})"));
        let mut vt = Vt::new(cols, rows, None);
        vt.feed(
            format!(
                "[pod: {reason}. The terminal keeps running in its holder, but this podd \
                 cannot show or drive it. Use the matching podd, or `pod rm --force {id}` \
                 to end it.]\r\n"
            )
            .as_bytes(),
        );
        let (tx, _) = mpsc::unbounded_channel::<Frame>();
        let status = super::status::TermStatus::new(&id, &argv, child_pid, b"");
        status.on_exit();
        Ok(Arc::new(Terminal {
            status,
            name: str_of("name"),
            argv,
            cwd: str_of("cwd").unwrap_or_default(),
            created_ms: u64_of("created_ms").unwrap_or(0),
            pid: child_pid,
            holder_pid: u64_of("holder_pid").unwrap_or(0) as u32,
            pipe: pipe.to_string(),
            conpty: str_of("conpty").unwrap_or_default(),
            parent_terminal: str_of("parent_terminal"),
            parent_task: str_of("parent_task"),
            holder_protocol: version,
            incompatible: Some(reason),
            changed: Notify::new(),
            state: Mutex::new(TState {
                vt,
                exit: Some(ExitInfo {
                    seq: 0,
                    exit_code: None,
                    killed: false,
                    exited_ms: crate::now_ms(),
                }),
                lost: false,
                subs: Vec::new(),
                queries: QueryScanner::default(),
            }),
            to_holder: tx,
            recent_sends: Mutex::new(VecDeque::new()),
            last_output_ms: AtomicU64::new(0),
            expect: Mutex::new(Expectations::default()),
            id,
        }))
    }

    fn on_frame(&self, frame: Frame) {
        let mut exited = false;
        let mut st = self.state.lock().unwrap();
        match frame {
            Frame::Output { seq, data } => {
                if seq != st.vt.seq() {
                    super::log(&format!(
                        "{}: output gap: expected seq {}, got {seq}",
                        self.id,
                        st.vt.seq()
                    ));
                }
                st.vt.feed(&data);
                // Terminal queries: each attached client may answer; record
                // them so duplicate and late replies are dropped.
                let clients = st.subs.len();
                if clients > 0 {
                    let now = crate::now_ms();
                    let mut expect = self.expect.lock().unwrap();
                    st.queries.scan(&data, |r| expect.expect(r, clients, now));
                } else {
                    st.queries.scan(&data, |_| {});
                }
                self.last_output_ms
                    .store(crate::now_ms(), Ordering::Relaxed);
                self.status.on_output(&data);
                st.broadcast(StreamMsg::Output {
                    seq,
                    data: data.into(),
                });
            }
            Frame::Resized { seq, cols, rows } => {
                st.vt.resize(cols, rows);
                st.broadcast(StreamMsg::Resized { seq, cols, rows });
            }
            Frame::Exited(info) => {
                st.exit = Some(info.clone());
                st.broadcast(StreamMsg::Exited(info));
                self.status.on_exit();
                exited = true;
            }
            other => super::log(&format!(
                "{}: unexpected frame from holder: {other:?}",
                self.id
            )),
        }
        drop(st);
        self.changed.notify_waiters();
        if exited {
            self.publish_exited();
        }
    }

    /// v9 `terminal.changed exited` (no stream lock held).
    fn publish_exited(&self) {
        super::status::publish_terminal(
            &self.id,
            crate::protocol::TerminalChange::Exited,
            Some(self.info()),
        );
    }

    /// The holder connection ended. If it never reported an exit, it died.
    fn on_holder_gone(&self) {
        let mut st = self.state.lock().unwrap();
        let lost = st.exit.is_none();
        if lost {
            let info = ExitInfo {
                seq: st.vt.seq(),
                exit_code: None,
                killed: false,
                exited_ms: crate::now_ms(),
            };
            st.exit = Some(info.clone());
            st.lost = true;
            st.broadcast(StreamMsg::Exited(info));
            self.status.on_exit();
        }
        st.subs.clear();
        drop(st);
        self.changed.notify_waiters();
        if lost {
            self.publish_exited();
        }
    }

    /// Unix ms of the last output (0 = none since the daemon connected).
    pub fn last_output_ms(&self) -> u64 {
        self.last_output_ms.load(Ordering::Relaxed)
    }

    pub fn is_running(&self) -> bool {
        self.state.lock().unwrap().exit.is_none()
    }

    pub fn size(&self) -> (u16, u16) {
        self.state.lock().unwrap().vt.size()
    }

    pub fn info(&self) -> TerminalInfo {
        let mut st = self.state.lock().unwrap();
        st.subs.retain(|s| !s.tx.is_closed());
        let (cols, rows) = st.vt.size();
        TerminalInfo {
            id: self.id.clone(),
            name: self.name.clone(),
            argv: self.argv.clone(),
            cwd: self.cwd.clone(),
            state: if st.exit.is_some() {
                TerminalState::Exited
            } else {
                TerminalState::Running
            },
            exit_code: st.exit.as_ref().and_then(|e| e.exit_code),
            pid: Some(self.pid),
            holder_pid: Some(self.holder_pid),
            cols,
            rows,
            created_ms: self.created_ms,
            exited_ms: st.exit.as_ref().map(|e| e.exited_ms),
            killed: st.exit.as_ref().is_some_and(|e| e.killed),
            lost: st.lost,
            attached: st.subs.len(),
            conpty: Some(self.conpty.clone()),
            status: Some(self.status.info()),
            parent_terminal: self.parent_terminal.clone(),
            parent_task: self.parent_task.clone(),
            incompatible_holder: self.incompatible.clone(),
        }
    }

    pub fn send_to_holder(&self, frame: Frame) -> bool {
        self.to_holder.send(frame).is_ok()
    }

    /// Type into the terminal. The status engine sees it too: answering a
    /// hook permission prompt ends `blocked` (docs/12 B2).
    pub fn input(&self, bytes: Vec<u8>) -> bool {
        self.status.on_input(&bytes);
        self.send_to_holder(Frame::Input(bytes))
    }

    /// Raw input from an attach client (`terminal.input`): duplicate and late
    /// replies to terminal queries are removed first (see `query_dedup`).
    /// Returns (bytes forwarded, replies dropped).
    pub fn attach_input(&self, bytes: &[u8]) -> (usize, Filtered) {
        let (bytes, dropped) = self
            .expect
            .lock()
            .unwrap()
            .filter_input(bytes, crate::now_ms());
        let n = bytes.len();
        if n > 0 {
            self.input(bytes);
        }
        (n, dropped)
    }

    /// Remember sent text so its echo does not satisfy `output_contains`.
    pub fn remember_sent(&self, text: &str) {
        let mut recent = self.recent_sends.lock().unwrap();
        for line in text
            .split(['\r', '\n'])
            .map(str::trim)
            .filter(|l| !l.is_empty())
        {
            recent.push_back(line.to_string());
            while recent.len() > RECENT_SENDS {
                recent.pop_front();
            }
        }
    }

    pub fn app_cursor(&self) -> bool {
        self.state.lock().unwrap().vt.app_cursor()
    }

    pub fn bracketed_paste(&self) -> bool {
        self.state.lock().unwrap().vt.bracketed_paste()
    }

    pub fn read_lines(&self, mode: ReadMode) -> (Vec<String>, (u16, u16)) {
        self.state.lock().unwrap().vt.read_lines(mode)
    }

    /// Whether scrollback + screen contains `needle` outside the echo of text
    /// sent with `terminal.send`: occurrences of the needle that lie inside a
    /// recently sent line (e.g. `PS> echo pod-ok`) are blanked out first.
    pub fn output_contains(&self, needle: &str) -> bool {
        let echoes: Vec<String> = self
            .recent_sends
            .lock()
            .unwrap()
            .iter()
            .filter(|t| t.contains(needle))
            .cloned()
            .collect();
        let (lines, _) = self.read_lines(ReadMode::History);
        let strip = |l: &str| {
            echoes
                .iter()
                .fold(l.to_string(), |acc, t| acc.replace(t.as_str(), "\u{0}"))
        };
        if needle.contains('\n') {
            return strip(&lines.join("\n")).contains(needle);
        }
        lines
            .iter()
            .any(|l| l.contains(needle) && strip(l).contains(needle))
    }

    /// Snapshot + subscription, atomically. Returns (snapshot ANSI, seq, receiver).
    pub fn attach(&self, scrollback: usize) -> (Vec<u8>, u64, AttachRx) {
        let (tx, rx) = mpsc::unbounded_channel();
        let shared = Arc::new(SubShared::default());
        let mut st = self.state.lock().unwrap();
        let snapshot = st.vt.snapshot_ansi(scrollback);
        let seq = st.vt.seq();
        match &st.exit {
            Some(e) => {
                let _ = tx.send(StreamMsg::Exited(e.clone()));
            }
            None => st.subs.push(Sub {
                tx,
                shared: shared.clone(),
            }),
        }
        (snapshot, seq, AttachRx { rx, shared })
    }
}
