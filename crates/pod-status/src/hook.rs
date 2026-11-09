//! L1 hook adapter: the wire message `pod-hook` sends to podd, and the mapping
//! of Claude Code / Codex hook events (stdin JSON payloads) to Pod states.
//!
//! Payload field names follow docs/04 §3.3 and the public
//! Claude Code hook schema. Fields marked "unverified" in [`PAYLOAD_FIELDS`]
//! were never seen in a real payload during research.

use crate::types::{AgentState, BlockedKind, Outcome};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Wire version of [`HookMessage`].
pub const HOOK_MESSAGE_VERSION: u32 = 1;

/// Env vars `pod-hook` reads (injected into each PTY by podd).
pub const ENV_TERMINAL_ID: &str = "POD_TERMINAL_ID";
pub const ENV_SOCKET: &str = "POD_SOCKET";
pub const ENV_HOOK_TOKEN: &str = "POD_HOOK_TOKEN";

/// Which agent CLI invoked the hook.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HookAgent {
    Claude,
    Codex,
}

impl HookAgent {
    pub fn parse(s: &str) -> Option<HookAgent> {
        match s.to_ascii_lowercase().as_str() {
            "claude" | "claude-code" => Some(HookAgent::Claude),
            "codex" => Some(HookAgent::Codex),
            _ => None,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            HookAgent::Claude => "claude",
            HookAgent::Codex => "codex",
        }
    }
}

/// One JSON line sent by `pod-hook` over the podd named pipe.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HookMessage {
    /// Always [`HOOK_MESSAGE_VERSION`].
    pub v: u32,
    /// `POD_TERMINAL_ID` of the PTY the agent runs in.
    pub terminal_id: String,
    /// `POD_HOOK_TOKEN`, if podd injected one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    pub agent: HookAgent,
    /// Event name from argv (falls back to `payload.hook_event_name`).
    pub event: String,
    /// The hook's stdin JSON as-is (`null` if stdin was empty or not JSON).
    #[serde(default)]
    pub payload: Value,
    /// Wall clock (unix ms) when `pod-hook` ran; informational only.
    #[serde(default)]
    pub sent_at_unix_ms: u64,
}

impl HookMessage {
    pub fn new(
        terminal_id: impl Into<String>,
        agent: HookAgent,
        event: impl Into<String>,
        payload: Value,
    ) -> Self {
        Self {
            v: HOOK_MESSAGE_VERSION,
            terminal_id: terminal_id.into(),
            token: None,
            agent,
            event: event.into(),
            payload,
            sent_at_unix_ms: 0,
        }
    }

    /// Serialize as one line (no trailing newline).
    pub fn to_line(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "{}".into())
    }

    pub fn from_line(line: &str) -> serde_json::Result<HookMessage> {
        serde_json::from_str(line.trim_end())
    }

    /// The effective event name.
    pub fn event_name(&self) -> &str {
        if !self.event.is_empty() {
            return &self.event;
        }
        self.payload
            .get("hook_event_name")
            .and_then(Value::as_str)
            .unwrap_or("")
    }
}

/// What a hook event does to the root (lead agent) state.
#[derive(Debug, Clone, PartialEq)]
pub enum RootEffect {
    /// No change to the root state.
    None,
    Set {
        state: AgentState,
        msg: Option<String>,
    },
    /// Return to the state held before the last wait (permission/question).
    RestoreFromWait,
    /// The session ended (the process will exit soon); hook evidence is dropped.
    EndSession,
}

/// What a hook event does to a sub-record (subagent).
#[derive(Debug, Clone, PartialEq)]
pub enum ChildEffect {
    Set {
        id: String,
        state: AgentState,
        title: Option<String>,
    },
    Remove {
        id: String,
    },
}

/// The interpretation of one hook event.
#[derive(Debug, Clone, PartialEq)]
pub struct HookEffect {
    pub root: RootEffect,
    pub child: Option<ChildEffect>,
    /// New session: clear sub-records.
    pub session_boundary: bool,
    pub outcome: Option<Outcome>,
    pub session_id: Option<String>,
    pub tool_name: Option<String>,
}

