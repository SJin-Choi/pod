//! `pod-hook` process behavior: always `{}` + exit 0, fast, with or without Pod.

use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_pod-hook");

fn base_cmd() -> Command {
    let mut c = Command::new(BIN);
    c.env_remove("POD_TERMINAL_ID")
        .env_remove("POD_SOCKET")
        .env_remove("POD_HOOK_TOKEN");
    c
}

/// Budget checks tolerate one slow start on a loaded machine (a full
/// workspace test run, Defender scanning the fresh exe): if the first run is
/// over `limit`, run again and require that run to be within it. Output and
/// exit code are checked on every run; a run over 10 s always fails.
fn run_within(
    make: impl Fn() -> Command,
    stdin: &[u8],
    close_stdin: bool,
    limit: Duration,
) -> (String, i32, Duration) {
    let mut last = None;
    for attempt in 0..2 {
        let (out, code, t) = run(make(), stdin, close_stdin);
        assert_eq!((out.trim(), code), ("{}", 0), "attempt {attempt}");
        assert!(t < Duration::from_secs(10), "attempt {attempt} took {t:?}");
        if t < limit {
            return (out, code, t);
        }
        eprintln!("pod-hook took {t:?} (limit {limit:?}); retrying once");
        last = Some(t);
    }
    panic!("pod-hook took {last:?} twice in a row (limit {limit:?})");
}

fn run(mut cmd: Command, stdin: &[u8], close_stdin: bool) -> (String, i32, Duration) {
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let start = Instant::now();
    let mut child = cmd.spawn().expect("spawn pod-hook");
    let mut input = child.stdin.take().unwrap();
    let _ = input.write_all(stdin);
    let keep = if close_stdin {
        drop(input);
        None
    } else {
        Some(input)
    };
    let out = child.wait_with_output().unwrap();
    let elapsed = start.elapsed();
    drop(keep);
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        out.status.code().unwrap_or(-1),
        elapsed,
    )
}

#[test]
fn no_env_prints_empty_object_and_exits_fast() {
    run_within(
        || {
            let mut c = base_cmd();
            c.args(["claude", "PreToolUse"]);
            c
        },
        br#"{"hook_event_name":"PreToolUse","tool_name":"Bash"}"#,
        true,
        Duration::from_millis(1500),
    );
}

#[test]
fn no_env_does_not_wait_for_stdin() {
    // stdin left open: without POD env the hook must not read it at all
    run_within(
        || {
            let mut c = base_cmd();
            c.args(["codex", "Stop"]);
            c
        },
        b"{",
        false,
        Duration::from_millis(1500),
    );
}

/// Many hooks at once (an agent firing several events while the machine is
/// busy): every one still answers `{}`, exits 0, and none hangs at exit.
#[test]
fn parallel_burst_all_answer_and_exit() {
    let handles: Vec<_> = (0..12)
        .map(|i| {
            std::thread::spawn(move || {
                let mut c = base_cmd();
                c.args(["claude", "Stop"])
                    .env("POD_TERMINAL_ID", "t1")
                    .env("POD_SOCKET", missing_pipe());
                // Half of them never close stdin.
                run(c, br#"{"hook_event_name":"Stop"}"#, i % 2 == 0)
            })
        })
        .collect();
    for h in handles {
        let (out, code, t) = h.join().unwrap();
        assert_eq!((out.trim(), code), ("{}", 0));
        assert!(t < Duration::from_secs(10), "took {t:?}");
    }
}

#[test]
fn bad_args_still_harmless() {
    for args in [&[][..], &["gemini", "Stop"][..], &["claude"][..]] {
        let mut c = base_cmd();
        c.args(args)
            .env("POD_TERMINAL_ID", "t1")
            .env("POD_SOCKET", missing_pipe());
        let (out, code, _) = run(c, b"not json", true);
        assert_eq!((out.trim(), code), ("{}", 0), "{args:?}");
    }
}

fn missing_pipe() -> String {
    if cfg!(windows) {
        format!(r"\\.\pipe\pod-hook-test-missing-{}", std::process::id())
    } else {
        format!("/tmp/pod-hook-test-missing-{}.sock", std::process::id())
    }
}

#[test]
fn env_set_but_no_pipe_prints_empty_object_and_exits_fast() {
    run_within(
        || {
            let mut c = base_cmd();
            c.args(["claude", "Stop"])
                .env("POD_TERMINAL_ID", "t1")
                .env("POD_SOCKET", missing_pipe());
            c
        },
        br#"{"hook_event_name":"Stop"}"#,
        true,
        Duration::from_millis(2000),
    );
}

#[test]
fn env_set_and_stdin_never_closed_still_exits_within_budget() {
    run_within(
        || {
            let mut c = base_cmd();
            c.args(["claude", "Stop"])
                .env("POD_TERMINAL_ID", "t1")
                .env("POD_SOCKET", missing_pipe());
            c
        },
        br#"{"partial":"#,
        false,
        Duration::from_millis(2500),
    );
}

#[test]
fn install_requires_dry_run_and_prints_blocks() {
    let mut c = base_cmd();
    c.args(["install"]);
    let (_, code, _) = run(c, b"", true);
    assert_eq!(code, 2, "real install is refused");

    let mut c = base_cmd();
    c.args([
        "install",
        "--dry-run",
        "--block-only",
        "--exe",
        "C:/pod/pod-hook.exe",
    ]);
    let (out, code, _) = run(c, b"", true);
    assert_eq!(code, 0);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(
        v["claude"]["hooks"]["Stop"][0]["hooks"][0]["command"],
        "C:/pod/pod-hook.exe claude Stop"
    );
    assert_eq!(
        v["codex"]["hooks"]["Interrupt"][0]["hooks"][0]["command"],
        "C:/pod/pod-hook.exe codex Interrupt"
    );
}

#[test]
fn install_dry_run_profile_writes_nothing() {
    let dir = std::env::temp_dir().join(format!("pod-hook-dry-{}", std::process::id()));
    let pc = dir.join("claude");
    let px = dir.join("codex");
    let mut c = base_cmd();
    c.args([
        "install",
        "--dry-run",
        "--exe",
        "C:/pod/pod-hook.exe",
        "--claude-config-dir",
    ])
    .arg(&pc)
    .arg("--codex-home")
    .arg(&px);
    let (out, code, _) = run(c, b"", true);
    assert_eq!(code, 0, "{out}");
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["scope"]["scope"], "profile");
    assert_eq!(v["files"].as_array().unwrap().len(), 2);
    assert!(!dir.exists(), "dry run must not create anything");
}

