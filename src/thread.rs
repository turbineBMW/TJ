//! A conversation with an agent: the transcript, approval cards, and the prompt box.

use std::{
    collections::HashSet,
    path::PathBuf,
    sync::{Arc, Mutex},
};

use gpui_kit::base::{h_flex, v_flex};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Sizable as _,
    button::{Button, ButtonVariants as _},
    input::{Input, InputEvent, InputState, Textarea, TextareaState},
    message_scroller::{MessageScroller, MessageScrollerState},
    text::{TextView, TextViewState},
};
use gpui_kit::{prelude::FluentBuilder as _, *};
use serde_json::Value;

use crate::agent::{
    AgentEvent, AgentSession, Decision, PermissionMode, PermissionRequest, ScriptedAgent, ScriptedCall, UsageWindow,
    claude::{self, ClaudeSession, SessionOptions},
};
use crate::overview::{self, Overview};
use crate::store::{Record, TranscriptWriter};

/// How often a long turn's overview is refreshed while it runs.
const OVERVIEW_EVERY: std::time::Duration = std::time::Duration::from_secs(60);

/// The most transcript text handed to a new session when a thread switches accounts.
const HANDOFF_MAX_CHARS: usize = 30_000;

/// Tells the workspace something it saves or shows has changed (title, session, status, ...).
pub struct ThreadChanged;

impl EventEmitter<ThreadChanged> for ThreadView {}

/// Who the thread is and where it keeps its transcript.
#[derive(Clone, Default)]
pub struct ThreadSetup {
    pub title: Option<String>,
    pub session_id: Option<String>,
    pub account_id: String,
    pub account_name: String,
    /// Where to append the transcript; nothing is saved when `None` (demos, tests).
    pub transcript: Option<PathBuf>,
}

/// How the thread starts its agent when the first prompt is sent.
pub enum Backend {
    Claude(SessionOptions),
    /// Plays back canned events (demos and tests); the events start on the first prompt.
    Scripted(Vec<(std::time::Duration, AgentEvent)>),
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Status {
    Idle,
    Running,
    AwaitingApproval,
}

#[derive(Clone, Copy, PartialEq)]
enum NoticeKind {
    Info,
    Warning,
    Error,
}

#[derive(Clone, Copy, PartialEq)]
enum ToolStatus {
    Running,
    Done,
    Failed,
}

struct ToolRow {
    id: String,
    name: String,
    summary: String,
    status: ToolStatus,
    output: Option<String>,
    expanded: bool,
}

struct ApprovalRow {
    request: PermissionRequest,
    /// `None` while waiting; otherwise what happened, shown in place of the buttons.
    outcome: Option<&'static str>,
    /// Created on first render, since an input needs a window.
    feedback: Option<Entity<InputState>>,
}

enum Row {
    User(Entity<TextViewState>),
    Assistant(Entity<TextViewState>),
    Tool(ToolRow),
    Approval(ApprovalRow),
    Notice(NoticeKind, SharedString),
    Footer(SharedString),
}

pub struct ThreadView {
    pub title: Option<String>,
    pub account_id: String,
    pub account_name: String,
    rows: Vec<Row>,
    /// The conversation as plain text ("User: ..." / "Claude: ..."), for account handoffs.
    plain: Vec<String>,
    plain_current: Option<usize>,
    log: Option<TranscriptWriter>,
    /// Streamed text not yet written to the transcript; saved as one delta per block.
    pending_text: String,
    replaying: bool,
    /// Context to prepend to the next prompt after an account switch.
    handoff: Option<String>,
    /// Files the agent wrote or edited in this thread (paths as the tools gave them).
    pub touched: HashSet<String>,
    /// Goal, Now, Next and tasks, kept by a cheap model.
    pub overview: Overview,
    /// How much of `plain` the overview has read.
    overview_mark: usize,
    pub overview_busy: bool,
    overview_again: bool,
    _overview_tick: Option<Task<()>>,
    /// Tokens in the latest request, and the model's context window.
    pub context_tokens: Option<u64>,
    pub context_window: Option<u64>,
    scroller: Entity<MessageScrollerState>,
    prompt: Entity<TextareaState>,
    backend: Option<Backend>,
    agent: Option<Box<dyn AgentSession>>,
    _pump: Option<Task<()>>,
    /// The assistant text row that deltas go to.
    current_text: Option<usize>,
    pub status: Status,
    pub mode: PermissionMode,
    pub session_id: Option<String>,
    pub model: Option<String>,
    pub cost_usd: f64,
    pub usage: Vec<UsageWindow>,
    /// What a scripted agent was asked to do; tests read it.
    pub scripted_log: Option<Arc<Mutex<Vec<ScriptedCall>>>>,
}

impl ThreadView {
    #[cfg(test)]
    pub fn new(backend: Backend, window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self::open(ThreadSetup::default(), backend, window, cx)
    }

    /// A thread with an identity, replaying its saved transcript if there is one.
    pub fn open(setup: ThreadSetup, backend: Backend, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mode = match &backend {
            Backend::Claude(options) => options.permission_mode,
            Backend::Scripted(_) => PermissionMode::default(),
        };
        let prompt = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(1, 8)
                .submit_on_enter(true)
                .placeholder("Ask Claude…  (Enter sends, Shift+Enter adds a line)")
        });
        cx.subscribe_in(&prompt, window, |this, _, event: &InputEvent, window, cx| {
            // Shift+Enter has already inserted a newline; only a plain or Ctrl+Enter sends.
            if let InputEvent::PressEnter { shift: false, .. } = event {
                this.submit(window, cx);
            }
        })
        .detach();

