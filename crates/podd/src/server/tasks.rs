//! Repositories and tasks: one task = one git worktree on branch `pod/<name>`
//! + the terminal(s) working in it.
//!
//! The registry is a small JSON file (`<state_dir>\tasks\registry.json`,
//! written atomically via a temp file + rename). Terminals keep living in
//! their holders; a task only stores terminal ids, so after a daemon restart
//! the task is re-joined with the rediscovered terminals by id.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::{Daemon, log};
use crate::git;
use crate::protocol::*;

const REGISTRY_VERSION: u32 = 1;
const MAX_NAME: usize = 64;
/// Environment variable carrying the task id inside a task's terminals.
pub const ENV_TASK_ID: &str = "POD_TASK_ID";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoRecord {
    pub name: String,
    pub path: String,
    pub added_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskRecord {
    pub id: String,
    pub repo: String,
    pub name: String,
    pub branch: String,
    pub base: String,
    pub base_commit: String,
    pub worktree: String,
    pub agent: AgentSpec,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    pub created_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_task: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_terminal: Option<String>,
    /// Terminal ids, first = the agent terminal.
    #[serde(default)]
    pub terminals: Vec<String>,
    /// Last known pull request of the branch (v10, `server/pr.rs`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr: Option<TaskPr>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Registry {
    #[serde(default)]
    pub version: u32,
    #[serde(default)]
    pub repos: Vec<RepoRecord>,
    #[serde(default)]
    pub tasks: Vec<TaskRecord>,
}

impl Registry {
    pub fn path(state_dir: &Path) -> PathBuf {
        state_dir.join("tasks").join("registry.json")
    }

    /// Load, or start empty. A corrupt file is moved aside, not overwritten.
    pub fn load(state_dir: &Path) -> Self {
        let path = Self::path(state_dir);
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Self::default(),
            Err(e) => {
                log(&format!("cannot read {}: {e}", path.display()));
                return Self::default();
            }
        };
        match serde_json::from_slice(&bytes) {
            Ok(r) => r,
            Err(e) => {
                let aside = path.with_extension(format!("corrupt-{}.json", crate::now_ms()));
                log(&format!(
                    "registry {} is corrupt ({e}); moved to {}",
                    path.display(),
                    aside.display()
                ));
                let _ = std::fs::rename(&path, aside);
                Self::default()
            }
        }
    }

    pub fn save(&mut self, state_dir: &Path) -> Result<(), RpcError> {
        self.version = REGISTRY_VERSION;
        let path = Self::path(state_dir);
        let write = || -> std::io::Result<()> {
            std::fs::create_dir_all(path.parent().unwrap())?;
            let tmp = path.with_extension("json.tmp");
            std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
            std::fs::rename(&tmp, &path)
        };
        write().map_err(|e| {
            RpcError::new(
                codes::INTERNAL_ERROR,
                format!("cannot save {}: {e}", path.display()),
            )
        })
    }

    /// By registry name, else by path.
    fn repo(&self, s: &str) -> Option<&RepoRecord> {
        self.repos.iter().find(|r| r.name == s).or_else(|| {
            let p = git::normalize(Path::new(s));
            self.repos
                .iter()
                .find(|r| git::same_path(Path::new(&r.path), &p))
        })
    }

    /// By id (`<repo>/<name>`), else by name when unique.
    fn task(&self, s: &str) -> Result<&TaskRecord, RpcError> {
        if let Some(t) = self.tasks.iter().find(|t| t.id == s) {
            return Ok(t);
        }
        let mut by_name = self.tasks.iter().filter(|t| t.name == s);
        match (by_name.next(), by_name.next()) {
            (Some(t), None) => Ok(t),
            (Some(_), Some(_)) => Err(RpcError::new(
                codes::TASK_NOT_FOUND,
                format!("task name {s:?} is ambiguous; use <repo>/<name>"),
            )),
            _ => Err(RpcError::new(
                codes::TASK_NOT_FOUND,
                format!("no task {s:?}"),
            )),
        }
    }

    /// Highest terminal number referenced by any task (so ids are not reused).
    pub fn max_terminal_number(&self) -> u64 {
        self.tasks
            .iter()
            .flat_map(|t| &t.terminals)
            .map(|id| super::id_number(id))
            .max()
            .unwrap_or(0)
    }
}

