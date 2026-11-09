//! `pod-holder`: owns one terminal (ConPTY + Job Object) independently of the
//! daemon, so terminals survive `podd` stopping, crashing or restarting.
//!
//! Started detached by the daemon as `pod-holder --spec <file>`. It keeps an
//! authoritative VT state (to answer terminal queries and to give a
//! reconnecting daemon an exact snapshot) and serves the frame protocol in
//! [`crate::holder_proto`] on its own user-only named pipe. It exits only on
//! `Release` (after the child exited), which closes the Job Object.

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::io::AsyncWriteExt;
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
use tokio::sync::mpsc;

use crate::holder_proto::{ExitInfo, Frame, HolderHello, HolderRecord, HolderSpec, read_frame};
use crate::pty::Pty;
use crate::vt::{OSC7501_REPLY, Osc7501Query, Vt};
use crate::winsec::UserOnly;

pub use crate::holder_proto::HOLDER_PROTOCOL;

struct State {
    vt: Vt,
    exited: Option<ExitInfo>,
    killed: bool,
    /// The connected daemon's outgoing frame queue (encoded frames).
    sub: Option<mpsc::UnboundedSender<Vec<u8>>>,
}

struct Holder {
    spec: HolderSpec,
    pty: Arc<Pty>,
    state: Mutex<State>,
    exit_code: Mutex<Option<i32>>,
    last_output: Mutex<Instant>,
    input: std_mpsc::Sender<Vec<u8>>,
    log: Mutex<Option<File>>,
    sa: UserOnly,
}

impl Holder {
    fn log(&self, msg: &str) {
        if let Some(f) = self.log.lock().unwrap().as_mut() {
            let _ = writeln!(f, "[pod-holder {} {}] {msg}", self.spec.id, crate::now_ms());
        }
    }

    fn send(state: &mut State, frame: &Frame) {
        if let Some(tx) = &state.sub
            && tx.send(frame.encode()).is_err()
        {
            state.sub = None;
        }
    }

    fn hello(&self, state: &State) -> HolderHello {
        let (cols, rows) = state.vt.size();
        HolderHello {
            version: HOLDER_PROTOCOL,
            id: self.spec.id.clone(),
            name: self.spec.name.clone(),
            argv: self.spec.argv.clone(),
            cwd: self.spec.cwd.clone(),
            created_ms: self.spec.created_ms,
            holder_pid: std::process::id(),
            child_pid: self.pty.pid(),
            cols,
            rows,
            conpty: crate::pty::conpty_api().kind.clone(),
            parent_terminal: self.spec.parent_terminal.clone(),
            parent_task: self.spec.parent_task.clone(),
        }
    }

    fn record_path(&self) -> PathBuf {
        PathBuf::from(&self.spec.record_path)
    }
}

fn sibling(record: &Path, ext: &str) -> PathBuf {
    record.with_extension(ext)
}