#[cfg(windows)]
mod pipe {
    use super::*;
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{PIPE_ACCESS_INBOUND, ReadFile};
    use windows_sys::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE, PIPE_WAIT,
    };

    /// A one-shot named-pipe server that returns the first line it receives.
    fn serve_once(name: &str) -> std::thread::JoinHandle<String> {
        let wide: Vec<u16> = std::ffi::OsStr::new(name)
            .encode_wide()
            .chain(Some(0))
            .collect();
        let h = unsafe {
            CreateNamedPipeW(
                wide.as_ptr(),
                PIPE_ACCESS_INBOUND,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                1,
                0,
                65536,
                0,
                std::ptr::null(),
            )
        };
        assert_ne!(h, INVALID_HANDLE_VALUE, "CreateNamedPipeW");
        let h = h as usize;
        std::thread::spawn(move || {
            let h = h as _;
            unsafe { ConnectNamedPipe(h, std::ptr::null_mut()) };
            let mut data = Vec::new();
            let mut buf = [0u8; 4096];
            loop {
                let mut n = 0u32;
                let ok = unsafe {
                    ReadFile(
                        h,
                        buf.as_mut_ptr(),
                        buf.len() as u32,
                        &mut n,
                        std::ptr::null_mut(),
                    )
                };
                if ok == 0 || n == 0 {
                    break;
                }
                data.extend_from_slice(&buf[..n as usize]);
                if data.contains(&b'\n') {
                    break;
                }
            }
            unsafe { CloseHandle(h) };
            String::from_utf8(data).unwrap()
        })
    }

    #[test]
    fn delivers_one_json_line_to_the_pipe() {
        let name = format!(r"\\.\pipe\pod-hook-test-{}", std::process::id());
        let server = serve_once(&name);
        let mut c = base_cmd();
        c.args(["claude", "PermissionRequest"])
            .env("POD_TERMINAL_ID", "term-42")
            .env("POD_SOCKET", &name)
            .env("POD_HOOK_TOKEN", "secret");
        let payload =
            br#"{"session_id":"s1","hook_event_name":"PermissionRequest","tool_name":"Bash"}"#;
        let (out, code, t) = run(c, payload, true);
        assert_eq!((out.trim(), code), ("{}", 0));
        // Delivery is checked below; the time only has to stay sane under load.
        assert!(t < Duration::from_secs(10), "took {t:?}");
        let line = server.join().unwrap();
        assert!(line.ends_with('\n'));
        let msg = pod_status::HookMessage::from_line(&line).unwrap();
        assert_eq!(msg.terminal_id, "term-42");
        assert_eq!(msg.token.as_deref(), Some("secret"));
        assert_eq!(msg.agent, pod_status::HookAgent::Claude);
        assert_eq!(msg.event, "PermissionRequest");
        assert_eq!(msg.payload["tool_name"], "Bash");
        // and podd's engine understands it
        let mut t = pod_status::TerminalStatus::new(0);
        t.on_hook(&msg, 1);
        assert_eq!(
            t.state(),
            pod_status::AgentState::Blocked(pod_status::BlockedKind::Permission)
        );
    }
}

/// No console window, however the agent spawns us: the exe is GUI-subsystem
/// (IMAGE_SUBSYSTEM_WINDOWS_GUI = 2), so Windows never allocates a console.
#[cfg(windows)]
#[test]
fn windows_exe_is_gui_subsystem() {
    let b = std::fs::read(BIN).unwrap();
    let pe = u32::from_le_bytes(b[0x3c..0x40].try_into().unwrap()) as usize;
    assert_eq!(&b[pe..pe + 4], b"PE\0\0");
    let optional_header = pe + 24;
    let subsystem = u16::from_le_bytes(
        b[optional_header + 68..optional_header + 70]
            .try_into()
            .unwrap(),
    );
    assert_eq!(subsystem, 2, "pod-hook must be a GUI-subsystem exe");
}

/// Worst case spawn: from a detached process (no console to inherit). A
/// console exe would get a new, visible console window here; pod-hook still
/// answers `{}` over the pipes and exits within the budget with a dead daemon.
#[cfg(windows)]
#[test]
fn detached_spawn_still_answers_on_pipes() {
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    run_within(
        || {
            let mut c = base_cmd();
            c.args(["claude", "Stop"])
                .env("POD_TERMINAL_ID", "t1")
                .env(
                    "POD_SOCKET",
                    r"\\.\pipe\pod-hook-test-no-such-daemon-detached",
                )
                .env("POD_HOOK_TOKEN", "x")
                .creation_flags(DETACHED_PROCESS);
            c
        },
        br#"{"hook_event_name":"Stop"}"#,
        true,
        Duration::from_millis(1500),
    );
}