/// Repository, task and branch names: `[A-Za-z0-9._-]`, 1..=64 chars, no
/// leading `.`/`-`, no `..`, not ending in `.`/`.lock`, not a DOS device name.
pub fn validate_name(kind: &str, s: &str) -> Result<(), RpcError> {
    let bad = |why: &str| {
        Err(RpcError::new(
            codes::INVALID_PARAMS,
            format!("invalid {kind} name {s:?}: {why}"),
        ))
    };
    if s.is_empty() || s.len() > MAX_NAME {
        return bad("must be 1 to 64 characters");
    }
    if !s
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    {
        return bad("use only letters, digits, '.', '_' and '-'");
    }
    if s.starts_with(['.', '-']) || s.ends_with('.') || s.contains("..") || s.ends_with(".lock") {
        return bad("must not start with '.'/'-', end with '.'/'.lock' or contain '..'");
    }
    let stem = s.split('.').next().unwrap_or("").to_ascii_uppercase();
    let reserved = ["CON", "PRN", "AUX", "NUL"];
    let numbered = ["COM", "LPT"];
    if reserved.contains(&stem.as_str())
        || (stem.len() == 4
            && numbered.contains(&&stem[..3])
            && stem.as_bytes()[3].is_ascii_digit())
    {
        return bad("reserved device name on Windows");
    }
    Ok(())
}

/// The agent terminal's argv (without the prompt check).
pub fn agent_argv(agent: &AgentSpec, prompt: Option<&str>) -> Result<Vec<String>, RpcError> {
    let with_prompt = |prog: &str| {
        let mut v = vec![prog.to_string()];
        v.extend(prompt.map(str::to_string));
        v
    };
    let no_prompt = |what: &str| {
        RpcError::new(
            codes::INVALID_PARAMS,
            format!("a prompt is only supported for claude and codex, not {what}"),
        )
    };
    match agent {
        AgentSpec::Kind(AgentKind::Claude) => Ok(with_prompt("claude")),
        AgentSpec::Kind(AgentKind::Codex) => Ok(with_prompt("codex")),
        AgentSpec::Kind(AgentKind::Shell) => match prompt {
            Some(_) => Err(no_prompt("shell")),
            None => Ok(vec!["powershell".into(), "-NoLogo".into()]),
        },
        AgentSpec::Argv { argv } => {
            if argv.is_empty() || argv[0].is_empty() {
                return Err(RpcError::new(
                    codes::INVALID_PARAMS,
                    "agent argv must not be empty",
                ));
            }
            match prompt {
                Some(_) => Err(no_prompt("an explicit argv")),
                None => Ok(argv.clone()),
            }
        }
    }
}

/// `cmd.exe` would interpret these inside a `.cmd` shim's arguments.
pub fn cmd_unsafe_prompt(prompt: &str) -> bool {
    prompt.contains(['"', '%', '\r', '\n'])
}

fn git_err(m: String) -> RpcError {
    RpcError::new(codes::GIT_FAILED, m)
}

pub(super) async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> T + Send + 'static,
) -> Result<T, RpcError> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| RpcError::new(codes::INTERNAL_ERROR, format!("worker failed: {e}")))
}

impl Daemon {
    pub(super) fn save_registry(&self, reg: &mut Registry) -> Result<(), RpcError> {
        reg.save(&self.config.state_dir)
    }

    // -- repositories -------------------------------------------------------