/// Entry point of the `pod-holder` binary. Returns the process exit code.
pub fn main() -> i32 {
    let args: Vec<String> = std::env::args().collect();
    let spec_path = match args
        .iter()
        .position(|a| a == "--spec")
        .and_then(|i| args.get(i + 1))
    {
        Some(p) => PathBuf::from(p),
        None => {
            eprintln!("usage: pod-holder --spec <file>   (started by podd)");
            return 2;
        }
    };
    let spec: HolderSpec = match std::fs::read(&spec_path)
        .map_err(|e| e.to_string())
        .and_then(|b| serde_json::from_slice(&b).map_err(|e| e.to_string()))
    {
        Ok(s) => s,
        Err(e) => {
            eprintln!("pod-holder: cannot read spec {}: {e}", spec_path.display());
            return 2;
        }
    };
    let _ = std::fs::remove_file(&spec_path);
    let record = PathBuf::from(&spec.record_path);
    let fail = |msg: String| {
        let _ = std::fs::write(sibling(&record, "err"), &msg);
        1
    };

    let sa = match UserOnly::new() {
        Ok(sa) => sa,
        Err(e) => return fail(format!("security descriptor: {e}")),
    };
    let (pty, mut output) = match Pty::spawn(
        &spec.argv,
        Path::new(&spec.cwd),
        &spec.env,
        spec.cols,
        spec.rows,
    ) {
        Ok(p) => p,
        Err(e) => return fail(format!("{e}")),
    };
    let pty = Arc::new(pty);

    // All writes to the child's input go through one thread (never block tokio).
    let (input_tx, input_rx) = std_mpsc::channel::<Vec<u8>>();
    {
        let pty = pty.clone();
        std::thread::spawn(move || {
            for bytes in input_rx {
                if pty.write(&bytes).is_err() {
                    break;
                }
            }
        });
    }
    let reply_tx = Mutex::new(input_tx.clone());
    let reply: crate::vt::ReplySink = Arc::new(move |b: &[u8]| {
        let _ = reply_tx.lock().unwrap().send(b.to_vec());
    });

    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(sibling(&record, "log"))
        .ok();
    let holder = Arc::new(Holder {
        state: Mutex::new(State {
            vt: Vt::new(spec.cols, spec.rows, Some(reply)),
            exited: None,
            killed: false,
            sub: None,
        }),
        spec,
        pty,
        exit_code: Mutex::new(None),
        last_output: Mutex::new(Instant::now()),
        input: input_tx,
        log: Mutex::new(log),
        sa,
    });

    let rt = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            holder.pty.kill_tree(1);
            return fail(format!("runtime: {e}"));
        }
    };
    let first = {
        let _guard = rt.enter();
        // SAFETY: the attributes outlive the call.
        match unsafe {
            ServerOptions::new()
                .first_pipe_instance(true)
                .reject_remote_clients(true)
                .create_with_security_attributes_raw(&holder.spec.pipe, holder.sa.as_ptr())
        } {
            Ok(s) => s,
            Err(e) => {
                holder.pty.kill_tree(1);
                return fail(format!(
                    "cannot create holder pipe {}: {e}",
                    holder.spec.pipe
                ));
            }
        }
    };

    let rec = HolderRecord {
        id: holder.spec.id.clone(),
        name: holder.spec.name.clone(),
        pipe: holder.spec.pipe.clone(),
        holder_pid: std::process::id(),
        child_pid: holder.pty.pid(),
        created_ms: holder.spec.created_ms,
    };
    let tmp = sibling(&record, "json.tmp");
    if let Err(e) = std::fs::write(&tmp, serde_json::to_vec_pretty(&rec).unwrap())
        .and_then(|_| std::fs::rename(&tmp, &record))
    {
        holder.pty.kill_tree(1);
        return fail(format!("cannot write record {}: {e}", record.display()));
    }
    holder.log(&format!(
        "started {:?} pid {} on {}",
        holder.spec.argv,
        holder.pty.pid(),
        holder.spec.pipe
    ));

    // PTY output reader.
    {
        let h = holder.clone();
        std::thread::spawn(move || {
            let mut buf = vec![0u8; 64 * 1024];
            let mut osc7501 = Osc7501Query::default();
            loop {
                match output.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        *h.last_output.lock().unwrap() = Instant::now();
                        let chunk = &buf[..n];
                        let mut st = h.state.lock().unwrap();
                        let seq = st.vt.seq();
                        // Feed up to each OSC 7501 query and answer it right
                        // there, so replies leave in stream order (before the
                        // DA1 reply that Claude uses as its "no answer" fence).
                        let mut done = 0;
                        for end in osc7501.scan(chunk) {
                            st.vt.feed(&chunk[done..end]);
                            let _ = h.input.send(OSC7501_REPLY.to_vec());
                            done = end;
                        }
                        st.vt.feed(&chunk[done..]);
                        Holder::send(
                            &mut st,
                            &Frame::Output {
                                seq,
                                data: chunk.to_vec(),
                            },
                        );
                    }
                }
            }
            // End of output: the pseudo console was closed after the child exited.
            let mut st = h.state.lock().unwrap();
            let info = ExitInfo {
                seq: st.vt.seq(),
                exit_code: *h.exit_code.lock().unwrap(),
                killed: st.killed,
                exited_ms: crate::now_ms(),
            };
            st.exited = Some(info.clone());
            Holder::send(&mut st, &Frame::Exited(info.clone()));
            drop(st);
            h.log(&format!(
                "exited code {:?} killed {}",
                info.exit_code, info.killed
            ));
        });
    }

    // Child exit watcher: let the last output drain, then close the console
    // so the reader sees end-of-file.
    {
        let h = holder.clone();
        std::thread::spawn(move || {
            let code = h.pty.wait();
            *h.exit_code.lock().unwrap() = code;
            let start = Instant::now();
            while start.elapsed() < Duration::from_secs(3)
                && h.last_output.lock().unwrap().elapsed() < Duration::from_millis(150)
            {
                std::thread::sleep(Duration::from_millis(30));
            }
            h.pty.close_console();
        });
    }

    rt.block_on(accept_loop(holder, first));
    0
}

