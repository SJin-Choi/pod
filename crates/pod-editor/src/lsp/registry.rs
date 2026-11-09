//! Language detection and the language-server registry.
//!
//! Built-in entries cover rust-analyzer, pyright and
//! typescript-language-server. A JSON file (see [`ServerRegistry::merge_json`])
//! can override or add servers: `%APPDATA%\Pod\lsp.json` for the user and
//! `<worktree>\.pod\lsp.json` per repository.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::Value;

/// A language the editor knows: gpui-component highlighter name + LSP id.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Language {
    /// Name passed to `EditorState::language` (Tree-sitter highlighter).
    pub highlight: &'static str,
    /// `TextDocumentItem.languageId`.
    pub lsp_id: &'static str,
    /// Short label for the status bar.
    pub label: &'static str,
}

const fn lang(highlight: &'static str, lsp_id: &'static str, label: &'static str) -> Language {
    Language { highlight, lsp_id, label }
}

pub fn language_for_path(path: &Path) -> Option<Language> {
    let name = path.file_name()?.to_string_lossy().to_lowercase();
    if name == "cargo.lock" {
        return Some(lang("toml", "toml", "TOML"));
    }
    let ext = path.extension()?.to_string_lossy().to_lowercase();
    Some(match ext.as_str() {
        "rs" => lang("rust", "rust", "Rust"),
        "py" | "pyi" => lang("python", "python", "Python"),
        "ts" | "mts" | "cts" => lang("typescript", "typescript", "TypeScript"),
        "tsx" => lang("tsx", "typescriptreact", "TSX"),
        "js" | "mjs" | "cjs" => lang("javascript", "javascript", "JavaScript"),
        "jsx" => lang("javascript", "javascriptreact", "JSX"),
        "json" | "jsonc" => lang("json", "json", "JSON"),
        "toml" => lang("toml", "toml", "TOML"),
        "md" | "markdown" => lang("markdown", "markdown", "Markdown"),
        _ => return None,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum RootStrategy {
    /// Topmost directory (inside the worktree) containing a marker: Cargo
    /// workspaces, where rust-analyzer wants the workspace root.
    #[default]
    Outermost,
    /// Closest directory containing a marker: JS/Python monorepos.
    Nearest,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct ServerConfig {
    pub id: String,
    /// LSP language ids this server handles.
    pub languages: Vec<String>,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub root_markers: Vec<String>,
    #[serde(default)]
    pub root_strategy: RootStrategy,
    #[serde(default)]
    pub initialization_options: Option<Value>,
    /// Args for an "is it really installed" probe (rustup proxies exist even
    /// when the component is missing). Empty: PATH presence is enough.
    #[serde(default)]
    pub probe_args: Vec<String>,
    /// Shown in the "server not installed" state.
    #[serde(default)]
    pub install_hint: String,
    #[serde(default = "yes")]
    pub enabled: bool,
}

fn yes() -> bool {
    true
}

impl ServerConfig {
    pub fn handles(&self, lsp_id: &str) -> bool {
        self.enabled && self.languages.iter().any(|l| l == lsp_id)
    }
}

#[derive(Clone, Debug)]
pub struct ServerRegistry {
    pub servers: Vec<ServerConfig>,
}

impl Default for ServerRegistry {
    fn default() -> Self {
        let s = |id: &str, langs: &[&str], cmd: &str, args: &[&str], markers: &[&str], strategy, probe: &[&str], hint: &str| ServerConfig {
            id: id.into(),
            languages: langs.iter().map(|x| x.to_string()).collect(),
            command: cmd.into(),
            args: args.iter().map(|x| x.to_string()).collect(),
            root_markers: markers.iter().map(|x| x.to_string()).collect(),
            root_strategy: strategy,
            initialization_options: None,
            probe_args: probe.iter().map(|x| x.to_string()).collect(),
            install_hint: hint.into(),
            enabled: true,
        };
        ServerRegistry {
            servers: vec![
                s("rust-analyzer", &["rust"], "rust-analyzer", &[], &["Cargo.toml"], RootStrategy::Outermost, &["--version"], "rustup component add rust-analyzer"),
                s(
                    "pyright",
                    &["python"],
                    "pyright-langserver",
                    &["--stdio"],
                    &["pyproject.toml", "setup.py", "setup.cfg", "pyrightconfig.json", "requirements.txt"],
                    RootStrategy::Nearest,
                    &[],
                    "npm install -g pyright",
                ),
                s(
                    "typescript-language-server",
                    &["typescript", "typescriptreact", "javascript", "javascriptreact"],
                    "typescript-language-server",
                    &["--stdio"],
                    &["tsconfig.json", "jsconfig.json", "package.json"],
                    RootStrategy::Nearest,
                    &["--version"],
                    "npm install -g typescript typescript-language-server",
                ),
            ],
        }
    }
}

#[derive(Deserialize)]
struct FileFormat {
    #[serde(default)]
    servers: Vec<ServerConfig>,
}

impl ServerRegistry {
    /// Adds or replaces (by `id`) servers from a JSON document:
    /// `{"servers":[{"id":"rust-analyzer","languages":["rust"],"command":"C:/x/ra.exe"}]}`
    pub fn merge_json(&mut self, json: &str) -> Result<(), String> {
        let f: FileFormat = serde_json::from_str(json).map_err(|e| e.to_string())?;
        for s in f.servers {
            match self.servers.iter_mut().find(|x| x.id == s.id) {
                Some(x) => *x = s,
                None => self.servers.push(s),
            }
        }
        Ok(())
    }

    /// Defaults + user file + worktree file (later wins). Bad files are
    /// reported but do not disable the defaults.
    pub fn load(worktree: &Path) -> (Self, Vec<String>) {
        let mut reg = ServerRegistry::default();
        let mut errors = Vec::new();
        let mut files = Vec::new();
        if let Some(appdata) = std::env::var_os("APPDATA") {
            files.push(PathBuf::from(appdata).join("Pod").join("lsp.json"));
        }
        files.push(worktree.join(".pod").join("lsp.json"));
        for f in files {
            if let Ok(s) = std::fs::read_to_string(&f)
                && let Err(e) = reg.merge_json(&s) {
                    errors.push(format!("{}: {e}", f.display()));
                }
        }
        (reg, errors)
    }

    pub fn for_language(&self, lsp_id: &str) -> Option<&ServerConfig> {
        self.servers.iter().find(|s| s.handles(lsp_id))
    }
}

/// Finds the server root for `file`, never leaving `worktree`.
pub fn find_root(file: &Path, worktree: &Path, markers: &[String], strategy: RootStrategy) -> PathBuf {
    let mut found: Option<PathBuf> = None;
    let mut dir = file.parent();
    while let Some(d) = dir {
        if !d.starts_with(worktree) {
            break;
        }
        if markers.iter().any(|m| d.join(m).exists()) {
            found = Some(d.to_path_buf());
            if strategy == RootStrategy::Nearest {
                break;
            }
        }
        if d == worktree {
            break;
        }
        dir = d.parent();
    }
    found.unwrap_or_else(|| worktree.to_path_buf())
}

/// Resolves a command on PATH honouring PATHEXT (`.exe`, `.cmd`, ...).
pub fn which(cmd: &str) -> Option<PathBuf> {
    let p = Path::new(cmd);
    if p.is_absolute() || cmd.contains(['/', '\\']) {
        return p.is_file().then(|| p.to_path_buf());
    }
    let exts: Vec<String> = if cfg!(windows) {
        let pe = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into());
        let mut v: Vec<String> = pe.split(';').filter(|e| !e.is_empty()).map(|e| e.to_lowercase()).collect();
        v.insert(0, String::new());
        v
    } else {
        vec![String::new()]
    };
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        for ext in &exts {
            if cfg!(windows) && ext.is_empty() && p.extension().is_none() {
                continue; // bare names without extension are not runnable on Windows
            }
            let cand = dir.join(format!("{cmd}{ext}"));
            if cand.is_file() {
                return Some(cand);
            }
        }
    }
    None
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Availability {
    Installed(PathBuf),
    /// Not on PATH, or the probe failed (message).
    Missing(String),
}

/// Checks a server can actually run. Blocking (probe up to 10 s): call off
/// the UI thread.
pub fn check_available(cfg: &ServerConfig) -> Availability {
    let Some(exe) = which(&cfg.command) else {
        return Availability::Missing(format!("{} 을(를) PATH에서 찾지 못했습니다", cfg.command));
    };
    if cfg.probe_args.is_empty() {
        return Availability::Installed(exe);
    }
    let mut c = Command::new(&exe);
    c.args(&cfg.probe_args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x0800_0000);
    }
    let Ok(mut child) = c.spawn() else {
        return Availability::Missing(format!("{} 실행 실패", exe.display()));
    };
    let t = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(st)) if st.success() => return Availability::Installed(exe),
            Ok(Some(_)) => {
                let mut err = String::new();
                if let Some(mut e) = child.stderr.take() {
                    let _ = std::io::Read::read_to_string(&mut e, &mut err);
                }
                let first = err.lines().next().unwrap_or("").trim().to_string();
                return Availability::Missing(if first.is_empty() { format!("{} 확인 실패", cfg.id) } else { first });
            }
            Ok(None) if t.elapsed() < Duration::from_secs(10) => std::thread::sleep(Duration::from_millis(50)),
            _ => {
                let _ = child.kill();
                return Availability::Missing(format!("{} 응답 없음", cfg.id));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn languages_by_extension() {
        assert_eq!(language_for_path(Path::new("a/b.rs")).unwrap().lsp_id, "rust");
        assert_eq!(language_for_path(Path::new("x.TSX")).unwrap().highlight, "tsx");
        assert_eq!(language_for_path(Path::new("Cargo.lock")).unwrap().highlight, "toml");
        assert_eq!(language_for_path(Path::new("README.md")).unwrap().lsp_id, "markdown");
        assert!(language_for_path(Path::new("image.png")).is_none());
    }

    #[test]
    fn registry_merge_overrides_and_adds() {
        let mut r = ServerRegistry::default();
        assert_eq!(r.for_language("rust").unwrap().id, "rust-analyzer");
        r.merge_json(r#"{"servers":[{"id":"rust-analyzer","languages":["rust"],"command":"C:/tools/ra.exe","args":["-v"]},{"id":"taplo","languages":["toml"],"command":"taplo","args":["lsp","stdio"],"root_strategy":"nearest"}]}"#).unwrap();
        assert_eq!(r.for_language("rust").unwrap().command, "C:/tools/ra.exe");
        assert_eq!(r.for_language("toml").unwrap().root_strategy, RootStrategy::Nearest);
        assert!(r.merge_json("{oops").is_err());
        r.merge_json(r#"{"servers":[{"id":"taplo","languages":["toml"],"command":"taplo","enabled":false}]}"#).unwrap();
        assert!(r.for_language("toml").is_none());
    }

    #[test]
    fn roots_stay_inside_worktree() {
        let d = tempfile::tempdir().unwrap();
        let w = d.path().join("wt");
        std::fs::create_dir_all(w.join("crates/a/src")).unwrap();
        std::fs::write(d.path().join("Cargo.toml"), "").unwrap(); // outside the worktree
        std::fs::write(w.join("Cargo.toml"), "").unwrap();
        std::fs::write(w.join("crates/a/Cargo.toml"), "").unwrap();
        let f = w.join("crates/a/src/lib.rs");
        let m = vec!["Cargo.toml".to_string()];
        assert_eq!(find_root(&f, &w, &m, RootStrategy::Outermost), w);
        assert_eq!(find_root(&f, &w, &m, RootStrategy::Nearest), w.join("crates/a"));
        assert_eq!(find_root(&f, &w, &["nope".into()], RootStrategy::Nearest), w);
    }

    #[cfg(windows)]
    #[test]
    fn which_uses_pathext() {
        assert!(which("cmd").is_some_and(|p| p.extension().unwrap().eq_ignore_ascii_case("exe")));
        assert!(which("definitely-not-a-real-command-xyz").is_none());
        let missing = ServerConfig { command: "definitely-not-a-real-command-xyz".into(), ..ServerRegistry::default().servers[0].clone() };
        assert!(matches!(check_available(&missing), Availability::Missing(_)));
    }
}
