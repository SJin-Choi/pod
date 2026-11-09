//! Thin wrapper over the `git` CLI for worktree-based tasks.
//!
//! Everything goes through the `git` executable (not libgit2) so behaviour
//! matches what the user and the agents see in their shells. Output is read
//! in porcelain / `-z` form only. Every invocation adds
//! `-c core.longpaths=true -c core.quotepath=false`, clears inherited
//! `GIT_DIR`-style variables, never prompts and never opens a console window
//! (the daemon runs detached, so a console child would get a fresh window).

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::protocol::{CommitInfo, DiffFile, TaskGit};

/// Environment override for the git executable.
pub const ENV_GIT: &str = "POD_GIT";

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
/// Untracked files larger than this are reported without line counts.
const MAX_COUNT_BYTES: u64 = 8 * 1024 * 1024;

/// Variables that would point git somewhere other than `-C <dir>`.
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

pub struct Output {
    pub ok: bool,
    pub stdout: String,
    pub stderr: String,
}

fn command(dir: &Path) -> Command {
    use std::os::windows::process::CommandExt;
    let exe = std::env::var_os(ENV_GIT).unwrap_or_else(|| "git".into());
    let mut cmd = Command::new(exe);
    cmd.arg("-C")
        .arg(dir)
        .args(["-c", "core.longpaths=true", "-c", "core.quotepath=false"])
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW);
    for v in GIT_ENV_OVERRIDES {
        cmd.env_remove(v);
    }
    cmd
}

