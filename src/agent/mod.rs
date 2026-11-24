//! The boundary between TJ's UI and a coding agent.
//!
//! A backend runs one agent session and reports what happens as [`AgentEvent`]s on a
//! channel; the UI drives it through [`AgentSession`]. Claude is the only backend today
//! ([`claude`]); [`ScriptedAgent`] replays canned events for demos and tests.

pub mod claude;

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Which coding agent runs a thread. Claude is the only backend today; the others are the
/// agents T3 Code supports, listed so a thread can name one (and show its mark) once TJ
/// grows a backend for it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentKind {
    #[default]
    Claude,
    Codex,
    OpenCode,
    Cursor,
    Grok,
    Pi,
    Kiro,
    Muse,
    Fx,
    Antigravity,
}

impl AgentKind {
    #[cfg(test)]
    pub const ALL: [AgentKind; 10] = [
        Self::Claude,
        Self::Codex,
        Self::OpenCode,
        Self::Cursor,
        Self::Grok,
        Self::Pi,
        Self::Kiro,
        Self::Muse,
        Self::Fx,
        Self::Antigravity,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Claude => "Claude",
            Self::Codex => "Codex",
            Self::OpenCode => "OpenCode",
            Self::Cursor => "Cursor",
            Self::Grok => "Grok",
            Self::Pi => "Pi",
            Self::Kiro => "Kiro",
            Self::Muse => "Muse Code",
            Self::Fx => "fx",
            Self::Antigravity => "Antigravity",
        }
    }
}

/// How much the agent may do without asking. Mirrors Claude Code's permission modes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize, Eq)]
pub enum PermissionMode {
    /// Ask before anything risky.
    #[default]
    Ask,
    /// File edits go through; commands still ask.
    AcceptEdits,
    /// Claude Code's classifier decides what needs asking.
    Auto,
    /// Read-only planning.
    Plan,
    /// Nothing asks.
    FullAccess,
}

impl PermissionMode {
    pub const ALL: [PermissionMode; 5] = [Self::Ask, Self::AcceptEdits, Self::Auto, Self::Plan, Self::FullAccess];

    /// The value Claude Code uses on the command line and in `set_permission_mode`.
    pub fn claude_name(self) -> &'static str {
        match self {
            Self::Ask => "default",
            Self::AcceptEdits => "acceptEdits",
            Self::Auto => "auto",
            Self::Plan => "plan",
            Self::FullAccess => "bypassPermissions",
        }
    }

    pub fn from_claude_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|mode| mode.claude_name() == name)
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Ask => "Ask",
            Self::AcceptEdits => "Accept edits",
            Self::Auto => "Auto",
            Self::Plan => "Plan",
            Self::FullAccess => "Full access",
        }
    }
}

/// The agent wants to use a tool and is waiting for the user.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PermissionRequest {
    /// Correlates the answer; chosen by the agent.
    pub id: String,
    pub tool_name: String,
    pub tool_use_id: Option<String>,
    /// The agent's own one-line description of the call, when it gave one.
    pub description: Option<String>,
    pub input: Value,
    /// Rules the agent suggests saving if the user approves for the session.
    pub suggestions: Vec<Value>,
    /// Why the call needs approval (for example a path outside the project).
    pub reason: Option<String>,
    /// The agent says this call must not get a standing "approve for session" grant.
    pub no_standing_grant: bool,
}

/// The user's answer to a [`PermissionRequest`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Decision {
    Approve,
    /// Approve, and stop asking for calls like this until the session ends.
    ApproveForSession,
    /// Refuse; the optional feedback is passed on to the agent.
    Decline {
        feedback: Option<String>,
    },
    /// Refuse and stop the turn.
    Cancel,
    /// Answer an AskUserQuestion: question text -> the chosen label(s) or the user's own words.
    Answer {
        answers: std::collections::BTreeMap<String, String>,
    },
    /// Approve a plan (ExitPlanMode) and start working, accepting edits or asking first.
    StartPlan {
        accept_edits: bool,
    },
}

/// A usage window on a subscription, such as the 5-hour or 7-day limit.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UsageWindow {
    /// The agent's name for the window, e.g. `five_hour` or `seven_day`.
    pub id: String,
    /// Fraction used, 0.0 to 1.0.
    pub utilization: f64,
    /// Unix seconds when the window resets.
    pub resets_at: Option<i64>,
}

/// What happened in the session, in order.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum AgentEvent {
    /// The process is up and has said who it is.
    Started {
        version: Option<String>,
        permission_mode: Option<PermissionMode>,
    },
    /// The session id and model are known. Arrives with every new conversation turn.
    SessionInfo {
        session_id: String,
        model: Option<String>,
    },
    /// The model that answered (it changes when the model is switched mid-session).
    ModelInUse(String),
    /// A new block of assistant text begins; following deltas belong to it.
    TextStarted,
    TextDelta(String),
    /// A tool call, with its complete input.
    ToolStarted {
        id: String,
        name: String,
        input: Value,
    },
    ToolFinished {
        id: String,
        output: String,
        is_error: bool,
    },
    PermissionRequested(PermissionRequest),
    /// The agent no longer needs the answer (for example the turn was interrupted).
    PermissionWithdrawn {
        id: String,
    },
    Usage(Vec<UsageWindow>),
    /// How full the context is: tokens in the latest request, and the model's window.
    Context {
        tokens: Option<u64>,
        window: Option<u64>,
    },
    /// A subagent (the Agent/Task tool call `tool_use_id`) started working, often in the background.
    SubagentStarted {
        tool_use_id: String,
        description: String,
        agent_type: Option<String>,
    },
    /// What a subagent is doing now, with its running totals.
    SubagentProgress {
        tool_use_id: String,
        description: String,
        tool_uses: Option<u64>,
        tokens: Option<u64>,
    },
    /// A subagent called a tool.
    SubagentTool {
        tool_use_id: String,
        name: String,
        input: Value,
    },
    /// A subagent said something (usually its report).
    SubagentText {
        tool_use_id: String,
        text: String,
    },
    /// A subagent finished; `summary` is its report.
    SubagentFinished {
        tool_use_id: String,
        status: String,
        summary: Option<String>,
    },
    TurnFinished {
        cost_usd: Option<f64>,
        duration_ms: Option<u64>,
        is_error: bool,
        error: Option<String>,
    },
    /// Something went wrong that the user should see; the session may continue.
    Error(String),
    /// The process ended. `stderr` holds its last lines.
    Exited {
        code: Option<i32>,
        stderr: String,
    },
}