        let records = setup.transcript.as_deref().map(crate::store::read_transcript).unwrap_or_default();
        let log = setup.transcript.as_deref().and_then(|path| TranscriptWriter::open(path).ok());
        let mut this = Self {
            title: setup.title,
            account_id: setup.account_id,
            account_name: setup.account_name,
            rows: Vec::new(),
            plain: Vec::new(),
            plain_current: None,
            log: None,
            pending_text: String::new(),
            replaying: false,
            handoff: None,
            touched: HashSet::new(),
            overview: Overview::default(),
            overview_mark: 0,
            overview_busy: false,
            overview_again: false,
            _overview_tick: None,
            context_tokens: None,
            context_window: None,
            scroller: cx.new(|cx| MessageScrollerState::new(0, cx)),
            prompt,
            backend: Some(backend),
            agent: None,
            _pump: None,
            current_text: None,
            status: Status::Idle,
            mode,
            session_id: None,
            model: None,
            cost_usd: 0.0,
            usage: Vec::new(),
            scripted_log: None,
        };
        this.replay(records, cx);
        // The saved session id wins over one seen in the transcript (an account switch clears it).
        this.session_id = setup.session_id;
        this.log = log;
        this
    }

    /// Rebuild the transcript from saved records, without writing them again.
    fn replay(&mut self, records: Vec<Record>, cx: &mut Context<Self>) {
        self.replaying = true;
        for record in records {
            match record {
                Record::Prompt { text } => self.push_prompt_row(&text, cx),
                Record::Event { event } => self.handle(event, cx),
                Record::Decision { request_id, outcome } => {
                    if let Some(Row::Approval(row)) =
                        self.rows.iter_mut().find(|r| matches!(r, Row::Approval(a) if a.request.id == request_id))
                    {
                        row.outcome = Some(outcome_label(&outcome));
                    }
                }
                Record::AccountSwitch { from, to } => {
                    self.notice(NoticeKind::Info, format!("Switched from {from} to {to}."), cx)
                }
                Record::Overview { overview, mark } => {
                    self.overview = overview;
                    self.overview_mark = mark.min(self.plain.len());
                }
            }
        }
        // Whatever was waiting belonged to a process that is gone.
        for row in &mut self.rows {
            if let Row::Approval(row) = row
                && row.outcome.is_none()
            {
                row.outcome = Some("No longer needed");
            }
        }
        self.current_text = None;
        self.plain_current = None;
        self.status = Status::Idle;
        self.replaying = false;
    }

    fn write(&mut self, record: Record) {
        if self.replaying {
            return;
        }
        if let Some(log) = &mut self.log {
            if !self.pending_text.is_empty() {
                let text = std::mem::take(&mut self.pending_text);
                let _ = log.append(&Record::Event { event: AgentEvent::TextDelta(text) });
            }
            let _ = log.append(&record);
        }
    }

    /// Move the thread to another account. The next message starts a new Claude session there,
    /// given this conversation's transcript as context, since a session can't change accounts.
    pub fn set_account(&mut self, id: &str, name: &str, config_dir: Option<PathBuf>, cx: &mut Context<Self>) {
        if self.account_id == id {
            return;
        }
        let from = std::mem::replace(&mut self.account_name, name.to_string());
        self.account_id = id.to_string();
        if let Some(Backend::Claude(options)) = &mut self.backend {
            options.config_dir = config_dir;
        }
        if !self.plain.is_empty() {
            self.handoff = Some(self.handoff_text(&from, name));
            self.agent = None;
            self._pump = None;
            self.session_id = None;
            self.status = Status::Idle;
            self.write(Record::AccountSwitch { from: from.clone(), to: name.to_string() });
            self.notice(
                NoticeKind::Info,
                format!(
                    "Switched from {from} to {name}. Your next message starts a new Claude session on {name}, with this conversation so far as context."
                ),
                cx,
            );
        }
        cx.emit(ThreadChanged);
        cx.notify();
    }

    fn handoff_text(&self, from: &str, to: &str) -> String {
        let mut transcript = self.plain.join("\n\n");
        if transcript.len() > HANDOFF_MAX_CHARS {
            let mut start = transcript.len() - HANDOFF_MAX_CHARS;
            while !transcript.is_char_boundary(start) {
                start += 1;
            }
            transcript = format!("[earlier messages omitted]\n\n{}", &transcript[start..]);
        }
        format!(
            "[Context from TJ: this conversation began in another Claude session (account: {from}) and continues here (account: {to}). Here is the conversation so far.]\n\n{transcript}\n\n[End of the earlier conversation. My new message follows.]"
        )
    }

    /// The user's last `n` messages in this thread, oldest first (context for change summaries).
    pub fn recent_requests(&self, n: usize) -> Vec<String> {
        let mut requests: Vec<String> =
            self.plain.iter().rev().filter_map(|line| line.strip_prefix("User: ")).take(n).map(str::to_owned).collect();
        requests.reverse();
        requests
    }

    #[cfg(test)]
    pub fn has_pending_handoff(&self) -> bool {
        self.handoff.is_some()
    }

    pub fn prompt_focus(&self, cx: &App) -> FocusHandle {
        self.prompt.focus_handle(cx)
    }

    // ---------- commands ----------

    /// Send what's in the prompt box.
    pub fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.prompt.read(cx).value().trim().to_string();
        if text.is_empty() {
            return;
        }
        self.prompt.update(cx, |p, cx| p.set_value("", window, cx));
        self.send(&text, cx);
    }

    /// Send `text` as the next user message.
    pub fn send(&mut self, text: &str, cx: &mut Context<Self>) {
        self.push_prompt_row(text, cx);
        self.write(Record::Prompt { text: text.to_string() });
        if self.title.is_none() {
            self.title = Some(title_from(text));
        }
        let message = match &self.handoff {
            Some(context) => format!("{context}\n\n{text}"),
            None => text.to_string(),
        };
        if let Err(err) = self.ensure_agent(cx).and_then(|agent| agent.send_prompt(&message)) {
            self.notice(NoticeKind::Error, format!("Could not send: {err:#}"), cx);
            cx.emit(ThreadChanged);
            return;
        }
        self.handoff = None;
        self.status = Status::Running;
        self.start_overview_ticker(cx);
        cx.emit(ThreadChanged);
        cx.notify();
    }

    /// While a turn runs, refresh the overview every [`OVERVIEW_EVERY`].
    fn start_overview_ticker(&mut self, cx: &mut Context<Self>) {
        if self._overview_tick.is_some() || !self.writes_overview() {
            return;
        }
        self._overview_tick = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(OVERVIEW_EVERY).await;
                let alive = this.update(cx, |t: &mut ThreadView, cx| {
                    if t.status != Status::Idle {
                        t.update_overview(cx);
                    }
                });
                if alive.is_err() {
                    break;
                }
            }
        }));
    }

    /// Overviews come from Claude, so only Claude-backed threads write them.
    fn writes_overview(&self) -> bool {
        matches!(self.backend, Some(Backend::Claude(_)))
    }

    /// Ask the overview writer to read what's new. At most one update runs at a time.
    pub fn update_overview(&mut self, cx: &mut Context<Self>) {
        if self.replaying || !self.writes_overview() || self.plain.len() <= self.overview_mark {
            return;
        }
        if self.overview_busy {
            self.overview_again = true;
            return;
        }
        let Some(Backend::Claude(options)) = &self.backend else { return };
        let (cwd, config_dir) = (options.cwd.clone(), options.config_dir.clone());
        let mark = self.plain.len();
        let delta = self.plain[self.overview_mark..].join("\n\n");
        let current = self.overview.clone();
        self.overview_busy = true;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { overview::write(&cwd, config_dir.as_deref(), &current, &delta) })
                .await;
            let _ = this.update(cx, |t: &mut ThreadView, cx| {
                t.overview_busy = false;
                match result {
                    Ok(overview) => t.apply_overview(overview, mark, cx),
                    // Not worth a notice: the next turn tries again with a larger delta.
                    Err(err) => eprintln!("tj: overview update failed: {err:#}"),
                }
                if std::mem::take(&mut t.overview_again) {
                    t.update_overview(cx);
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Take a new overview covering `plain[..mark]`, and save it.
    pub fn apply_overview(&mut self, overview: Overview, mark: usize, cx: &mut Context<Self>) {
        if let Some(title) = &overview.title {
            self.title = Some(title.clone());
        }
        self.overview = overview.clone();
        self.overview_mark = mark;
        self.write(Record::Overview { overview, mark });
        cx.emit(ThreadChanged);
        cx.notify();
    }

    fn push_prompt_row(&mut self, text: &str, cx: &mut Context<Self>) {
        self.push_text_row(true, text, cx);
        self.plain.push(format!("User: {text}"));
        self.plain_current = None;
        self.current_text = None;
    }

    pub fn interrupt(&mut self, cx: &mut Context<Self>) {
        if let Some(agent) = &self.agent
            && let Err(err) = agent.interrupt()
        {
            self.notice(NoticeKind::Error, format!("Could not stop: {err:#}"), cx);
        }
    }

    pub fn set_mode(&mut self, mode: PermissionMode, cx: &mut Context<Self>) {
        self.mode = mode;
        if let Some(Backend::Claude(options)) = &mut self.backend {
            options.permission_mode = mode;
        }
        if let Some(agent) = &self.agent
            && let Err(err) = agent.set_permission_mode(mode)
        {
            self.notice(NoticeKind::Error, format!("Could not change mode: {err:#}"), cx);
        }
        cx.notify();
    }

    /// Answer the approval card in row `ix`.
    pub fn decide(&mut self, ix: usize, decision: Decision, cx: &mut Context<Self>) {
        let Some(Row::Approval(row)) = self.rows.get_mut(ix) else { return };
        if row.outcome.is_some() {
            return;
        }
        let decision = match decision {
            Decision::Decline { .. } => {
                let feedback = row.feedback.as_ref().map(|f| f.read(cx).value().trim().to_string()).unwrap_or_default();
                Decision::Decline { feedback: (!feedback.is_empty()).then_some(feedback) }
            }
            other => other,
        };
        row.outcome = Some(match decision {
            Decision::Approve => "Approved",
            Decision::ApproveForSession => "Approved for this session",
            Decision::Decline { .. } => "Declined",
            Decision::Cancel => "Cancelled the turn",
        });
        let request = row.request.clone();
        let outcome = row.outcome.unwrap_or_default().to_string();
        self.write(Record::Decision { request_id: request.id.clone(), outcome });
        if let Some(agent) = &self.agent
            && let Err(err) = agent.respond(&request, decision)
        {
            self.notice(NoticeKind::Error, format!("Could not answer: {err:#}"), cx);
        }
        if !self.has_pending_approval() && self.status == Status::AwaitingApproval {
            self.status = Status::Running;
        }
        cx.emit(ThreadChanged);
        self.remeasure(ix, cx);
    }

    fn ensure_agent(&mut self, cx: &mut Context<Self>) -> anyhow::Result<&dyn AgentSession> {
        if self.agent.is_none() {
            let (agent, events): (Box<dyn AgentSession>, _) = match self.backend.as_mut() {
                Some(Backend::Claude(options)) => {
                    options.resume = self.session_id.clone();
                    let (session, events) = ClaudeSession::spawn(options)?;
                    let _ = session.request_usage();
                    (Box::new(session), events)
                }
                Some(Backend::Scripted(events)) => {
                    let (agent, rx) = ScriptedAgent::start(std::mem::take(events));
                    self.scripted_log = Some(agent.log.clone());
                    (Box::new(agent), rx)
                }
                None => anyhow::bail!("no agent backend"),
            };
            self._pump = Some(cx.spawn(async move |this, cx| {
                while let Ok(event) = events.recv().await {
                    if this.update(cx, |this: &mut ThreadView, cx| this.handle(event, cx)).is_err() {
                        break;
                    }
                }
            }));
            self.agent = Some(agent);
        }
        Ok(self.agent.as_deref().expect("agent was just set"))
    }

    #[cfg(test)]
    /// Use a scripted agent that is already running (tests).
    pub fn attach_scripted(&mut self, agent: ScriptedAgent) {
        self.scripted_log = Some(agent.log.clone());
        self.agent = Some(Box::new(agent));
    }

    // ---------- events ----------

    pub fn handle(&mut self, event: AgentEvent, cx: &mut Context<Self>) {
        match &event {
            // Process lifecycle and live usage aren't part of the conversation.
            AgentEvent::Started { .. } | AgentEvent::Exited { .. } | AgentEvent::Usage(_) => {}
            AgentEvent::TextDelta(text) if !self.replaying => self.pending_text.push_str(text),
            _ => self.write(Record::Event { event: event.clone() }),
        }
        let status_before = self.status;
        let session_before = self.session_id.clone();
        // A finished tool may have changed files; the workspace refreshes the project's changes.
        let tool_finished = matches!(event, AgentEvent::ToolFinished { .. });
        self.handle_event(event, cx);
        if !self.replaying && (tool_finished || self.status != status_before || self.session_id != session_before) {
            cx.emit(ThreadChanged);
        }
    }

    fn handle_event(&mut self, event: AgentEvent, cx: &mut Context<Self>) {
        match event {
            AgentEvent::Started { version, permission_mode } => {
                if let Some(version) = version.filter(|v| claude::is_newer_than_tested(v)) {
                    self.notice(
                        NoticeKind::Warning,
                        format!(
                            "claude {version} is newer than the version TJ was tested with ({}). If something looks wrong, that's the likely cause.",
                            claude::TESTED_VERSION
                        ),
                        cx,
                    );
                }
                if let Some(mode) = permission_mode {
                    self.mode = mode;
                }
            }
            AgentEvent::SessionInfo { session_id, model } => {
                self.session_id = Some(session_id);
                self.model = model.or(self.model.take());
            }
            AgentEvent::TextStarted => {
                self.current_text = Some(self.push_text_row(false, "", cx));
                self.plain.push("Claude: ".into());
                self.plain_current = Some(self.plain.len() - 1);
            }
            AgentEvent::TextDelta(text) => {
                let ix = match self.current_text {
                    Some(ix) => ix,
                    None => {
                        let ix = self.push_text_row(false, "", cx);
                        self.current_text = Some(ix);
                        ix
                    }
                };
                if let Some(Row::Assistant(body)) = self.rows.get(ix) {
                    body.update(cx, |state, cx| state.push_str(&text, cx));
                }
                let plain = match self.plain_current {
                    Some(plain) => plain,
                    None => {
                        self.plain.push("Claude: ".into());
                        self.plain.len() - 1
                    }
                };
                self.plain[plain].push_str(&text);
                self.plain_current = Some(plain);
                self.remeasure(ix, cx);
            }
            AgentEvent::ToolStarted { id, name, input } => {
                self.current_text = None;
                if matches!(name.as_str(), "Write" | "Edit" | "MultiEdit" | "NotebookEdit")
                    && let Some(path) = input.get("file_path").or(input.get("notebook_path")).and_then(Value::as_str)
                {
                    self.touched.insert(path.to_string());
                }
                let summary = tool_summary(&name, &input);
                let line = match (name.as_str(), input.get("todos").and_then(Value::as_array)) {
                    // Claude's own task list is the best evidence for the overview's tasks.
                    ("TodoWrite", Some(todos)) => {
                        let items: Vec<String> = todos
                            .iter()
                            .filter_map(|t| {
                                let done = t.get("status").and_then(Value::as_str) == Some("completed");
                                Some(format!("[{}] {}", if done { "x" } else { " " }, t.get("content")?.as_str()?))
                            })
                            .collect();
                        format!("(Claude updated its task list: {})", items.join("; "))
                    }
                    _ => format!("(Claude used {name}: {})", truncate(&summary, 300)),
                };
                self.plain.push(line);
                self.plain_current = None;
                self.push_row(
                    Row::Tool(ToolRow {
                        id,
                        name,
                        summary,
                        status: ToolStatus::Running,
                        output: None,
                        expanded: false,
                    }),
                    cx,
                );
            }
            AgentEvent::ToolFinished { id, output, is_error } => {
                if let Some(ix) = self.rows.iter().rposition(|r| matches!(r, Row::Tool(t) if t.id == id)) {
                    if let Row::Tool(tool) = &mut self.rows[ix] {
                        tool.status = if is_error { ToolStatus::Failed } else { ToolStatus::Done };
                        if is_error {
                            self.plain.push(format!("({} failed: {})", tool.name, truncate(&output, 240)));
                            self.plain_current = None;
                        }
                        tool.output = Some(output);
                    }
                    self.remeasure(ix, cx);
                }
            }
            AgentEvent::PermissionRequested(request) => {
                self.current_text = None;
                self.push_row(Row::Approval(ApprovalRow { request, outcome: None, feedback: None }), cx);
                self.status = Status::AwaitingApproval;
            }
            AgentEvent::PermissionWithdrawn { id } => {
                if let Some(ix) = self.rows.iter().position(|r| matches!(r, Row::Approval(a) if a.request.id == id)) {
                    if let Row::Approval(row) = &mut self.rows[ix]
                        && row.outcome.is_none()
                    {
                        row.outcome = Some("No longer needed");
                    }
                    self.remeasure(ix, cx);
                }
            }
            AgentEvent::Context { tokens, window } => {
                self.context_tokens = tokens.or(self.context_tokens);
                self.context_window = window.or(self.context_window);
            }
            AgentEvent::Usage(windows) => {
                for window in windows {
                    match self.usage.iter_mut().find(|w| w.id == window.id) {
                        Some(existing) => *existing = window,
                        None => self.usage.push(window),
                    }
                }
            }
            AgentEvent::TurnFinished { cost_usd, duration_ms, is_error, error } => {
                self.current_text = None;
                self.status = Status::Idle;
                self.update_overview(cx);
                if let Some(cost) = cost_usd {
                    self.cost_usd += cost;
                }
                if is_error {
                    self.notice(NoticeKind::Error, error.unwrap_or_else(|| "The turn failed.".into()), cx);
                }
                let mut parts = Vec::new();
                if let Some(model) = &self.model {
                    parts.push(short_model(model));
                }
                if let Some(cost) = cost_usd {
                    parts.push(format!("${cost:.4}"));
                }
                if let Some(ms) = duration_ms {
                    parts.push(format!("{:.1}s", ms as f64 / 1000.0));
                }
                if !parts.is_empty() {
                    self.push_row(Row::Footer(parts.join(" · ").into()), cx);
                }
            }
            AgentEvent::Error(message) => self.notice(NoticeKind::Error, message, cx),
            AgentEvent::Exited { code, stderr } => {
                self.agent = None;
                self._pump = None;
                self.current_text = None;
                if self.status != Status::Idle || code.is_some_and(|c| c != 0) {
                    let detail = stderr.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("").to_string();
                    let code = code.map_or("a signal".to_string(), |c| format!("code {c}"));
                    self.notice(
                        NoticeKind::Error,
                        format!("Claude stopped ({code}). {detail} Sending another message resumes the conversation."),
                        cx,
                    );
                }
                self.status = Status::Idle;
            }
        }
        cx.notify();
    }

    fn has_pending_approval(&self) -> bool {
        self.rows.iter().any(|r| matches!(r, Row::Approval(a) if a.outcome.is_none()))
    }

    // ---------- rows ----------

    fn push_row(&mut self, row: Row, cx: &mut Context<Self>) -> usize {
        self.rows.push(row);
        self.scroller.update(cx, |s, cx| {
            s.append(1, cx);
        });
        self.rows.len() - 1
    }

    fn push_text_row(&mut self, user: bool, text: &str, cx: &mut Context<Self>) -> usize {
        let body = cx.new(|cx| TextViewState::markdown(text, cx).selectable(true));
        self.push_row(if user { Row::User(body) } else { Row::Assistant(body) }, cx)
    }

    fn notice(&mut self, kind: NoticeKind, text: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.push_row(Row::Notice(kind, text.into()), cx);
        cx.notify();
    }

    fn remeasure(&mut self, ix: usize, cx: &mut Context<Self>) {
        self.scroller.update(cx, |s, cx| {
            s.remeasure_items(ix..ix + 1, cx);
        });
        cx.notify();
    }

    #[cfg(test)]
    pub fn approval_rows(&self) -> Vec<usize> {
        (0..self.rows.len()).filter(|ix| matches!(self.rows[*ix], Row::Approval(_))).collect()
    }

    #[cfg(test)]
    pub fn approval_outcome(&self, ix: usize) -> Option<&'static str> {
        match self.rows.get(ix) {
            Some(Row::Approval(row)) => row.outcome,
            _ => None,
        }
    }

    #[cfg(test)]
    pub fn feedback_input(&self, ix: usize) -> Option<Entity<InputState>> {
        match self.rows.get(ix) {
            Some(Row::Approval(row)) => row.feedback.clone(),
            _ => None,
        }
    }

    // ---------- rendering ----------

    fn render_row(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        if let Some(Row::Approval(row)) = self.rows.get_mut(ix)
            && row.outcome.is_none()
            && row.feedback.is_none()
        {
            row.feedback =
                Some(cx.new(|cx| {
                    InputState::new(window, cx).placeholder("Optional: tell Claude why, or what to do instead")
                }));
        }
        let theme = cx.theme();
        let (border, muted, secondary, accent) = (theme.border, theme.muted_foreground, theme.secondary, theme.primary);
        let mono = theme.mono_font_family.clone();
        let (danger, warning) = (theme.danger, theme.warning);
        let pad = |el: Div| el.w_full().px_4().py_1p5();

        match &self.rows[ix] {
            Row::User(body) => h_flex()
                .w_full()
                .px_4()
                .py_2()
                .justify_end()
                .child(
                    div()
                        .max_w(relative(0.8))
                        .px_3()
                        .py_2()
                        .rounded_lg()
                        .bg(secondary)
                        .child(TextView::new(body).selectable(true)),
                )
                .into_any_element(),
            Row::Assistant(body) => {
                pad(div()).py_2().child(TextView::new(body).selectable(true).stream_fade(true)).into_any_element()
            }
            Row::Tool(tool) => {
                let (dot, label) = match tool.status {
                    ToolStatus::Running => (muted, "running"),
                    ToolStatus::Done => (theme.success, "done"),
                    ToolStatus::Failed => (danger, "failed"),
                };
                let output = tool.output.clone().filter(|_| tool.expanded);
                pad(div())
                    .child(
                        v_flex()
                            .rounded_md()
                            .border_1()
                            .border_color(border)
                            .child(
                                h_flex()
                                    .id(SharedString::from(format!("tool-{ix}")))
                                    .gap_2()
                                    .px_3()
                                    .py_1p5()
                                    .cursor_pointer()
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        if let Some(Row::Tool(tool)) = this.rows.get_mut(ix) {
                                            tool.expanded = !tool.expanded;
                                        }
                                        this.remeasure(ix, cx);
                                    }))
                                    .child(div().size_2().rounded_full().bg(dot))
                                    .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).child(tool.name.clone()))
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .overflow_hidden()
                                            .whitespace_nowrap()
                                            .text_ellipsis()
                                            .text_sm()
                                            .font_family(mono.clone())
                                            .text_color(muted)
                                            .child(tool.summary.clone()),
                                    )
                                    .child(div().text_xs().text_color(muted).child(label)),
                            )
                            .when_some(output, |el, output| {
                                el.child(
                                    div()
                                        .border_t_1()
                                        .border_color(border)
                                        .px_3()
                                        .py_2()
                                        .text_xs()
                                        .font_family(mono)
                                        .text_color(muted)
                                        .child(truncate(&output, 4000)),
                                )
                            }),
                    )
                    .into_any_element()
            }
            Row::Approval(row) => {
                let request = &row.request;
                let summary = tool_summary(&request.tool_name, &request.input);
                let pending = row.outcome.is_none();
                pad(div())
                    .child(
                        v_flex()
                            .gap_2()
                            .p_3()
                            .rounded_md()
                            .border_1()
                            .border_color(if pending { accent } else { border })
                            .child(
                                div()
                                    .text_sm()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(format!("Claude wants to use {}", request.tool_name)),
                            )
                            .child(
                                div()
                                    .px_2()
                                    .py_1()
                                    .rounded_sm()
                                    .bg(secondary)
                                    .text_sm()
                                    .font_family(mono)
                                    .child(truncate(&summary, 2000)),
                            )
                            .when_some(request.description.clone().filter(|d| *d != summary), |el, d| {
                                el.child(div().text_sm().text_color(muted).child(d))
                            })
                            .when_some(request.reason.clone(), |el, reason| {
                                el.child(div().text_sm().text_color(warning).child(reason))
                            })
                            .map(|el| match row.outcome {
                                Some(outcome) => el.child(div().text_sm().text_color(muted).child(outcome)),
                                None => {
                                    el.when_some(row.feedback.as_ref(), |el, f| el.child(Input::new(f).small())).child(
                                        h_flex()
                                            .gap_2()
                                            .child(
                                                Button::new(SharedString::from(format!("approve-{ix}")))
                                                    .primary()
                                                    .small()
                                                    .label("Approve")
                                                    .on_click(cx.listener(move |this, _, _, cx| {
                                                        this.decide(ix, Decision::Approve, cx)
                                                    })),
                                            )
                                            .when(!request.no_standing_grant, |el| {
                                                el.child(
                                                    Button::new(SharedString::from(format!("approve-session-{ix}")))
                                                        .small()
                                                        .label("Approve for session")
                                                        .on_click(cx.listener(move |this, _, _, cx| {
                                                            this.decide(ix, Decision::ApproveForSession, cx)
                                                        })),
                                                )
                                            })
                                            .child(
                                                Button::new(SharedString::from(format!("decline-{ix}")))
                                                    .small()
                                                    .label("Decline")
                                                    .on_click(cx.listener(move |this, _, _, cx| {
                                                        this.decide(ix, Decision::Decline { feedback: None }, cx)
                                                    })),
                                            )
                                            .child(
                                                Button::new(SharedString::from(format!("cancel-{ix}")))
                                                    .ghost()
                                                    .small()
                                                    .label("Cancel turn")
                                                    .on_click(cx.listener(move |this, _, _, cx| {
                                                        this.decide(ix, Decision::Cancel, cx)
                                                    })),
                                            ),
                                    )
                                }
                            }),
                    )
                    .into_any_element()
            }
            Row::Notice(kind, text) => {
                let color = match kind {
                    NoticeKind::Info => muted,
                    NoticeKind::Warning => warning,
                    NoticeKind::Error => danger,
                };
                pad(div()).text_sm().text_color(color).child(text.clone()).into_any_element()
            }
            Row::Footer(text) => pad(div()).pb_3().text_xs().text_color(muted).child(text.clone()).into_any_element(),
        }
    }
}