impl HookEffect {
    fn none() -> Self {
        Self {
            root: RootEffect::None,
            child: None,
            session_boundary: false,
            outcome: None,
            session_id: None,
            tool_name: None,
        }
    }
    fn set(state: AgentState, msg: Option<String>) -> Self {
        Self {
            root: RootEffect::Set { state, msg },
            ..Self::none()
        }
    }
}

/// Payload field reference: `(agent, event, field, verified)`. `verified` =
/// seen in a real payload or only in documentation (docs/04 §3.3). Unverified
/// fields are read defensively (absence is fine).
pub static PAYLOAD_FIELDS: &[(&str, &str, &str, bool)] = &[
    ("claude", "*", "session_id", true),
    ("claude", "*", "hook_event_name", true),
    ("claude", "*", "transcript_path", true),
    ("claude", "*", "cwd", true),
    ("claude", "*", "agent_id", true),
    ("claude", "SessionStart", "source", true),
    ("claude", "UserPromptSubmit", "prompt", true),
    ("claude", "PreToolUse", "tool_name", true),
    ("claude", "PreToolUse", "tool_use_id", true),
    ("claude", "PermissionRequest", "tool_name", true),
    ("claude", "Elicitation", "message", false),
    ("claude", "Elicitation", "mcp_server_name", false),
    ("claude", "Stop", "is_interrupt", false),
    ("claude", "StopFailure", "error", false),
    ("claude", "SubagentStart", "agent_type", true),
    ("claude", "SessionEnd", "reason", false),
    ("codex", "*", "session_id", false),
    ("codex", "*", "turn_id", false),
    ("codex", "*", "transcript_path", false),
    ("codex", "*", "agent_id", false),
    ("codex", "PreToolUse", "tool_name", false),
    ("codex", "PermissionRequest", "tool_name", false),
];

/// Tool names that mean "the agent asks the user a question".
pub static QUESTION_TOOLS: &[&str] = &["AskUserQuestion", "request_user_input"];

fn s(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(Value::as_str)
        .filter(|x| !x.is_empty())
        .map(str::to_string)
}

fn child_id(agent_id: &str) -> String {
    let clean: String = agent_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "_.+-".contains(c) {
                c
            } else {
                '-'
            }
        })
        .take(32)
        .collect();
    format!("hook/{clean}")
}

/// Interpret a hook event. Unknown events map to "no effect".
pub fn map_hook(agent: HookAgent, event: &str, payload: &Value) -> HookEffect {
    let mut eff = match agent {
        HookAgent::Claude => map_claude(event, payload),
        HookAgent::Codex => map_codex(event, payload),
    };
    eff.session_id = s(payload, "session_id");
    if eff.tool_name.is_none() {
        eff.tool_name = s(payload, "tool_name");
    }
    eff
}

/// Convenience: interpret a [`HookMessage`].
pub fn map_message(msg: &HookMessage) -> HookEffect {
    map_hook(msg.agent, msg.event_name(), &msg.payload)
}