    pub(super) async fn repo_add(&self, p: RepoAddParams) -> Result<RepoInfo, RpcError> {
        let dir = git::normalize(Path::new(&p.path));
        if !dir.is_dir() {
            return Err(RpcError::new(
                codes::NOT_A_REPO,
                format!("{} is not a directory", dir.display()),
            ));
        }
        let probe = dir.clone();
        let root = blocking(move || git::repo_root(&probe))
            .await?
            .map_err(|m| RpcError::new(codes::NOT_A_REPO, m))?;
        let path = git::normalize(&root);
        let name = match p.name {
            Some(n) => n,
            None => path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
        };
        validate_name("repository", &name).map_err(|mut e| {
            e.message.push_str(" (pass a name)");
            e
        })?;
        let rec = RepoRecord {
            name: name.clone(),
            path: path.display().to_string(),
            added_ms: crate::now_ms(),
        };
        {
            let mut reg = self.registry.lock().unwrap();
            if let Some(r) = reg
                .repos
                .iter()
                .find(|r| git::same_path(Path::new(&r.path), &path))
            {
                return Err(RpcError::new(
                    codes::REPO_EXISTS,
                    format!("{} is already registered as {:?}", path.display(), r.name),
                ));
            }
            if reg.repos.iter().any(|r| r.name == name) {
                return Err(RpcError::new(
                    codes::REPO_EXISTS,
                    format!("a repository named {name:?} exists; pass another name"),
                ));
            }
            reg.repos.push(rec.clone());
            self.save_registry(&mut reg)?;
        }
        log(&format!("repo add {name} {}", path.display()));
        Ok(self.repo_info(rec).await)
    }

    async fn repo_info(&self, r: RepoRecord) -> RepoInfo {
        let tasks = self
            .registry
            .lock()
            .unwrap()
            .tasks
            .iter()
            .filter(|t| t.repo == r.name)
            .count();
        let path = PathBuf::from(&r.path);
        let default_base = if path.is_dir() {
            blocking(move || git::default_base(&path)).await.ok()
        } else {
            None
        };
        RepoInfo {
            name: r.name,
            path: r.path,
            added_ms: r.added_ms,
            default_base,
            tasks,
        }
    }

    pub(super) async fn repo_list(&self) -> RepoListResult {
        let repos = self.registry.lock().unwrap().repos.clone();
        let mut out = Vec::with_capacity(repos.len());
        for r in repos {
            out.push(self.repo_info(r).await);
        }
        RepoListResult { repos: out }
    }

    pub(super) fn repo_remove(&self, p: RepoRef) -> Result<RepoRemoveResult, RpcError> {
        let mut reg = self.registry.lock().unwrap();
        let name = reg
            .repo(&p.repo)
            .ok_or_else(|| repo_not_found(&p.repo))?
            .name
            .clone();
        let n = reg.tasks.iter().filter(|t| t.repo == name).count();
        if n > 0 {
            return Err(RpcError::new(
                codes::REPO_IN_USE,
                format!("repository {name:?} still has {n} task(s); remove them first"),
            ));
        }
        reg.repos.retain(|r| r.name != name);
        self.save_registry(&mut reg)?;
        log(&format!("repo rm {name}"));
        Ok(RepoRemoveResult { name })
    }

    // -- tasks --------------------------------------------------------------