impl Render for ThreadView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (bg, border) = (theme.background, theme.border);
        let this = cx.entity();
        let running = self.status != Status::Idle;

        let transcript = MessageScroller::new("transcript", self.scroller.clone(), move |ix, window, cx| {
            this.update(cx, |thread, cx| {
                if ix < thread.rows.len() { thread.render_row(ix, window, cx) } else { div().into_any_element() }
            })
        })
        .scrollbar(true)
        .jump_button(true)
        .with_bottom_fade(bg);

        v_flex().size_full().bg(bg).child(div().flex_1().min_h_0().child(transcript)).child(
            h_flex()
                .items_end()
                .gap_2()
                .p_3()
                .border_t_1()
                .border_color(border)
                .child(div().flex_1().child(Textarea::new(&self.prompt)))
                .child(if running {
                    Button::new("stop").label("Stop").on_click(cx.listener(|this, _, _, cx| this.interrupt(cx)))
                } else {
                    Button::new("send")
                        .primary()
                        .label("Send")
                        .disabled(self.agent.is_none() && self.backend.is_none())
                        .on_click(cx.listener(|this, _, window, cx| this.submit(window, cx)))
                }),
        )
    }
}

/// A thread title from its first prompt: the first line, shortened.
pub fn title_from(prompt: &str) -> String {
    let line = prompt.lines().find(|l| !l.trim().is_empty()).unwrap_or("New thread").trim();
    truncate(line, 60)
}