fn map_claude(event: &str, p: &Value) -> HookEffect {
    let agent_id = s(p, "agent_id");
    let tool = s(p, "tool_name");
    match event {
        "SessionStart" => {
            if s(p, "source").as_deref() == Some("compact") {
                return HookEffect::none();
            }
            HookEffect {
                session_boundary: true,
                ..HookEffect::set(AgentState::Idle, None)
            }
        }
        "UserPromptSubmit" => HookEffect::set(AgentState::Working, None),
        "PreToolUse" | "PostToolUse" | "PostToolUseFailure" if agent_id.is_some() => {
            // Activity inside a subagent: update the child, leave the lead alone.
            HookEffect {
                child: Some(ChildEffect::Set {
                    id: child_id(agent_id.as_deref().unwrap()),
                    state: AgentState::Working,
                    title: None,
                }),
                ..HookEffect::none()
            }
        }
        "PreToolUse" => {
            if tool.as_deref().is_some_and(|t| QUESTION_TOOLS.contains(&t)) {
                HookEffect::set(AgentState::Blocked(BlockedKind::Question), tool)
            } else {
                HookEffect::set(AgentState::Working, None)
            }
        }
        "PostToolUse" | "PostToolUseFailure" => HookEffect::set(AgentState::Working, None),
        "PermissionRequest" => HookEffect::set(AgentState::Blocked(BlockedKind::Permission), tool),
        "Elicitation" => {
            HookEffect::set(AgentState::Blocked(BlockedKind::Question), s(p, "message"))
        }
        "ElicitationResult" => HookEffect {
            root: RootEffect::RestoreFromWait,
            ..HookEffect::none()
        },
        "Stop" => {
            if p.get("is_interrupt").and_then(Value::as_bool) == Some(true) {
                HookEffect {
                    outcome: Some(Outcome::Cancelled),
                    ..HookEffect::set(AgentState::Idle, None)
                }
            } else {
                HookEffect {
                    outcome: Some(Outcome::Completed),
                    ..HookEffect::set(AgentState::Done, None)
                }
            }
        }
        "StopFailure" => {
            let msg = s(p, "error").or_else(|| s(p, "error_message"));
            HookEffect {
                outcome: Some(Outcome::Failed),
                ..HookEffect::set(AgentState::Error, msg)
            }
        }
        "SubagentStart" => match agent_id {
            Some(id) => HookEffect {
                child: Some(ChildEffect::Set {
                    id: child_id(&id),
                    state: AgentState::Working,
                    title: s(p, "agent_type"),
                }),
                ..HookEffect::none()
            },
            None => HookEffect::none(),
        },
        "SubagentStop" => match agent_id {
            Some(id) => HookEffect {
                child: Some(ChildEffect::Remove { id: child_id(&id) }),
                ..HookEffect::none()
            },
            None => HookEffect::none(),
        },
        "SessionEnd" => HookEffect {
            root: RootEffect::EndSession,
            ..HookEffect::none()
        },
        _ => HookEffect::none(),
    }
}

fn map_codex(event: &str, p: &Value) -> HookEffect {
    let tool = s(p, "tool_name");
    match event {
        "SessionStart" => HookEffect {
            session_boundary: true,
            ..HookEffect::set(AgentState::Idle, None)
        },
        "UserPromptSubmit" | "PostToolUse" => HookEffect::set(AgentState::Working, None),
        "PreToolUse" => {
            if tool.as_deref().is_some_and(|t| QUESTION_TOOLS.contains(&t)) {
                HookEffect::set(AgentState::Blocked(BlockedKind::Question), tool)
            } else {
                HookEffect::set(AgentState::Working, None)
            }
        }
        "PermissionRequest" => HookEffect::set(AgentState::Blocked(BlockedKind::Permission), tool),
        "Stop" => HookEffect {
            outcome: Some(Outcome::Completed),
            ..HookEffect::set(AgentState::Done, None)
        },
        "Interrupt" => HookEffect {
            outcome: Some(Outcome::Cancelled),
            ..HookEffect::set(AgentState::Idle, None)
        },
        "SubagentStart" => match s(p, "agent_id") {
            Some(id) => HookEffect {
                child: Some(ChildEffect::Set {
                    id: child_id(&id),
                    state: AgentState::Working,
                    title: s(p, "agent_type"),
                }),
                ..HookEffect::none()
            },
            None => HookEffect::none(),
        },
        "SubagentStop" => match s(p, "agent_id") {
            Some(id) => HookEffect {
                child: Some(ChildEffect::Remove { id: child_id(&id) }),
                ..HookEffect::none()
            },
            None => HookEffect::none(),
        },
        _ => HookEffect::none(),
    }
}

/// Hook events Pod installs, per agent (docs/04 §4.3).
pub static CLAUDE_EVENTS: &[&str] = &[
    "SessionStart",
    "UserPromptSubmit",
    "PreToolUse",
    "PostToolUse",
    "PostToolUseFailure",
    "PermissionRequest",
    "Elicitation",
    "ElicitationResult",
    "Stop",
    "StopFailure",
    "SubagentStart",
    "SubagentStop",
    "SessionEnd",
];

pub static CODEX_EVENTS: &[&str] = &[
    "SessionStart",
    "UserPromptSubmit",
    "PreToolUse",
    "PermissionRequest",
    "PostToolUse",
    "Stop",
    "Interrupt",
    "SubagentStart",
    "SubagentStop",
];

/// Claude events that take a `matcher` (tool/server name) in settings.json.
pub static CLAUDE_MATCHER_EVENTS: &[&str] = &[
    "PreToolUse",
    "PostToolUse",
    "PostToolUseFailure",
    "PermissionRequest",
    "Elicitation",
    "ElicitationResult",
];