    pub(super) async fn task_create(&self, p: TaskCreateParams) -> Result<TaskInfo, RpcError> {
        let _op = self.task_ops.lock().await;
        let repo = self
            .registry
            .lock()
            .unwrap()
            .repo(&p.repo)
            .cloned()
            .ok_or_else(|| repo_not_found(&p.repo))?;
        validate_name("task", &p.name)?;
        let id = format!("{}/{}", repo.name, p.name);
        let branch = format!("pod/{}", p.name);
        if self
            .registry
            .lock()
            .unwrap()
            .tasks
            .iter()
            .any(|t| t.id == id)
        {
            return Err(RpcError::new(
                codes::TASK_EXISTS,
                format!("task {id} exists"),
            ));
        }
        let agent = p.agent.unwrap_or_default();
        let prompt = p.prompt.filter(|s| !s.is_empty());
        let argv = agent_argv(&agent, prompt.as_deref())?;
        if let Some(pr) = &prompt
            && cmd_unsafe_prompt(pr)
            && crate::pty::runs_through_cmd(&argv, &self.terminal_env("t0", BTreeMap::new()))
        {
            return Err(RpcError::new(
                codes::INVALID_PARAMS,
                format!(
                    "{} is a .cmd shim run by cmd.exe, which would mangle '\"', '%' or line breaks \
                     in the prompt; simplify the prompt or start the agent without one",
                    argv[0]
                ),
            ));
        }
        // Parent: the caller's terminal, if this daemon knows it.
        let (parent_terminal, parent_task) = match &p.parent_terminal {
            Some(tid) if self.terminals.lock().unwrap().iter().any(|t| &t.id == tid) => {
                let task = self
                    .registry
                    .lock()
                    .unwrap()
                    .tasks
                    .iter()
                    .find(|t| t.terminals.contains(tid))
                    .map(|t| t.id.clone());
                (Some(tid.clone()), task)
            }
            Some(tid) => {
                log(&format!("task {id}: unknown parent terminal {tid} ignored"));
                (None, None)
            }
            None => (None, None),
        };

        let worktree = self.config.worktrees_root.join(&repo.name).join(&p.name);
        if worktree.exists() {
            return Err(RpcError::new(
                codes::TASK_EXISTS,
                format!("worktree directory {} already exists", worktree.display()),
            ));
        }
        let repo_path = PathBuf::from(&repo.path);
        let (base, base_commit) = {
            let (repo_path, worktree, branch) =
                (repo_path.clone(), worktree.clone(), branch.clone());
            let base = p.base.clone();
            blocking(move || -> Result<(String, String), RpcError> {
                if !git::valid_branch_name(&repo_path, &branch) {
                    return Err(RpcError::new(
                        codes::INVALID_PARAMS,
                        format!("{branch:?} is not a valid branch name"),
                    ));
                }
                if git::branch_exists(&repo_path, &branch) {
                    return Err(RpcError::new(
                        codes::TASK_EXISTS,
                        format!("branch {branch} already exists in {}", repo_path.display()),
                    ));
                }
                let base = base.unwrap_or_else(|| git::default_base(&repo_path));
                let commit = git::resolve_commit(&repo_path, &base).ok_or_else(|| {
                    RpcError::new(
                        codes::INVALID_PARAMS,
                        format!("base {base:?} is not a commit in {}", repo_path.display()),
                    )
                })?;
                if let Some(parent) = worktree.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| {
                        RpcError::new(
                            codes::INTERNAL_ERROR,
                            format!("cannot create {}: {e}", parent.display()),
                        )
                    })?;
                }
                git::worktree_add(&repo_path, &worktree, &branch, &commit).map_err(git_err)?;
                Ok((base, commit))
            })
            .await??
        };
        log(&format!(
            "task {id}: worktree {} on {branch} from {base} ({})",
            worktree.display(),
            &base_commit[..base_commit.len().min(12)]
        ));

        // Codex: trust exactly this worktree for this run (agent_launch.rs).
        let argv = super::agent_launch::task_agent_argv(&agent, argv, &worktree);
        let mut env = BTreeMap::new();
        env.insert(ENV_TASK_ID.to_string(), id.clone());
        let run = self
            .run(RunParams {
                argv,
                cwd: Some(worktree.display().to_string()),
                name: Some(id.clone()),
                env,
                cols: p.cols,
                rows: p.rows,
                task: None,
                // The agent terminal's parent is the task's parent (the caller).
                parent_terminal: parent_terminal.clone(),
            })
            .await;
        let term = match run {
            Ok(t) => t,
            Err(e) => {
                // Roll back the worktree and the branch.
                let (r, w, b) = (repo_path.clone(), worktree.clone(), branch.clone());
                let _ = blocking(move || {
                    let _ = git::worktree_remove(&r, &w, true);
                    let _ = git::worktree_prune(&r);
                    let _ = git::branch_delete(&r, &b);
                })
                .await;
                return Err(e);
            }
        };
        let rec = TaskRecord {
            id: id.clone(),
            repo: repo.name.clone(),
            name: p.name.clone(),
            branch,
            base,
            base_commit,
            worktree: worktree.display().to_string(),
            agent,
            prompt,
            created_ms: crate::now_ms(),
            parent_task,
            parent_terminal,
            terminals: vec![term.id.clone()],
            pr: None,
        };
        {
            let mut reg = self.registry.lock().unwrap();
            reg.tasks.push(rec.clone());
            self.save_registry(&mut reg)?;
        }
        log(&format!("task {id}: agent terminal {}", term.id));
        let info = self.task_info(rec).await;
        super::lifecycle::task_created(&info);
        Ok(info)
    }

    pub(super) fn repo_record(&self, s: &str) -> Result<RepoRecord, RpcError> {
        self.registry
            .lock()
            .unwrap()
            .repo(s)
            .cloned()
            .ok_or_else(|| repo_not_found(s))
    }

    pub(super) fn task_record(&self, s: &str) -> Result<TaskRecord, RpcError> {
        self.registry.lock().unwrap().task(s).cloned()
    }

    pub(super) async fn task_info(&self, rec: TaskRecord) -> TaskInfo {
        let terminals: Vec<TaskTerminal> = {
            let terms = self.terminals.lock().unwrap();
            rec.terminals
                .iter()
                .map(|id| TaskTerminal {
                    id: id.clone(),
                    info: terms.iter().find(|t| &t.id == id).map(|t| t.info()),
                })
                .collect()
        };
        let wt = PathBuf::from(&rec.worktree);
        let worktree_exists = wt.is_dir();
        let (git, git_error) = if worktree_exists {
            let (base, commit) = (rec.base.clone(), rec.base_commit.clone());
            match blocking(move || git::facts(&wt, &base, &commit)).await {
                Ok(Ok(g)) => (Some(g), None),
                Ok(Err(e)) => (None, Some(e)),
                Err(e) => (None, Some(e.message)),
            }
        } else {
            (None, Some("worktree directory is missing".into()))
        };
        let status = aggregate_task_status(terminals.iter().filter_map(|t| t.info.as_ref()));
        let (review, gates, group) = {
            let st = self.orch.lock().unwrap();
            (
                st.review_counts(&rec.id),
                st.task_gates(&rec.id),
                st.group_of(&rec.id),
            )
        };
        TaskInfo {
            status,
            review,
            gates,
            group,
            pr: rec.pr,
            id: rec.id,
            repo: rec.repo,
            name: rec.name,
            branch: rec.branch,
            base: rec.base,
            base_commit: rec.base_commit,
            worktree: rec.worktree,
            agent: rec.agent,
            prompt: rec.prompt,
            created_ms: rec.created_ms,
            parent_task: rec.parent_task,
            parent_terminal: rec.parent_terminal,
            terminals,
            worktree_exists,
            git,
            git_error,
        }
    }

    pub(super) async fn task_list(&self, p: TaskListParams) -> Result<TaskListResult, RpcError> {
        let tasks: Vec<TaskRecord> = {
            let reg = self.registry.lock().unwrap();
            let repo = match &p.repo {
                Some(r) => Some(reg.repo(r).ok_or_else(|| repo_not_found(r))?.name.clone()),
                None => None,
            };
            reg.tasks
                .iter()
                .filter(|t| repo.as_ref().is_none_or(|r| &t.repo == r))
                .cloned()
                .collect()
        };
        let mut out = Vec::with_capacity(tasks.len());
        for t in tasks {
            out.push(self.task_info(t).await);
        }
        Ok(TaskListResult { tasks: out })
    }

    pub(super) async fn task_show(&self, p: TaskRef) -> Result<TaskInfo, RpcError> {
        let rec = self.task_record(&p.task)?;
        Ok(self.task_info(rec).await)
    }

    pub(super) async fn task_diff(&self, p: TaskRef) -> Result<TaskDiffResult, RpcError> {
        let rec = self.task_record(&p.task)?;
        let wt = PathBuf::from(&rec.worktree);
        if !wt.is_dir() {
            return Err(git_err(format!("worktree {} is missing", wt.display())));
        }
        let (base, commit) = (rec.base.clone(), rec.base_commit.clone());
        let (merge_base, files) = blocking(move || git::diff(&wt, &base, &commit))
            .await?
            .map_err(git_err)?;
        let added = files.iter().filter_map(|f| f.added).sum();
        let deleted = files.iter().filter_map(|f| f.deleted).sum();
        Ok(TaskDiffResult {
            task: rec.id,
            base: rec.base,
            merge_base,
            files,
            added,
            deleted,
        })
    }

    pub(super) async fn task_remove(
        &self,
        p: TaskRemoveParams,
    ) -> Result<TaskRemoveResult, RpcError> {
        let _op = self.task_ops.lock().await;
        let rec = self.task_record(&p.task)?;
        let wt = PathBuf::from(&rec.worktree);
        let repo_path = {
            let reg = self.registry.lock().unwrap();
            reg.repo(&rec.repo).map(|r| PathBuf::from(&r.path))
        }
        .ok_or_else(|| repo_not_found(&rec.repo))?;

        // 1. Refuse to lose work unless forced.
        if wt.is_dir() && !p.force {
            let (w, base, commit) = (wt.clone(), rec.base.clone(), rec.base_commit.clone());
            let g = blocking(move || git::facts(&w, &base, &commit))
                .await?
                .map_err(|e| git_err(format!("{e} (use force to remove anyway)")))?;
            if g.dirty > 0 || g.unpushed > 0 {
                let mut e = RpcError::new(
                    codes::TASK_DIRTY,
                    format!(
                        "task {} has {} uncommitted change(s) and {} unpushed commit(s); use force",
                        rec.id, g.dirty, g.unpushed
                    ),
                );
                e.data = Some(serde_json::json!({"dirty": g.dirty, "unpushed": g.unpushed}));
                return Err(e);
            }
        }

        // 2. Tell editors (v9 `task.removing`), give them a moment to save or
        // close, and without force re-check: a save may have made it dirty.
        let grace = super::lifecycle::remove_grace_ms();
        super::status::publish_task_removing(&rec.id, &rec.worktree, p.force, grace);
        if grace > 0 {
            tokio::time::sleep(Duration::from_millis(grace)).await;
            if wt.is_dir() && !p.force {
                let (w, base, commit) = (wt.clone(), rec.base.clone(), rec.base_commit.clone());
                let g = blocking(move || git::facts(&w, &base, &commit))
                    .await?
                    .map_err(|e| git_err(format!("{e} (use force to remove anyway)")))?;
                if g.dirty > 0 || g.unpushed > 0 {
                    let mut e = RpcError::new(
                        codes::TASK_DIRTY,
                        format!(
                            "task {} has {} uncommitted change(s) and {} unpushed commit(s) \
                             after editors were told it is being removed; use force",
                            rec.id, g.dirty, g.unpushed
                        ),
                    );
                    e.data = Some(serde_json::json!({"dirty": g.dirty, "unpushed": g.unpushed}));
                    return Err(e);
                }
            }
        }

        // 3. Kill and forget the task's terminals (they hold the directory open).
        let mut removed = Vec::new();
        for tid in &rec.terminals {
            let known = self.terminals.lock().unwrap().iter().any(|t| &t.id == tid);
            if known {
                self.rm(RmParams {
                    target: TerminalRef::id(tid.clone()),
                    force: true,
                })
                .await?;
                removed.push(tid.clone());
            }
        }

        // 4. Worktree + branch.
        let (force, delete_branch, branch) = (p.force, p.delete_branch, rec.branch.clone());
        let (worktree_removed, branch_deleted) = {
            let (repo_path, wt) = (repo_path.clone(), wt.clone());
            blocking(move || remove_worktree(&repo_path, &wt, force, delete_branch, &branch))
                .await??
        };

        {
            let mut reg = self.registry.lock().unwrap();
            reg.tasks.retain(|t| t.id != rec.id);
            self.save_registry(&mut reg)?;
        }
        log(&format!(
            "task rm {} (terminals {:?}, worktree removed {worktree_removed}, branch deleted {branch_deleted})",
            rec.id, removed
        ));
        super::lifecycle::task_removed(&rec.id);
        Ok(TaskRemoveResult {
            id: rec.id,
            terminals: removed,
            worktree_removed,
            branch_deleted,
        })
    }

    /// `terminal.run` with `task`: default cwd = worktree, `POD_TASK_ID` set,
    /// terminal recorded in the task.
    pub(super) async fn run_in_task(&self, mut p: RunParams) -> Result<TerminalInfo, RpcError> {
        let Some(task) = p.task.take() else {
            return self.run(p).await;
        };
        let _op = self.task_ops.lock().await;
        let rec = self.task_record(&task)?;
        p.cwd.get_or_insert_with(|| rec.worktree.clone());
        p.env
            .entry(ENV_TASK_ID.to_string())
            .or_insert_with(|| rec.id.clone());
        let info = self.run(p).await?;
        {
            let mut reg = self.registry.lock().unwrap();
            if let Some(t) = reg.tasks.iter_mut().find(|t| t.id == rec.id) {
                t.terminals.push(info.id.clone());
            }
            self.save_registry(&mut reg)?;
        }
        self.task_terminals_changed(&rec.id);
        Ok(info)
    }

    /// A terminal was removed: drop it from its task.
    pub(super) fn forget_task_terminal(&self, id: &str) {
        let mut changed = Vec::new();
        {
            let mut reg = self.registry.lock().unwrap();
            for t in reg.tasks.iter_mut() {
                let before = t.terminals.len();
                t.terminals.retain(|x| x != id);
                if t.terminals.len() != before {
                    changed.push(t.id.clone());
                }
            }
            if !changed.is_empty()
                && let Err(e) = self.save_registry(&mut reg)
            {
                log(&e.message);
            }
        }
        for task in changed {
            self.task_terminals_changed(&task);
        }
    }
}

