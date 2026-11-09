//! Debounced recursive file watcher for a worktree root.
//!
//! Raw `notify` events are coalesced on a helper thread into batches after
//! [`DEBOUNCE`] of quiet. Paths under the root `.gitignore` (e.g. `target/`)
//! are dropped so a build does not trigger tree or git refreshes; changes to
//! `.git/index` or `.git/HEAD` are reported as `git_changed` (stage/commit/
//! checkout done by an agent).

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use notify::{RecursiveMode, Watcher as _};

pub const DEBOUNCE: Duration = Duration::from_millis(250);

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Batch {
    /// Changed paths outside `.git`, de-duplicated.
    pub paths: Vec<PathBuf>,
    pub git_changed: bool,
}

pub struct RootWatcher {
    _watcher: notify::RecommendedWatcher,
}

/// Classifies one raw path. Returns `None` to drop it.
pub fn classify(root: &Path, ignore: &ignore::gitignore::Gitignore, path: &Path) -> Option<Result<PathBuf, ()>> {
    let rel = path.strip_prefix(root).ok()?;
    let mut comps = rel.components();
    if let Some(first) = comps.next()
        && first.as_os_str() == ".git" {
            let rest = comps.as_path().to_string_lossy().to_lowercase();
            return (rest == "index" || rest == "head" || rest.starts_with("refs")).then_some(Err(()));
        }
    let is_dir = path.is_dir();
    if ignore.matched_path_or_any_parents(path, is_dir).is_ignore() {
        return None;
    }
    Some(Ok(path.to_path_buf()))
}

impl RootWatcher {
    pub fn start(root: &Path) -> notify::Result<(RootWatcher, UnboundedReceiver<Batch>)> {
        let (raw_tx, raw_rx) = mpsc::channel::<notify::Result<notify::Event>>();
        let mut watcher = notify::recommended_watcher(move |e| {
            let _ = raw_tx.send(e);
        })?;
        watcher.watch(root, RecursiveMode::Recursive)?;
        let (tx, rx) = unbounded();
        let root = root.to_path_buf();
        std::thread::Builder::new().name("pod-editor-watch".into()).spawn(move || debounce_loop(root, raw_rx, tx)).ok();
        Ok((RootWatcher { _watcher: watcher }, rx))
    }
}

fn debounce_loop(root: PathBuf, raw: mpsc::Receiver<notify::Result<notify::Event>>, tx: UnboundedSender<Batch>) {
    let mut builder = ignore::gitignore::GitignoreBuilder::new(&root);
    builder.add(root.join(".gitignore"));
    let ignore = builder.build().unwrap_or_else(|_| ignore::gitignore::Gitignore::empty());
    let mut batch = Batch::default();
    let mut deadline: Option<Instant> = None;
    loop {
        let msg = match deadline {
            Some(d) => raw.recv_timeout(d.saturating_duration_since(Instant::now())),
            None => raw.recv().map_err(|_| mpsc::RecvTimeoutError::Disconnected),
        };
        match msg {
            Ok(Ok(ev)) => {
                if matches!(ev.kind, notify::EventKind::Access(_)) {
                    continue;
                }
                for p in ev.paths {
                    match classify(&root, &ignore, &p) {
                        Some(Ok(p)) => {
                            if !batch.paths.contains(&p) {
                                batch.paths.push(p);
                            }
                        }
                        Some(Err(())) => batch.git_changed = true,
                        None => continue,
                    }
                    deadline.get_or_insert_with(|| Instant::now() + DEBOUNCE);
                }
            }
            Ok(Err(_)) => {
                // Overflow or watcher error: ask for a full refresh.
                batch.paths.push(root.clone());
                batch.git_changed = true;
                deadline.get_or_insert_with(|| Instant::now() + DEBOUNCE);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                deadline = None;
                let b = std::mem::take(&mut batch);
                if (!b.paths.is_empty() || b.git_changed) && tx.unbounded_send(b).is_err() {
                    return;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_filters_ignored_and_git() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        std::fs::write(r.join(".gitignore"), "target/\n").unwrap();
        std::fs::create_dir_all(r.join("target")).unwrap();
        let mut b = ignore::gitignore::GitignoreBuilder::new(r);
        b.add(r.join(".gitignore"));
        let ig = b.build().unwrap();
        assert_eq!(classify(r, &ig, &r.join("src/a.rs")), Some(Ok(r.join("src/a.rs"))));
        assert_eq!(classify(r, &ig, &r.join("target/debug/x.o")), None);
        assert_eq!(classify(r, &ig, &r.join(".git/index")), Some(Err(())));
        assert_eq!(classify(r, &ig, &r.join(".git/objects/ab/cd")), None);
    }

    #[test]
    fn batches_real_changes() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().canonicalize().unwrap();
        let (_w, mut rx) = RootWatcher::start(&root).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        std::fs::write(root.join("a.txt"), "1").unwrap();
        std::fs::write(root.join("b.txt"), "2").unwrap();
        let mut batch = None;
        for _ in 0..50 {
            if let Ok(b) = rx.try_recv() {
                batch = Some(b);
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let batch = batch.expect("a batch within 5s");
        let names: Vec<_> = batch.paths.iter().filter_map(|p| p.file_name()).map(|n| n.to_string_lossy().into_owned()).collect();
        assert!(names.contains(&"a.txt".to_string()), "{names:?}");
    }
}
