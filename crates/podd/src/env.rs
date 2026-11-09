//! Terminal environment hygiene.
//!
//! `pod` auto-starts `podd` from whatever shell it happens to run in: an agent
//! tool shell, another IDE's terminal, VS Code, Windows Terminal... Variables from
//! that launcher must not leak into the terminals we start. Examples seen in
//! practice: `NO_COLOR=1` (exported by agent tool shells) turns off Claude
//! Code's colors, and `CLAUDECODE` / `CLAUDE_CODE_*` session markers make
//! Claude believe it is a child session (no transcript, warning banner).
//!
//! The daemon therefore builds each terminal's environment as:
//! 1. the user's *registry* environment (what a fresh logon would get,
//!    `CreateEnvironmentBlock`), falling back to the daemon's own environment;
//! 2. minus everything [`is_leaky`] matches;
//! 3. plus `TERM`, `COLORTERM`, `POD_TERMINAL_ID`, `POD_SOCKET`;
//! 4. plus the per-run `env` overrides from `terminal.run`, which may set anything.

use std::collections::BTreeMap;

/// Exact names (case-insensitive) that describe the *launching* terminal or agent.
const DROP_EXACT: &[&str] = &[
    "NO_COLOR",
    "FORCE_COLOR",
    "CLICOLOR_FORCE",
    "CLAUDECODE",
    "CLAUDE_PID",
    "CLAUDE_EFFORT",
    "TERM",
    "TERM_PROGRAM",
    "TERM_PROGRAM_VERSION",
    "TERM_SESSION_ID",
    "COLORTERM",
    "WT_SESSION",
    "WT_PROFILE_ID",
    "ConEmuPID",
    "ConEmuANSI",
    "POD_TERMINAL_ID",
    "POD_SOCKET",
    "HC_TERMINAL_ID",
];

/// Prefixes (case-insensitive) of session / sandbox / host markers, including
/// the markers that parent IDEs and terminal multiplexers export.
const DROP_PREFIX: &[&str] = &[
    "CLAUDE_CODE_",
    "CODEX_",
    "ORCA_",
    "VSCODE_",
    "WT_",
    "HERDR_",
    "HC_",
];

/// User configuration that shares a dropped prefix but is not a session marker.
const KEEP: &[&str] = &[
    "CODEX_HOME",
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_VERTEX",
    "CLAUDE_CODE_SKIP_BEDROCK_AUTH",
    "CLAUDE_CODE_SKIP_VERTEX_AUTH",
    "CLAUDE_CODE_GIT_BASH_PATH",
    "CLAUDE_CODE_MAX_OUTPUT_TOKENS",
    "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC",
];

/// True for variables that belong to the launcher, not to the user.
pub fn is_leaky(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    if KEEP.iter().any(|k| *k == upper) {
        return false;
    }
    DROP_EXACT.iter().any(|d| d.eq_ignore_ascii_case(name))
        || DROP_PREFIX.iter().any(|p| upper.starts_with(p))
}

/// Remove launcher markers from an environment map.
pub fn sanitize(env: impl IntoIterator<Item = (String, String)>) -> BTreeMap<String, String> {
    env.into_iter()
        .filter(|(k, _)| !k.is_empty() && !k.starts_with('=') && !is_leaky(k))
        .collect()
}

/// Insert or replace `key`, matching existing keys case-insensitively (Windows semantics).
pub fn set_var(env: &mut BTreeMap<String, String>, key: &str, value: impl Into<String>) {
    if let Some(existing) = env.keys().find(|k| k.eq_ignore_ascii_case(key)).cloned() {
        env.remove(&existing);
    }
    env.insert(key.to_string(), value.into());
}

/// Look up a variable case-insensitively.
pub fn get_var<'a>(env: &'a BTreeMap<String, String>, key: &str) -> Option<&'a str> {
    env.iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(key))
        .map(|(_, v)| v.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn drops_launcher_markers_and_keeps_user_config() {
        let out = sanitize(env(&[
            ("PATH", r"C:\bin"),
            ("NO_COLOR", "1"),
            ("no_color", "1"),
            ("CLAUDECODE", "1"),
            ("CLAUDE_CODE_CHILD_SESSION", "1"),
            ("CLAUDE_CODE_SESSION_ID", "x"),
            ("CLAUDE_CODE_ENTRYPOINT", "cli"),
            ("CLAUDE_PID", "42"),
            ("CODEX_SANDBOX", "seatbelt"),
            ("CODEX_SANDBOX_NETWORK_DISABLED", "1"),
            ("ORCA_TERMINAL_HANDLE", "term_x"),
            ("ORCA_AGENT_HOOK_TOKEN", "secret"),
            ("TERM_PROGRAM", "vscode"),
            ("TERM", "dumb"),
            ("WT_SESSION", "guid"),
            ("POD_TERMINAL_ID", "t9"),
            ("=C:", r"C:\"),
            ("CODEX_HOME", r"C:\codex"),
            ("CLAUDE_CODE_USE_BEDROCK", "1"),
            ("CLAUDE_CONFIG_DIR", r"C:\cfg"),
            ("USERPROFILE", r"C:\Users\me"),
        ]));
        let keys: Vec<&str> = out.keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            [
                "CLAUDE_CODE_USE_BEDROCK",
                "CLAUDE_CONFIG_DIR",
                "CODEX_HOME",
                "PATH",
                "USERPROFILE"
            ]
        );
    }

    #[test]
    fn set_var_is_case_insensitive() {
        let mut e = sanitize(env(&[("Path", "a")]));
        set_var(&mut e, "PATH", "b");
        assert_eq!(e.len(), 1);
        assert_eq!(get_var(&e, "path"), Some("b"));
    }
}
