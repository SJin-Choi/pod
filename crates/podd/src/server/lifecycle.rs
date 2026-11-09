//! v9 lifecycle events: `terminal.changed`, `task.changed`, `task.removing`
//! and `task.touch` (docs/03 §4.3).
//!
//! Terminal and task create/remove events are published where those things
//! happen (`run`, `rm`, `Terminal::on_frame`, `task_create`, `task_remove`).
//! This module adds what needs state: a worker that keeps the last announced
//! aggregate status and git facts per task and publishes `task.changed` when
//! they differ.
//!
//! - **status**: recomputed on every `status.changed` of a task terminal (no
//!   git, cheap).
//! - **git** (ahead, behind, dirty, unpushed, HEAD): refreshed when a task
//!   terminal's state changed (an agent turn usually edits or commits), on
//!   `task.touch`, and every [`GIT_POLL`] while at least one lifecycle
//!   subscriber is connected (edits made outside Pod). Without lifecycle
//!   subscribers nothing is polled.
//!
//! The first facts of a task are a baseline and produce no event.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use tokio::sync::broadcast;

use super::status::{self, HubEvent, lifecycle_subscribers, publish_task, task_event};
use super::tasks::{TaskRecord, blocking};
use super::{Daemon, log};
use crate::git;
use crate::protocol::*;

/// Git facts poll period while a lifecycle subscriber is connected.
pub const GIT_POLL: Duration = Duration::from_secs(5);
/// How often queued git refreshes run (coalesces bursts of status events).
const TICK: Duration = Duration::from_millis(250);
/// Default wait between `task.removing` and deleting the worktree when a
/// lifecycle subscriber is connected (`POD_TASK_REMOVE_GRACE_MS`).
pub const DEFAULT_REMOVE_GRACE_MS: u64 = 300;
const MAX_REMOVE_GRACE_MS: u64 = 10_000;

/// What was last announced for one task.
#[derive(Default)]
struct Snap {
    status: Option<TaskStatus>,
    git: Option<TaskGit>,
}

fn snaps() -> &'static Mutex<HashMap<String, Snap>> {
    static S: OnceLock<Mutex<HashMap<String, Snap>>> = OnceLock::new();
    S.get_or_init(Default::default)
}

