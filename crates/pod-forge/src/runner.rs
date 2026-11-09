//! Process spawning for `git` and `gh`.
//!
//! Windows rules (same as podd's `git.rs`): `CREATE_NO_WINDOW` (podd runs
//! detached, a console child would open a window), stdin from a string or
//! null, no interactive prompts (`GH_PROMPT_DISABLED`, `GIT_TERMINAL_PROMPT=0`),
//! no colors or update notices, inherited `GIT_DIR`-style variables removed.
//! A run longer than its timeout is killed and reported as
//! [`ForgeErrorKind::Timeout`].

use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::error::{ForgeError, ForgeErrorKind};

/// The result of one finished process.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RunOutput {
    /// `None` when killed by a signal (not on Windows) or unknown.
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl RunOutput {
    pub fn ok(&self) -> bool {
        self.code == Some(0)
    }
}

/// One command to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invocation {
    /// `git` or `gh` (resolved by the runner).
    pub program: String,
    pub args: Vec<String>,
    pub stdin: Option<String>,
    pub timeout: Duration,
}

impl Invocation {
    pub fn new(program: &str, args: &[&str], timeout: Duration) -> Self {
        Self { program: program.into(), args: args.iter().map(|s| s.to_string()).collect(), stdin: None, timeout }
    }

    /// `program arg arg…` for logs and recorded fixtures.
    pub fn line(&self) -> String {
        std::iter::once(self.program.as_str()).chain(self.args.iter().map(String::as_str)).collect::<Vec<_>>().join(" ")
    }
}

/// Runs invocations. [`ProcessRunner`] spawns real processes; tests use a
/// recorded runner that answers from fixtures.
pub trait Runner: Send + Sync {
    /// Runs to completion. `Err` only when the process could not run at all
    /// (missing tool) or timed out; a non-zero exit is an `Ok` output.
    fn run(&self, dir: &Path, inv: &Invocation) -> Result<RunOutput, ForgeError>;
}

/// Environment overrides for the executables.
pub const ENV_GH: &str = "POD_GH";
pub const ENV_GIT: &str = "POD_GIT";

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Variables that would point git somewhere other than the worktree.
const GIT_ENV_OVERRIDES: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_COMMON_DIR",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_NAMESPACE",
    "GIT_CEILING_DIRECTORIES",
    "GIT_PREFIX",
];

#[derive(Debug, Clone, Default)]
pub struct ProcessRunner;

impl ProcessRunner {
    fn command(&self, dir: &Path, inv: &Invocation) -> Command {
        let exe = match inv.program.as_str() {
            "gh" => std::env::var_os(ENV_GH).unwrap_or_else(|| "gh".into()),
            "git" => std::env::var_os(ENV_GIT).unwrap_or_else(|| "git".into()),
            other => other.into(),
        };
        let mut cmd = Command::new(exe);
        if inv.program == "git" {
            cmd.arg("-C").arg(dir).args(["-c", "core.longpaths=true", "-c", "core.quotepath=false"]);
        }
        cmd.args(&inv.args)
            .current_dir(dir)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GCM_INTERACTIVE", "never")
            .env("GH_PROMPT_DISABLED", "1")
            .env("GH_NO_UPDATE_NOTIFIER", "1")
            .env("GH_SPINNER_DISABLED", "1")
            .env("NO_COLOR", "1")
            .env("CLICOLOR", "0")
            .env("LC_ALL", "C")
            .stdin(if inv.stdin.is_some() { Stdio::piped() } else { Stdio::null() })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for v in GIT_ENV_OVERRIDES {
            cmd.env_remove(v);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        #[cfg(not(windows))]
        let _ = CREATE_NO_WINDOW;
        cmd
    }
}

impl Runner for ProcessRunner {
    fn run(&self, dir: &Path, inv: &Invocation) -> Result<RunOutput, ForgeError> {
        let mut child = self.command(dir, inv).spawn().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                ForgeError::new(ForgeErrorKind::ToolMissing, format!("{} is not installed or not on PATH", inv.program))
            } else {
                ForgeError::failed(format!("cannot run {}: {e}", inv.program))
            }
        })?;
        if let (Some(text), Some(mut pipe)) = (inv.stdin.clone(), child.stdin.take()) {
            // A writer thread: a large body must not block on a full pipe.
            std::thread::spawn(move || {
                let _ = pipe.write_all(text.as_bytes());
            });
        }
        let drain = |r: Option<Box<dyn Read + Send>>| {
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                if let Some(mut r) = r {
                    let _ = r.read_to_end(&mut buf);
                }
                String::from_utf8_lossy(&buf).into_owned()
            })
        };
        let out = drain(child.stdout.take().map(|s| Box::new(s) as Box<dyn Read + Send>));
        let err = drain(child.stderr.take().map(|s| Box::new(s) as Box<dyn Read + Send>));
        let start = Instant::now();
        let status = loop {
            match child.try_wait() {
                Ok(Some(st)) => break st,
                Ok(None) if start.elapsed() >= inv.timeout => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(ForgeError::new(
                        ForgeErrorKind::Timeout,
                        format!("{} {} did not finish within {} s", inv.program, inv.args.first().map(String::as_str).unwrap_or(""), inv.timeout.as_secs()),
                    ));
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(20)),
                Err(e) => return Err(ForgeError::failed(format!("waiting for {}: {e}", inv.program))),
            }
        };
        Ok(RunOutput { code: status.code(), stdout: out.join().unwrap_or_default(), stderr: err.join().unwrap_or_default() })
    }
}

/// Runs `inv` and turns a non-zero exit into a classified [`ForgeError`].
pub fn run_ok(runner: &dyn Runner, dir: &Path, inv: &Invocation) -> Result<RunOutput, ForgeError> {
    let out = runner.run(dir, inv)?;
    if out.ok() { Ok(out) } else { Err(ForgeError::classify(&inv.program, out.code, &out.stderr)) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn here() -> std::path::PathBuf {
        std::env::temp_dir()
    }

    #[test]
    fn runs_and_captures_stdin_stdout_stderr() {
        // `git hash-object --stdin` echoes a hash of stdin: stdin and stdout work.
        let mut inv = Invocation::new("git", &["hash-object", "--stdin"], Duration::from_secs(20));
        inv.stdin = Some("hello\n".into());
        let out = ProcessRunner.run(&here(), &inv).unwrap();
        assert!(out.ok(), "{out:?}");
        assert_eq!(out.stdout.trim(), "ce013625030ba8dba906f756967f9e9ca394464a");
        let bad = ProcessRunner.run(&here(), &Invocation::new("git", &["no-such-subcommand-xyz"], Duration::from_secs(20))).unwrap();
        assert!(!bad.ok());
        assert!(bad.stderr.contains("no-such-subcommand-xyz"), "{bad:?}");
    }

    #[test]
    fn missing_tool_and_timeout() {
        let e = ProcessRunner.run(&here(), &Invocation::new("pod-no-such-tool-xyz", &[], Duration::from_secs(5))).unwrap_err();
        assert_eq!(e.kind, ForgeErrorKind::ToolMissing);
        // `ping -n 30` runs ~29 s; the 300 ms limit kills it.
        let t = Instant::now();
        let e = ProcessRunner.run(&here(), &Invocation::new("ping", &["-n", "30", "127.0.0.1"], Duration::from_millis(300))).unwrap_err();
        assert_eq!(e.kind, ForgeErrorKind::Timeout);
        assert!(t.elapsed() < Duration::from_secs(10), "killed promptly: {:?}", t.elapsed());
    }
}