/// Map a saved outcome back to the label shown on the card.
fn outcome_label(outcome: &str) -> &'static str {
    ["Approved", "Approved for this session", "Declined", "Cancelled the turn", "No longer needed"]
        .into_iter()
        .find(|label| *label == outcome)
        .unwrap_or("Answered")
}

/// One line describing a tool call, e.g. the command for Bash or the path for Edit.
pub fn tool_summary(name: &str, input: &Value) -> String {
    let field = |key: &str| input.get(key).and_then(Value::as_str).map(str::to_owned);
    let found = match name {
        "Bash" => field("command"),
        "Read" | "Write" | "Edit" | "MultiEdit" | "NotebookEdit" => field("file_path").or(field("notebook_path")),
        "Glob" | "Grep" => field("pattern"),
        "WebFetch" => field("url"),
        "WebSearch" => field("query"),
        "Task" | "Agent" => field("description"),
        "TodoWrite" => input.get("todos").and_then(Value::as_array).map(|t| format!("{} tasks", t.len())),
        _ => None,
    };
    found.unwrap_or_else(|| truncate(&input.to_string(), 160))
}

fn truncate(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

/// "claude-opus-5-5" -> "Opus 5.5"
pub fn short_model(model: &str) -> String {
    let parts: Vec<&str> = model.trim_start_matches("claude-").split('-').collect();
    match parts.as_slice() {
        [family, major, minor, ..] if major.chars().all(|c| c.is_ascii_digit()) => {
            let mut name = family.to_string();
            if let Some(first) = name.get_mut(0..1) {
                first.make_ascii_uppercase();
            }
            format!("{name} {major}.{minor}")
        }
        _ => model.to_string(),
    }
}

/// Where a new thread runs.
pub fn default_cwd() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

#[cfg(test)]
mod tests {
    use super::{Backend, Status, ThreadView};
    use crate::agent::{AgentEvent, Decision, ScriptedAgent, ScriptedCall, claude};
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{
        AppContext as _, Entity, TestAppContext, VisualTestContext, WindowBounds, WindowOptions, bounds, point, px,
        size,
    };

    fn fixture(name: &str) -> Vec<AgentEvent> {
        let path = format!("{}/tests/fixtures/claude/{name}", env!("CARGO_MANIFEST_DIR"));
        let mut parser = claude::Parser;
        claude::read_fixture(std::fs::File::open(path).unwrap()).iter().flat_map(|m| parser.parse(m)).collect()
    }

    /// A thread in a real gpui-kit window, wired to a scripted agent whose calls are logged.
    fn open(cx: &mut TestAppContext) -> (gpui_kit::AnyWindowHandle, Entity<ThreadView>) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::keymap::init(cx);
        });
        cx.update(|cx| {
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds(point(px(0.), px(0.)), size(px(900.), px(1400.))))),
                ..Default::default()
            };
            gpui_kit::open_window(options, cx, |window, cx| {
                cx.new(|cx| {
                    let mut thread = ThreadView::new(Backend::Scripted(Vec::new()), window, cx);
                    thread.attach_scripted(ScriptedAgent::start(Vec::new()).0);
                    thread
                })
            })
            .expect("open test window")
        })
    }

    fn calls(thread: &Entity<ThreadView>, cx: &mut VisualTestContext) -> Vec<ScriptedCall> {
        thread.read_with(cx, |t, _| t.scripted_log.as_ref().unwrap().lock().unwrap().clone())
    }

    /// Feed the events up to and including the first permission request; return that row.
    fn until_approval(thread: &Entity<ThreadView>, events: Vec<AgentEvent>, cx: &mut VisualTestContext) -> usize {
        thread.update(cx, |t, cx| {
            for event in events {
                let stop = matches!(event, AgentEvent::PermissionRequested(_));
                t.handle(event, cx);
                if stop {
                    break;
                }
            }
        });
        let rows = thread.read_with(cx, |t, _| t.approval_rows());
        assert_eq!(rows.len(), 1);
        rows[0]
    }

    #[gpui_kit::test]
    fn approve_button_answers_the_request(cx: &mut TestAppContext) {
        let (window, thread) = open(cx);
        let cx = &mut VisualTestContext::from_window(window, cx);
        let ix = until_approval(&thread, fixture("2.1.331-approve.jsonl"), cx);
        assert_eq!(thread.read_with(cx, |t, _| t.status), Status::AwaitingApproval);

        cx.update(|window, cx| {
            window.render_frame(cx);
            window.click(gpui_kit::SharedString::from(format!("approve-{ix}")), cx);
        });

        let answered: Vec<_> = calls(&thread, cx)
            .into_iter()
            .filter_map(|c| match c {
                ScriptedCall::Respond { decision, .. } => Some(decision),
                _ => None,
            })
            .collect();
        assert_eq!(answered, vec![Decision::Approve]);
        assert_eq!(thread.read_with(cx, |t, _| t.approval_outcome(ix)), Some("Approved"));
        assert_eq!(thread.read_with(cx, |t, _| t.status), Status::Running);
    }

    #[gpui_kit::test]
    fn decline_sends_the_typed_feedback(cx: &mut TestAppContext) {
        let (window, thread) = open(cx);
        let cx = &mut VisualTestContext::from_window(window, cx);
        let ix = until_approval(&thread, fixture("2.1.331-decline.jsonl"), cx);

        cx.update(|window, cx| {
            window.render_frame(cx);
            let feedback = thread.read(cx).feedback_input(ix).expect("feedback field after render");
            feedback.update(cx, |f, cx| f.set_value("use mktemp instead", window, cx));
            window.click(gpui_kit::SharedString::from(format!("decline-{ix}")), cx);
        });

        let request_id = request_id(&thread, ix, cx);
        assert!(calls(&thread, cx).contains(&ScriptedCall::Respond {
            request_id,
            decision: Decision::Decline { feedback: Some("use mktemp instead".into()) },
        }));
        assert_eq!(thread.read_with(cx, |t, _| t.approval_outcome(ix)), Some("Declined"));
    }

    fn request_id(thread: &Entity<ThreadView>, ix: usize, cx: &mut VisualTestContext) -> String {
        thread.read_with(cx, |t, _| match &t.rows[ix] {
            super::Row::Approval(row) => row.request.id.clone(),
            _ => panic!("row {ix} is not an approval"),
        })
    }

    #[gpui_kit::test]
    fn enter_sends_the_prompt_and_shift_enter_does_not(cx: &mut TestAppContext) {
        let (window, thread) = open(cx);
        let cx = &mut VisualTestContext::from_window(window, cx);
        cx.update(|window, cx| {
            window.render_frame(cx);
            let focus = thread.read(cx).prompt_focus(cx);
            window.focus(&focus, cx);
            window.input("first line", cx);
            window.press("shift-enter", cx);
            window.input("second line", cx);
        });
        assert!(calls(&thread, cx).iter().all(|c| !matches!(c, ScriptedCall::Prompt(_))), "shift-enter sent");

        cx.update(|window, cx| window.press("enter", cx));
        assert_eq!(calls(&thread, cx), vec![ScriptedCall::Prompt("first line\nsecond line".into())]);
        assert_eq!(thread.read_with(cx, |t, cx| t.prompt.read(cx).value().to_string()), "");
    }

    #[gpui_kit::test]
    fn switching_accounts_hands_the_conversation_to_a_new_session(cx: &mut TestAppContext) {
        let (window, thread) = open(cx);
        let cx = &mut VisualTestContext::from_window(window, cx);
        thread.update(cx, |t, cx| {
            t.send("What does main.rs do?", cx);
            t.handle(AgentEvent::SessionInfo { session_id: "s1".into(), model: None }, cx);
            t.handle(AgentEvent::TextStarted, cx);
            t.handle(AgentEvent::TextDelta("It opens the window.".into()), cx);
            t.handle(AgentEvent::TurnFinished { cost_usd: None, duration_ms: None, is_error: false, error: None }, cx);
            t.set_account("work", "Work", None, cx);
        });
        assert_eq!(thread.read_with(cx, |t, _| t.session_id.clone()), None);
        assert!(thread.read_with(cx, |t, _| t.has_pending_handoff()));

        thread.update(cx, |t, cx| t.send("And store.rs?", cx));
        let prompt = calls(&thread, cx)
            .into_iter()
            .find_map(|c| match c {
                ScriptedCall::Prompt(p) => Some(p),
                _ => None,
            })
            .expect("the new session got a prompt");
        assert!(prompt.starts_with("[Context from TJ"), "{prompt}");
        assert!(prompt.contains("account: Work"));
        assert!(prompt.contains("User: What does main.rs do?"));
        assert!(prompt.contains("Claude: It opens the window."));
        assert!(prompt.ends_with("And store.rs?"));
        assert!(!thread.read_with(cx, |t, _| t.has_pending_handoff()), "the handoff is sent once");
    }

    #[gpui_kit::test]
    fn a_saved_transcript_replays_into_the_same_rows(cx: &mut TestAppContext) {
        let path = std::env::temp_dir().join(format!("tj-replay-{}.jsonl", crate::store::new_id()));
        let setup = super::ThreadSetup { transcript: Some(path.clone()), ..Default::default() };
        let (window, thread) = open(cx);
        let cx = &mut VisualTestContext::from_window(window, cx);
        let first = cx.update(|window, cx| {
            cx.new(|cx| ThreadView::open(setup.clone(), Backend::Scripted(Vec::new()), window, cx))
        });
        first.update(cx, |t, cx| {
            t.attach_scripted(ScriptedAgent::start(Vec::new()).0);
            t.send("Touch a file", cx);
            for event in fixture("2.1.331-approve.jsonl") {
                t.handle(event, cx);
            }
        });
        let ix = first.read_with(cx, |t, _| t.approval_rows()[0]);
        first.update(cx, |t, cx| t.decide(ix, Decision::Approve, cx));
        let _ = thread;

        let second =
            cx.update(|window, cx| cx.new(|cx| ThreadView::open(setup, Backend::Scripted(Vec::new()), window, cx)));
        let summary = |t: &ThreadView| (t.rows.len(), t.plain.clone(), t.approval_outcome(ix));
        let (a, b) = (first.read_with(cx, |t, _| summary(t)), second.read_with(cx, |t, _| summary(t)));
        assert_eq!(a, b);
        assert_eq!(b.2, Some("Approved"));
        assert_eq!(second.read_with(cx, |t, _| t.status), Status::Idle);
        let _ = std::fs::remove_file(path);
    }

    #[gpui_kit::test]
    fn edited_files_and_recent_requests_are_tracked(cx: &mut TestAppContext) {
        let (window, thread) = open(cx);
        let cx = &mut VisualTestContext::from_window(window, cx);
        thread.update(cx, |t, cx| {
            for prompt in ["one", "two", "three", "four"] {
                t.send(prompt, cx);
            }
            for (id, name, input) in [
                ("1", "Edit", serde_json::json!({ "file_path": "/p/src/a.rs" })),
                ("2", "Write", serde_json::json!({ "file_path": "/p/b.md" })),
                ("3", "Read", serde_json::json!({ "file_path": "/p/c.rs" })),
                ("4", "Bash", serde_json::json!({ "command": "touch d" })),
            ] {
                t.handle(AgentEvent::ToolStarted { id: id.into(), name: name.into(), input }, cx);
            }
        });
        let (touched, requests) = thread.read_with(cx, |t, _| (t.touched.clone(), t.recent_requests(3)));
        assert_eq!(touched, ["/p/src/a.rs", "/p/b.md"].map(String::from).into_iter().collect());
        assert_eq!(requests, vec!["two", "three", "four"]);
        let relative = crate::changes::touched_relative(std::path::Path::new("/p"), &touched);
        assert!(relative.contains("src/a.rs") && relative.contains("b.md"));
    }

    #[gpui_kit::test]
    fn an_applied_overview_sets_the_title_and_survives_a_reopen(cx: &mut TestAppContext) {
        let path = std::env::temp_dir().join(format!("tj-overview-{}.jsonl", crate::store::new_id()));
        let setup = super::ThreadSetup { transcript: Some(path.clone()), ..Default::default() };
        let (window, _thread) = open(cx);
        let cx = &mut VisualTestContext::from_window(window, cx);
        let first = cx.update(|window, cx| {
            cx.new(|cx| ThreadView::open(setup.clone(), Backend::Scripted(Vec::new()), window, cx))
        });
        let overview = crate::overview::Overview {
            title: Some("Dark mode toggle".into()),
            goal: Some("Ship a dark mode setting".into()),
            tasks: vec![crate::overview::Task { text: "Add toggle".into(), done: true }],
            ..Default::default()
        };
        first.update(cx, |t, cx| {
            t.attach_scripted(ScriptedAgent::start(Vec::new()).0);
            t.send("Add a dark mode toggle", cx);
            let mark = t.plain.len();
            t.apply_overview(overview.clone(), mark, cx);
        });
        assert_eq!(first.read_with(cx, |t, _| t.title.clone()), Some("Dark mode toggle".into()));

        let second =
            cx.update(|window, cx| cx.new(|cx| ThreadView::open(setup, Backend::Scripted(Vec::new()), window, cx)));
        assert_eq!(second.read_with(cx, |t, _| t.overview.clone()), overview);
        assert_eq!(second.read_with(cx, |t, _| t.overview_mark), 1);
        let _ = std::fs::remove_file(path);
    }

    #[gpui_kit::test]
    fn claudes_task_list_reaches_the_overview_text(cx: &mut TestAppContext) {
        let (window, thread) = open(cx);
        let cx = &mut VisualTestContext::from_window(window, cx);
        thread.update(cx, |t, cx| {
            let todos = serde_json::json!({ "todos": [
                { "content": "Write parser", "status": "completed" },
                { "content": "Add tests", "status": "in_progress" }
            ]});
            t.handle(AgentEvent::ToolStarted { id: "1".into(), name: "TodoWrite".into(), input: todos }, cx);
        });
        let plain = thread.read_with(cx, |t, _| t.plain.clone());
        assert_eq!(plain, vec!["(Claude updated its task list: [x] Write parser; [ ] Add tests)"]);
    }

    #[gpui_kit::test]
    fn tab_in_the_prompt_keeps_focus(cx: &mut TestAppContext) {
        let (window, thread) = open(cx);
        let cx = &mut VisualTestContext::from_window(window, cx);
        let focused = cx.update(|window, cx| {
            window.render_frame(cx);
            let focus = thread.read(cx).prompt_focus(cx);
            window.focus(&focus, cx);
            window.press("tab", cx);
            focus.is_focused(window)
        });
        assert!(focused, "Tab moved focus out of the prompt box");
    }
}