/// `git worktree remove` (retrying while killed processes release the
/// directory), prune, and optionally delete the branch.
fn remove_worktree(
    repo: &Path,
    wt: &Path,
    force: bool,
    delete_branch: bool,
    branch: &str,
) -> Result<(bool, bool), RpcError> {
    let registered = git::worktree_list(repo)
        .map_err(git_err)?
        .iter()
        .any(|w| git::same_path(&w.path, wt));
    let mut worktree_removed = false;
    if registered {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match git::worktree_remove(repo, wt, force) {
                Ok(()) => {
                    worktree_removed = true;
                    break;
                }
                // Handles of just-killed processes can linger for a moment.
                Err(_) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(250));
                }
                Err(e) => {
                    // git may have unregistered it but left files behind.
                    if force && wt.exists() && std::fs::remove_dir_all(wt).is_ok() {
                        worktree_removed = true;
                        break;
                    }
                    return Err(git_err(e));
                }
            }
        }
    }
    git::worktree_prune(repo).map_err(git_err)?;
    let mut branch_deleted = false;
    if delete_branch && git::branch_exists(repo, branch) {
        git::branch_delete(repo, branch).map_err(git_err)?;
        branch_deleted = true;
    }
    Ok((worktree_removed, branch_deleted))
}

fn repo_not_found(s: &str) -> RpcError {
    RpcError::new(
        codes::REPO_NOT_FOUND,
        format!("no repository {s:?} (see repo.list / pod repo add)"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        for ok in ["feat-1", "a", "fix_x.2", "ABC"] {
            assert!(validate_name("task", ok).is_ok(), "{ok}");
        }
        for bad in [
            "", "-x", ".x", "x.", "a..b", "a/b", "a b", "x.lock", "con", "NUL.txt", "com1", "é",
        ] {
            assert!(validate_name("task", bad).is_err(), "{bad}");
        }
        assert!(validate_name("task", &"x".repeat(65)).is_err());
        assert!(validate_name("task", "command").is_ok());
    }

    #[test]
    fn agent_command_lines() {
        let claude = AgentSpec::Kind(AgentKind::Claude);
        assert_eq!(agent_argv(&claude, None).unwrap(), ["claude"]);
        assert_eq!(
            agent_argv(&claude, Some("fix the bug")).unwrap(),
            ["claude", "fix the bug"]
        );
        assert_eq!(
            agent_argv(&AgentSpec::Kind(AgentKind::Codex), Some("x")).unwrap(),
            ["codex", "x"]
        );
        assert!(agent_argv(&AgentSpec::default(), Some("x")).is_err());
        assert_eq!(
            agent_argv(&AgentSpec::default(), None).unwrap(),
            ["powershell", "-NoLogo"]
        );
        assert!(agent_argv(&AgentSpec::Argv { argv: vec![] }, None).is_err());
        assert!(cmd_unsafe_prompt("say \"hi\""));
        assert!(cmd_unsafe_prompt("100%"));
        assert!(!cmd_unsafe_prompt("fix it & ship | now"));
    }

    #[test]
    fn registry_roundtrip_and_lookup() {
        let dir = std::env::temp_dir().join(format!(
            "pod-registry-test-{}-{}",
            std::process::id(),
            crate::now_ms()
        ));
        let mut reg = Registry::default();
        reg.repos.push(RepoRecord {
            name: "app".into(),
            path: r"C:\src\app".into(),
            added_ms: 1,
        });
        for (repo, name, term) in [("app", "x", "t3"), ("lib", "x", "t9"), ("app", "y", "t4")] {
            reg.tasks.push(TaskRecord {
                id: format!("{repo}/{name}"),
                repo: repo.into(),
                name: name.into(),
                branch: format!("pod/{name}"),
                base: "main".into(),
                base_commit: "abc".into(),
                worktree: "w".into(),
                agent: AgentSpec::default(),
                prompt: None,
                created_ms: 0,
                parent_task: None,
                parent_terminal: None,
                terminals: vec![term.into()],
                pr: None,
            });
        }
        reg.save(&dir).unwrap();
        let back = Registry::load(&dir);
        assert_eq!(back.version, REGISTRY_VERSION);
        assert_eq!(back.tasks.len(), 3);
        assert_eq!(back.max_terminal_number(), 9);
        assert_eq!(back.repo("app").unwrap().name, "app");
        assert_eq!(back.repo("c:/SRC/app/").unwrap().name, "app");
        assert!(back.repo("nope").is_none());
        assert_eq!(back.task("y").unwrap().id, "app/y");
        assert_eq!(back.task("lib/x").unwrap().id, "lib/x");
        assert_eq!(back.task("x").unwrap_err().code, codes::TASK_NOT_FOUND);

        // A corrupt file is moved aside, never silently overwritten.
        std::fs::write(Registry::path(&dir), b"{not json").unwrap();
        assert!(Registry::load(&dir).tasks.is_empty());
        let aside = std::fs::read_dir(dir.join("tasks"))
            .unwrap()
            .flatten()
            .any(|e| e.file_name().to_string_lossy().contains("corrupt"));
        assert!(aside);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