/// Run git in `dir`; never fails on a non-zero exit (see [`Output::ok`]).
pub fn output(dir: &Path, args: &[&str]) -> Result<Output, String> {
    let out = command(dir)
        .args(args)
        .output()
        .map_err(|e| format!("cannot run git: {e}"))?;
    Ok(Output {
        ok: out.status.success(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
    })
}

/// Run git in `dir`; a non-zero exit becomes `Err("git <args>: <stderr>")`.
pub fn run(dir: &Path, args: &[&str]) -> Result<String, String> {
    let o = output(dir, args)?;
    if o.ok {
        Ok(o.stdout)
    } else {
        Err(format!("git {}: {}", args.join(" "), o.stderr))
    }
}

/// Like [`run`] for path arguments.
fn run_os(dir: &Path, args: &[&std::ffi::OsStr]) -> Result<String, String> {
    let out = command(dir)
        .args(args)
        .output()
        .map_err(|e| format!("cannot run git: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        let shown: Vec<String> = args.iter().map(|a| a.to_string_lossy().into()).collect();
        Err(format!(
            "git {}: {}",
            shown.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

// ---------------------------------------------------------------------------
// Paths
// ---------------------------------------------------------------------------

/// Absolute, symlink-resolved path without the `\\?\` prefix that
/// `canonicalize` adds (git and most tools do not accept it). Falls back to
/// a lexical absolute path when the path does not exist.
pub fn normalize(p: &Path) -> PathBuf {
    let abs = std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf());
    match std::fs::canonicalize(&abs) {
        Ok(c) => strip_verbatim(&c),
        Err(_) => abs,
    }
}

fn strip_verbatim(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        PathBuf::from(format!(r"\\{rest}"))
    } else if let Some(rest) = s.strip_prefix(r"\\?\") {
        PathBuf::from(rest)
    } else {
        p.to_path_buf()
    }
}

/// Case-insensitive, separator-insensitive comparison (Windows semantics).
pub fn same_path(a: &Path, b: &Path) -> bool {
    fn key(p: &Path) -> String {
        let s = strip_verbatim(p).to_string_lossy().replace('/', "\\");
        s.trim_end_matches('\\').to_lowercase()
    }
    key(a) == key(b)
}

/// git prints `C:/x/y`; make it `C:\x\y`.
pub fn native_path(git_path: &str) -> PathBuf {
    PathBuf::from(git_path.trim().replace('/', "\\"))
}

// ---------------------------------------------------------------------------
// Repositories, refs, worktrees
// ---------------------------------------------------------------------------

/// The repository whose root is `dir`. Errors when `dir` is not inside a git
/// work tree, is bare, is a linked worktree, or is a subdirectory.
pub fn repo_root(dir: &Path) -> Result<PathBuf, String> {
    let o = output(
        dir,
        &[
            "rev-parse",
            "--path-format=absolute",
            "--is-bare-repository",
            "--git-dir",
            "--git-common-dir",
            "--show-toplevel",
        ],
    )?;
    if !o.ok {
        return Err(format!("{} is not a git repository", dir.display()));
    }
    let lines: Vec<&str> = o.stdout.lines().collect();
    if lines.first() == Some(&"true") {
        return Err(format!(
            "{} is a bare repository (a work tree is needed)",
            dir.display()
        ));
    }
    let [_, git_dir, common_dir, top] = lines[..] else {
        return Err(format!("{} is not inside a work tree", dir.display()));
    };
    if !same_path(&native_path(git_dir), &native_path(common_dir)) {
        return Err(format!(
            "{} is a linked worktree; add the main repository instead",
            dir.display()
        ));
    }
    let top = native_path(top);
    if !same_path(&normalize(&top), &normalize(dir)) {
        return Err(format!(
            "{} is inside the repository {}; add its root",
            dir.display(),
            top.display()
        ));
    }
    Ok(top)
}

/// `origin/HEAD`'s branch when it exists locally (else the remote ref), else
/// the checked-out branch when it has a commit, else `HEAD`.
pub fn default_base(repo: &Path) -> String {
    if let Ok(o) = output(
        repo,
        &[
            "symbolic-ref",
            "--quiet",
            "--short",
            "refs/remotes/origin/HEAD",
        ],
    ) && o.ok
    {
        let remote = o.stdout.trim().to_string();
        if let Some(local) = remote.strip_prefix("origin/")
            && branch_exists(repo, local)
        {
            return local.to_string();
        }
        if !remote.is_empty() {
            return remote;
        }
    }
    if let Ok(o) = output(repo, &["symbolic-ref", "--quiet", "--short", "HEAD"])
        && o.ok
    {
        let b = o.stdout.trim();
        if !b.is_empty() && resolve_commit(repo, b).is_some() {
            return b.to_string();
        }
    }
    "HEAD".into()
}

/// Full commit id of `rev`, if it names a commit.
pub fn resolve_commit(dir: &Path, rev: &str) -> Option<String> {
    if rev.is_empty() || rev.starts_with('-') {
        return None;
    }
    let spec = format!("{rev}^{{commit}}");
    let o = output(
        dir,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            "--end-of-options",
            &spec,
        ],
    )
    .ok()?;
    let s = o.stdout.trim();
    (o.ok && !s.is_empty()).then(|| s.to_string())
}

pub fn branch_exists(repo: &Path, branch: &str) -> bool {
    output(
        repo,
        &[
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ],
    )
    .is_ok_and(|o| o.ok)
}

/// `git check-ref-format --branch`.
pub fn valid_branch_name(repo: &Path, branch: &str) -> bool {
    output(repo, &["check-ref-format", "--branch", branch]).is_ok_and(|o| o.ok)
}

/// `git worktree add -b <branch> <path> <commit>`.
pub fn worktree_add(repo: &Path, path: &Path, branch: &str, commit: &str) -> Result<(), String> {
    run_os(
        repo,
        &[
            "worktree".as_ref(),
            "add".as_ref(),
            "-b".as_ref(),
            branch.as_ref(),
            path.as_os_str(),
            commit.as_ref(),
        ],
    )
    .map(|_| ())
}

/// `git worktree remove [--force] <path>`.
pub fn worktree_remove(repo: &Path, path: &Path, force: bool) -> Result<(), String> {
    let mut args: Vec<&std::ffi::OsStr> = vec!["worktree".as_ref(), "remove".as_ref()];
    if force {
        args.push("--force".as_ref());
    }
    args.push(path.as_os_str());
    run_os(repo, &args).map(|_| ())
}

pub fn worktree_prune(repo: &Path) -> Result<(), String> {
    run(repo, &["worktree", "prune"]).map(|_| ())
}

pub fn branch_delete(repo: &Path, branch: &str) -> Result<(), String> {
    run(repo, &["branch", "-D", "--end-of-options", branch]).map(|_| ())
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct WorktreeEntry {
    pub path: PathBuf,
    pub head: Option<String>,
    /// `refs/heads/...` stripped to the short name.
    pub branch: Option<String>,
    pub bare: bool,
    pub detached: bool,
    pub prunable: bool,
}

pub fn worktree_list(repo: &Path) -> Result<Vec<WorktreeEntry>, String> {
    run(repo, &["worktree", "list", "--porcelain", "-z"]).map(|s| parse_worktree_list(&s))
}

/// Parse `git worktree list --porcelain -z`: attribute lines separated by
/// NUL, records separated by an empty attribute.
pub fn parse_worktree_list(s: &str) -> Vec<WorktreeEntry> {
    let mut out = Vec::new();
    let mut cur: Option<WorktreeEntry> = None;
    for field in s.split('\0') {
        if field.is_empty() {
            if let Some(w) = cur.take() {
                out.push(w);
            }
            continue;
        }
        let (key, value) = field.split_once(' ').unwrap_or((field, ""));
        match key {
            "worktree" => {
                if let Some(w) = cur.take() {
                    out.push(w);
                }
                cur = Some(WorktreeEntry {
                    path: native_path(value),
                    ..Default::default()
                });
            }
            _ => {
                let Some(w) = cur.as_mut() else { continue };
                match key {
                    "HEAD" => w.head = Some(value.to_string()),
                    "branch" => {
                        w.branch = Some(value.strip_prefix("refs/heads/").unwrap_or(value).into())
                    }
                    "bare" => w.bare = true,
                    "detached" => w.detached = true,
                    "prunable" => w.prunable = true,
                    _ => {}
                }
            }
        }
    }
    if let Some(w) = cur {
        out.push(w);
    }
    out
}

// ---------------------------------------------------------------------------
// Task facts and diff
// ---------------------------------------------------------------------------

/// The base to compare against: `base` if it still resolves, else the commit
/// recorded at creation.
pub fn base_rev(dir: &Path, base: &str, base_commit: &str) -> String {
    resolve_commit(dir, base).unwrap_or_else(|| base_commit.to_string())
}

/// `merge-base(base, HEAD)` of a worktree (the base itself when there is none).
pub fn merge_base(worktree: &Path, base: &str, base_commit: &str) -> String {
    let base = base_rev(worktree, base, base_commit);
    match output(worktree, &["merge-base", &base, "HEAD"]) {
        Ok(o) if o.ok && !o.stdout.trim().is_empty() => o.stdout.trim().to_string(),
        _ => base,
    }
}

/// Contents of `path` (`/`-separated, relative to the repository root) at `rev`.
pub fn show_file(dir: &Path, rev: &str, path: &str) -> Result<String, String> {
    if rev.starts_with('-') {
        return Err(format!("invalid revision {rev:?}"));
    }
    run(dir, &["show", &format!("{rev}:{path}")])
}

/// Branch, ahead/behind, dirty count, unpushed count and last commit of a worktree.
pub fn facts(worktree: &Path, base: &str, base_commit: &str) -> Result<TaskGit, String> {
    let base = base_rev(worktree, base, base_commit);
    let mut g = TaskGit {
        head: resolve_commit(worktree, "HEAD"),
        ..Default::default()
    };
    let b = output(worktree, &["symbolic-ref", "--quiet", "--short", "HEAD"])?;
    if b.ok {
        g.branch = Some(b.stdout.trim().to_string());
    }
    let counts = run(
        worktree,
        &[
            "rev-list",
            "--left-right",
            "--count",
            &format!("{base}...HEAD"),
        ],
    )?;
    (g.behind, g.ahead) = parse_left_right(&counts)
        .ok_or_else(|| format!("unexpected rev-list output {counts:?}"))?;
    let unpushed = run(
        worktree,
        &["rev-list", "--count", "HEAD", "--not", &base, "--remotes"],
    )?;
    g.unpushed = unpushed.trim().parse().unwrap_or(0);
    let status = run(
        worktree,
        &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
    )?;
    g.dirty = count_status_entries(&status);
    let log = output(
        worktree,
        &["log", "-1", "--format=%H%x00%s%x00%an%x00%ct", "HEAD"],
    )?;
    if log.ok {
        g.last_commit = parse_commit(&log.stdout);
    }
    Ok(g)
}

fn parse_left_right(s: &str) -> Option<(u32, u32)> {
    let mut it = s.split_whitespace();
    let left = it.next()?.parse().ok()?;
    let right = it.next()?.parse().ok()?;
    Some((left, right))
}

/// Entries in `git status --porcelain=v1 -z` (a rename/copy carries an extra
/// NUL-terminated source path that is not an entry of its own).
pub fn count_status_entries(s: &str) -> u32 {
    let mut n = 0;
    let mut fields = s.split('\0').filter(|f| !f.is_empty());
    while let Some(f) = fields.next() {
        n += 1;
        let xy = f.as_bytes();
        if xy.len() >= 2 && (matches!(xy[0], b'R' | b'C') || matches!(xy[1], b'R' | b'C')) {
            fields.next();
        }
    }
    n
}

fn parse_commit(s: &str) -> Option<CommitInfo> {
    let mut it = s.trim_end_matches(['\r', '\n']).split('\0');
    let sha = it.next()?.to_string();
    let summary = it.next()?.to_string();
    let author = it.next()?.to_string();
    let secs: u64 = it.next()?.trim().parse().ok()?;
    (!sha.is_empty()).then_some(CommitInfo {
        sha,
        summary,
        author,
        time_ms: secs * 1000,
    })
}

/// Worktree against the merge base of the base and HEAD: tracked changes
/// (committed, staged or not) from `git diff --numstat` + `--name-status`,
/// plus untracked files counted as fully added.
pub fn diff(
    worktree: &Path,
    base: &str,
    base_commit: &str,
) -> Result<(String, Vec<DiffFile>), String> {
    let base = base_rev(worktree, base, base_commit);
    let mb = output(worktree, &["merge-base", &base, "HEAD"])?;
    let merge_base = if mb.ok && !mb.stdout.trim().is_empty() {
        mb.stdout.trim().to_string()
    } else {
        base.clone()
    };
    let common = ["diff", "--no-ext-diff", "--no-textconv", "-M", "-z"];
    let numstat = run(
        worktree,
        &[&common[..], &["--numstat", &merge_base, "--"]].concat(),
    )?;
    let names = run(
        worktree,
        &[&common[..], &["--name-status", &merge_base, "--"]].concat(),
    )?;
    let mut files = merge_diff(&parse_name_status(&names), &parse_numstat(&numstat));
    let untracked = run(
        worktree,
        &["ls-files", "-z", "--others", "--exclude-standard"],
    )?;
    for path in untracked.split('\0').filter(|p| !p.is_empty()) {
        let (added, binary) = count_lines(&worktree.join(path));
        files.push(DiffFile {
            path: path.to_string(),
            old_path: None,
            status: "?".into(),
            added,
            deleted: (!binary).then_some(0),
            binary,
        });
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok((merge_base, files))
}

/// `(status, old_path, path)` from `--name-status -z`.
pub fn parse_name_status(s: &str) -> Vec<(String, Option<String>, String)> {
    let mut out = Vec::new();
    let mut it = s.split('\0').filter(|f| !f.is_empty());
    while let Some(code) = it.next() {
        let letter = code.chars().next().unwrap_or('M');
        if matches!(letter, 'R' | 'C') {
            let (Some(old), Some(new)) = (it.next(), it.next()) else {
                break;
            };
            out.push((letter.to_string(), Some(old.to_string()), new.to_string()));
        } else {
            let Some(path) = it.next() else { break };
            out.push((letter.to_string(), None, path.to_string()));
        }
    }
    out
}

/// `(added, deleted, path)` from `--numstat -z`; counts are `None` for binary
/// files. A rename is `a\td\t\0old\0new\0` and keyed by the new path.
pub fn parse_numstat(s: &str) -> Vec<(Option<u64>, Option<u64>, String)> {
    let mut out = Vec::new();
    let mut it = s.split('\0');
    while let Some(rec) = it.next() {
        if rec.is_empty() {
            continue;
        }
        let mut cols = rec.splitn(3, '\t');
        let (Some(a), Some(d), Some(path)) = (cols.next(), cols.next(), cols.next()) else {
            continue;
        };
        let path = if path.is_empty() {
            let _old = it.next();
            match it.next() {
                Some(new) => new.to_string(),
                None => break,
            }
        } else {
            path.to_string()
        };
        out.push((a.parse().ok(), d.parse().ok(), path));
    }
    out
}

fn merge_diff(
    names: &[(String, Option<String>, String)],
    stats: &[(Option<u64>, Option<u64>, String)],
) -> Vec<DiffFile> {
    names
        .iter()
        .map(|(status, old, path)| {
            let st = stats.iter().find(|(_, _, p)| p == path);
            let (added, deleted) = st.map(|(a, d, _)| (*a, *d)).unwrap_or((Some(0), Some(0)));
            DiffFile {
                path: path.clone(),
                old_path: old.clone(),
                status: status.clone(),
                binary: added.is_none() && deleted.is_none(),
                added,
                deleted,
            }
        })
        .collect()
}

/// Lines in a new file (`None` + binary for files with NUL bytes, or too big to read).
fn count_lines(path: &Path) -> (Option<u64>, bool) {
    let too_big = std::fs::metadata(path).map_or(true, |m| m.len() > MAX_COUNT_BYTES);
    if too_big {
        return (None, false);
    }
    let Ok(bytes) = std::fs::read(path) else {
        return (None, false);
    };
    if bytes[..bytes.len().min(8000)].contains(&0) {
        return (None, true);
    }
    let mut n = bytes.iter().filter(|&&b| b == b'\n').count() as u64;
    if bytes.last().is_some_and(|&b| b != b'\n') {
        n += 1;
    }
    (Some(n), false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worktree_list_porcelain() {
        let s = "worktree C:/src/app\0HEAD 1111\0branch refs/heads/main\0\0\
                 worktree C:/Users/me/pod/worktrees/app/feat x\0HEAD 2222\0branch refs/heads/pod/feat-x\0\0\
                 worktree C:/tmp/gone\0HEAD 3333\0detached\0prunable gitdir file points to non-existent location\0\0";
        let w = parse_worktree_list(s);
        assert_eq!(w.len(), 3);
        assert_eq!(w[0].path, PathBuf::from(r"C:\src\app"));
        assert_eq!(w[0].branch.as_deref(), Some("main"));
        assert_eq!(
            w[1].path,
            PathBuf::from(r"C:\Users\me\pod\worktrees\app\feat x")
        );
        assert_eq!(w[1].branch.as_deref(), Some("pod/feat-x"));
        assert!(w[2].detached && w[2].prunable && w[2].branch.is_none());
    }

    #[test]
    fn numstat_and_name_status() {
        let numstat =
            "3\t0\tnew.txt\x001\t2\tREADME.md\0-\t-\timg.png\x000\t0\t\0old name.rs\0new name.rs\0";
        let n = parse_numstat(numstat);
        assert_eq!(
            n,
            vec![
                (Some(3), Some(0), "new.txt".into()),
                (Some(1), Some(2), "README.md".into()),
                (None, None, "img.png".into()),
                (Some(0), Some(0), "new name.rs".into()),
            ]
        );
        let names = "A\0new.txt\0M\0README.md\0M\0img.png\0R100\0old name.rs\0new name.rs\0";
        let s = parse_name_status(names);
        assert_eq!(
            s[3],
            ("R".into(), Some("old name.rs".into()), "new name.rs".into())
        );
        let files = merge_diff(&s, &n);
        assert_eq!(files[0].added, Some(3));
        assert_eq!(files[1].deleted, Some(2));
        assert!(files[2].binary && files[2].added.is_none());
        assert_eq!(files[3].old_path.as_deref(), Some("old name.rs"));
    }

    #[test]
    fn status_entries_skip_rename_sources() {
        assert_eq!(count_status_entries(""), 0);
        assert_eq!(
            count_status_entries(" M a.txt\0R  new.txt\0old.txt\0?? b c.txt\0"),
            3
        );
    }

    #[test]
    fn misc_parsers() {
        assert_eq!(parse_left_right("2\t5\n"), Some((2, 5)));
        let c = parse_commit("abc\0fix: thing\0Pod Test\x001700000000\n").unwrap();
        assert_eq!(c.summary, "fix: thing");
        assert_eq!(c.time_ms, 1_700_000_000_000);
    }

    #[test]
    fn path_helpers() {
        assert!(same_path(Path::new(r"C:\A\b\"), Path::new("c:/a/B")));
        assert!(!same_path(Path::new(r"C:\a\b"), Path::new(r"C:\a\bc")));
        assert_eq!(
            strip_verbatim(Path::new(r"\\?\C:\x y\z")),
            PathBuf::from(r"C:\x y\z")
        );
        assert_eq!(
            strip_verbatim(Path::new(r"\\?\UNC\srv\share\x")),
            PathBuf::from(r"\\srv\share\x")
        );
        assert_eq!(native_path("C:/a b/c\n"), PathBuf::from(r"C:\a b\c"));
    }
}