/// A model the agent offers, as `claude` lists it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModelChoice {
    /// What to pass to `--model` / `set_model`: an alias (`opus`) or a full id.
    pub value: String,
    pub name: String,
    pub description: String,
    /// The model id the value runs (`claude-opus-5-5`), when `claude` says.
    #[serde(default)]
    pub resolved: Option<String>,
    /// Effort levels the model accepts (`low` ... `max`); empty when it has none.
    pub efforts: Vec<String>,
}

impl ModelChoice {
    /// Used until `claude` has listed the real ones (and if it can't).
    pub fn fallback() -> Vec<Self> {
        let effort = ["low", "medium", "high", "xhigh", "max"].map(String::from).to_vec();
        [
            ("default", "Default", "Claude Code's default model"),
            ("opus", "Opus", ""),
            ("sonnet", "Sonnet", ""),
            ("haiku", "Haiku", ""),
        ]
        .into_iter()
        .map(|(value, name, description)| Self {
            value: value.into(),
            name: name.into(),
            description: description.into(),
            resolved: None,
            efforts: effort.clone(),
        })
        .collect()
    }
}

/// An image sent with a prompt.
#[derive(Clone, Debug, PartialEq)]
pub struct PromptImage {
    /// `image/png`, `image/jpeg`, `image/gif` or `image/webp`.
    pub media_type: String,
    pub data: std::sync::Arc<Vec<u8>>,
}

/// Commands from the UI to a running session.
pub trait AgentSession {
    /// The next user message: text, and images to go with it.
    fn send_message(&self, text: &str, images: &[PromptImage]) -> anyhow::Result<()>;
    /// Text only (the live tests use it).
    #[cfg(test)]
    fn send_prompt(&self, text: &str) -> anyhow::Result<()> {
        self.send_message(text, &[])
    }
    fn respond(&self, request: &PermissionRequest, decision: Decision) -> anyhow::Result<()>;
    fn interrupt(&self) -> anyhow::Result<()>;
    fn set_permission_mode(&self, mode: PermissionMode) -> anyhow::Result<()>;
    /// Switch model for the rest of the session; `None` goes back to the default.
    fn set_model(&self, model: Option<&str>) -> anyhow::Result<()>;
    /// Change effort for the rest of the session; `None` goes back to the model's default.
    fn set_effort(&self, effort: Option<&str>) -> anyhow::Result<()>;
}

/// An agent that plays back a fixed list of events and records what the UI sends it.
/// Used by `--demo` and by UI tests; it never runs a model.
pub struct ScriptedAgent {
    pub log: Arc<Mutex<Vec<ScriptedCall>>>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ScriptedCall {
    Prompt(String),
    /// The media types of images sent with the next prompt.
    Images(Vec<String>),
    Respond {
        request_id: String,
        decision: Decision,
    },
    Interrupt,
    SetPermissionMode(PermissionMode),
    SetModel(Option<String>),
    SetEffort(Option<String>),
}

impl ScriptedAgent {
    /// Start playing `events`, each after its delay, on a background thread.
    pub fn start(events: Vec<(Duration, AgentEvent)>) -> (Self, smol::channel::Receiver<AgentEvent>) {
        let (tx, rx) = smol::channel::unbounded();
        std::thread::spawn(move || {
            for (delay, event) in events {
                if !delay.is_zero() {
                    std::thread::sleep(delay);
                }
                if tx.send_blocking(event).is_err() {
                    break;
                }
            }
        });
        (Self { log: Arc::default() }, rx)
    }

    fn record(&self, call: ScriptedCall) -> anyhow::Result<()> {
        self.log.lock().unwrap().push(call);
        Ok(())
    }
}

impl AgentSession for ScriptedAgent {
    fn send_message(&self, text: &str, images: &[PromptImage]) -> anyhow::Result<()> {
        if !images.is_empty() {
            self.record(ScriptedCall::Images(images.iter().map(|i| i.media_type.clone()).collect()))?;
        }
        self.record(ScriptedCall::Prompt(text.to_string()))
    }
    fn respond(&self, request: &PermissionRequest, decision: Decision) -> anyhow::Result<()> {
        self.record(ScriptedCall::Respond { request_id: request.id.clone(), decision })
    }
    fn interrupt(&self) -> anyhow::Result<()> {
        self.record(ScriptedCall::Interrupt)
    }
    fn set_permission_mode(&self, mode: PermissionMode) -> anyhow::Result<()> {
        self.record(ScriptedCall::SetPermissionMode(mode))
    }
    fn set_model(&self, model: Option<&str>) -> anyhow::Result<()> {
        self.record(ScriptedCall::SetModel(model.map(str::to_owned)))
    }
    fn set_effort(&self, effort: Option<&str>) -> anyhow::Result<()> {
        self.record(ScriptedCall::SetEffort(effort.map(str::to_owned)))
    }
}