/// Grace between `task.removing` and deletion.
pub(super) fn remove_grace_ms() -> u64 {
    if lifecycle_subscribers() == 0 {
        return 0;
    }
    std::env::var("POD_TASK_REMOVE_GRACE_MS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_REMOVE_GRACE_MS)
        .min(MAX_REMOVE_GRACE_MS)
}

/// Record a new git fact; returns whether it differs from an announced one.
/// `None` facts (unknown before) are a baseline.
fn store_git(task: &str, git: &TaskGit) -> bool {
    let mut s = snaps().lock().unwrap();
    let snap = s.entry(task.to_string()).or_default();
    let changed = snap.git.as_ref().is_some_and(|g| g != git);
    snap.git = Some(git.clone());
    changed
}

fn store_status(task: &str, status: &Option<TaskStatus>) -> bool {
    let mut s = snaps().lock().unwrap();
    let snap = s.entry(task.to_string()).or_default();
    let changed = snap.status != *status;
    snap.status = status.clone();
    changed
}

/// `task.create` finished: baseline + `task.changed created`.
pub(super) fn task_created(info: &TaskInfo) {
    {
        let mut s = snaps().lock().unwrap();
        s.insert(
            info.id.clone(),
            Snap {
                status: info.status.clone(),
                git: info.git.clone(),
            },
        );
    }
    let mut ev = task_event(&info.id, TaskChange::Created);
    ev.task = Some(Box::new(info.clone()));
    publish_task(ev);
}

/// `task.remove` finished.
pub(super) fn task_removed(id: &str) {
    snaps().lock().unwrap().remove(id);
    publish_task(task_event(id, TaskChange::Removed));
}

/// Start the worker (after rediscovery, so terminals are known).
pub(super) fn start(daemon: &Arc<Daemon>) {
    let daemon = daemon.clone();
    let mut rx = status::subscribe();
    tokio::spawn(async move {
        // Baseline of every task (no events), then follow changes.
        let mut due: HashSet<String> = daemon.task_ids().into_iter().collect();
        for id in &due {
            let st = daemon.task_status_now(id);
            store_status(id, &st);
        }
        let mut baseline = true;
        let mut tick = tokio::time::interval(TICK);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut last_poll = Instant::now();
        loop {
            tokio::select! {
                ev = rx.recv() => match ev {
                    Ok(HubEvent::Status(ev)) => {
                        for task in daemon.tasks_of_terminal(&ev.id) {
                            daemon.check_task_status(&task);
                            // Git only on a state transition (`from` set), not
                            // on every confidence/progress update.
                            if ev.from.is_some() || ev.removed {
                                due.insert(task);
                            }
                        }
                    }
                    Ok(_) => {}
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        for task in daemon.task_ids() {
                            daemon.check_task_status(&task);
                            due.insert(task);
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => return,
                },
                _ = tick.tick() => {
                    let subscribed = lifecycle_subscribers() > 0;
                    if subscribed && last_poll.elapsed() >= GIT_POLL {
                        due.extend(daemon.task_ids());
                        last_poll = Instant::now();
                    }
                    if !subscribed && !baseline {
                        // Nobody listens: keep the last facts, poll nothing.
                        due.clear();
                        continue;
                    }
                    baseline = false;
                    let ids: Vec<String> = due.drain().collect();
                    for id in ids {
                        if let Ok(rec) = daemon.task_record(&id) {
                            daemon.refresh_git(&rec).await;
                        }
                    }
                }
            }
        }
    });
}

impl Daemon {
    fn task_ids(&self) -> Vec<String> {
        self.registry
            .lock()
            .unwrap()
            .tasks
            .iter()
            .map(|t| t.id.clone())
            .collect()
    }

    pub(super) fn tasks_of_terminal(&self, terminal: &str) -> Vec<String> {
        self.registry
            .lock()
            .unwrap()
            .tasks
            .iter()
            .filter(|t| t.terminals.iter().any(|x| x == terminal))
            .map(|t| t.id.clone())
            .collect()
    }

    /// The task's aggregate status right now (no git).
    pub(super) fn task_status_now(&self, task: &str) -> Option<TaskStatus> {
        let ids = self.task_record(task).ok()?.terminals;
        let infos: Vec<TerminalInfo> = {
            let terms: Vec<Arc<super::terminal::Terminal>> = self.terminals.lock().unwrap().clone();
            terms
                .iter()
                .filter(|t| ids.contains(&t.id))
                .map(|t| t.info())
                .collect()
        };
        // Keep the task's terminal order (ties go to the first terminal).
        let ordered: Vec<&TerminalInfo> = ids
            .iter()
            .filter_map(|id| infos.iter().find(|i| &i.id == id))
            .collect();
        aggregate_task_status(ordered)
    }

    /// Publish `task.changed status` when the aggregate differs from the last one.
    pub(super) fn check_task_status(&self, task: &str) {
        if self.task_record(task).is_err() {
            return;
        }
        let st = self.task_status_now(task);
        if store_status(task, &st) {
            let mut ev = task_event(task, TaskChange::Status);
            ev.status = st;
            publish_task(ev);
        }
    }

    /// The task's terminal list changed: `task.changed terminals` (+ status).
    pub(super) fn task_terminals_changed(&self, task: &str) {
        let Ok(rec) = self.task_record(task) else {
            return;
        };
        let mut ev = task_event(task, TaskChange::Terminals);
        ev.terminals = rec.terminals;
        publish_task(ev);
        self.check_task_status(task);
    }

    /// Fresh git facts; publishes `task.changed git` when they differ from
    /// the last announced ones. Returns (facts, error, changed).
    pub(super) async fn refresh_git(&self, rec: &TaskRecord) -> (Option<TaskGit>, Option<String>, bool) {
        let wt = PathBuf::from(&rec.worktree);
        if !wt.is_dir() {
            return (None, Some("worktree directory is missing".into()), false);
        }
        let (base, commit) = (rec.base.clone(), rec.base_commit.clone());
        match blocking(move || git::facts(&wt, &base, &commit)).await {
            Ok(Ok(g)) => {
                // The task may have been removed meanwhile: no event, no snapshot.
                if self.task_record(&rec.id).is_err() {
                    return (Some(g), None, false);
                }
                let changed = store_git(&rec.id, &g);
                if changed {
                    let mut ev = task_event(&rec.id, TaskChange::Git);
                    ev.git = Some(g.clone());
                    publish_task(ev);
                }
                (Some(g), None, changed)
            }
            Ok(Err(e)) => (None, Some(e), false),
            Err(e) => (None, Some(e.message), false),
        }
    }

    /// `task.touch`: refresh git now, then `task.changed touched`.
    pub(super) async fn task_touch(&self, p: TaskTouchParams) -> Result<TaskTouchResult, RpcError> {
        let rec = self.task_record(&p.task)?;
        let (git, git_error, git_changed) = self.refresh_git(&rec).await;
        let mut ev = task_event(&rec.id, TaskChange::Touched);
        ev.paths = p.paths;
        publish_task(ev);
        if let Some(e) = &git_error {
            log(&format!("task.touch {}: {e}", rec.id));
        }
        Ok(TaskTouchResult {
            task: rec.id,
            git,
            git_error,
            git_changed,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn g(dirty: u32, ahead: u32) -> TaskGit {
        TaskGit {
            dirty,
            ahead,
            ..Default::default()
        }
    }

    #[test]
    fn git_baseline_then_changes() {
        let id = "lifecycle-test/git";
        assert!(!store_git(id, &g(0, 0)), "first facts are a baseline");
        assert!(!store_git(id, &g(0, 0)));
        assert!(store_git(id, &g(1, 0)), "dirty changed");
        assert!(store_git(id, &g(1, 1)), "ahead changed");
        assert!(!store_git(id, &g(1, 1)));
        snaps().lock().unwrap().remove(id);
    }

    #[test]
    fn status_changes_are_detected() {
        let id = "lifecycle-test/status";
        let st = |state: &str| {
            Some(TaskStatus {
                state: state.into(),
                kind: None,
                terminal: "t1".into(),
                badge: state.into(),
                terminals: 1,
            })
        };
        assert!(store_status(id, &st("idle")), "unknown -> idle");
        assert!(!store_status(id, &st("idle")));
        assert!(store_status(id, &st("working")));
        assert!(store_status(id, &None), "status gone");
        snaps().lock().unwrap().remove(id);
    }
}