async fn accept_loop(holder: Arc<Holder>, mut server: NamedPipeServer) {
    loop {
        if let Err(e) = server.connect().await {
            holder.log(&format!("accept failed: {e}"));
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        // SAFETY: the attributes outlive the call.
        let next = match unsafe {
            ServerOptions::new()
                .reject_remote_clients(true)
                .create_with_security_attributes_raw(&holder.spec.pipe, holder.sa.as_ptr())
        } {
            Ok(n) => n,
            Err(e) => {
                holder.log(&format!("cannot create next pipe instance: {e}"));
                tokio::time::sleep(Duration::from_millis(500)).await;
                continue;
            }
        };
        let conn = std::mem::replace(&mut server, next);
        tokio::spawn(serve_daemon(holder.clone(), conn));
    }
}

async fn serve_daemon(h: Arc<Holder>, conn: NamedPipeServer) {
    let (mut rd, mut wr) = tokio::io::split(conn);
    let (tx, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
    {
        // Hello + snapshot + (exit) + subscription, atomically w.r.t. output.
        let mut st = h.state.lock().unwrap();
        let hello = h.hello(&st);
        let _ = tx.send(Frame::Hello(hello).encode());
        let _ = tx.send(
            Frame::Snapshot {
                seq: st.vt.seq(),
                data: st.vt.snapshot_ansi(usize::MAX),
            }
            .encode(),
        );
        if let Some(e) = &st.exited {
            let _ = tx.send(Frame::Exited(e.clone()).encode());
        }
        st.sub = Some(tx); // replaces (and so disconnects) any previous daemon
    }
    h.log("daemon connected");
    let writer = tokio::spawn(async move {
        while let Some(bytes) = rx.recv().await {
            if wr.write_all(&bytes).await.is_err() {
                break;
            }
        }
    });

    loop {
        let frame = match read_frame(&mut rd).await {
            Ok(Some(f)) => f,
            Ok(None) => break,
            Err(e) => {
                h.log(&format!("daemon read error: {e}"));
                break;
            }
        };
        match frame {
            Frame::Input(bytes) => {
                let _ = h.input.send(bytes);
            }
            Frame::Resize { cols, rows } => {
                let (cols, rows) = (cols.clamp(2, 1000), rows.clamp(2, 500));
                if let Err(e) = h.pty.resize(cols, rows) {
                    h.log(&format!("resize failed: {e}"));
                }
                let mut st = h.state.lock().unwrap();
                st.vt.resize(cols, rows);
                let seq = st.vt.seq();
                Holder::send(&mut st, &Frame::Resized { seq, cols, rows });
            }
            Frame::Kill => {
                h.state.lock().unwrap().killed = true;
                h.log("kill (job)");
                h.pty.kill_tree(1);
            }
            Frame::Release => {
                if h.state.lock().unwrap().exited.is_some() {
                    h.log("released");
                    let record = h.record_path();
                    let _ = std::fs::remove_file(&record);
                    let _ = std::fs::remove_file(sibling(&record, "log"));
                    std::process::exit(0);
                }
                h.log("release ignored: child still running");
            }
            Frame::DaemonHello(d) => {
                // Nothing in protocol 2 depends on the daemon's version yet;
                // recorded so a mismatch is visible in the holder log.
                h.log(&format!(
                    "daemon speaks holder protocol {} (this holder: {HOLDER_PROTOCOL})",
                    d.version
                ));
            }
            other => h.log(&format!("unexpected frame from daemon: {other:?}")),
        }
    }
    h.log("daemon disconnected");
    writer.abort();
}
