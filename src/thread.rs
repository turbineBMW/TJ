//! A conversation with an agent: the transcript, approval cards, and the prompt box.

use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    path::PathBuf,
    sync::{Arc, Mutex},
};

use gpui_kit::base::{h_flex, v_flex};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Sizable as _,
    button::{Button, ButtonVariants as _},
    input::{Input, InputEvent, InputState, Textarea, TextareaState},
    menu::{DropdownMenu as _, PopupMenuItem},
    message_scroller::{MessageScroller, MessageScrollerState},
    text::{TextView, TextViewState},
};
use gpui_kit::{TestSupportExt as _, prelude::FluentBuilder as _, *};
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

/// Something the composer's menus ask the workspace to do.
#[derive(Clone, Debug, PartialEq)]
pub enum ThreadAction {
    SwitchAccount(String),
    /// Make the thread's model and effort the default for new threads in its project.
    MakeModelDefault,
    /// Make the thread's account the default for new threads in its project.
    MakeAccountDefault,
    /// Make the thread's mode the default for new threads in its project.
    MakeModeDefault,
    /// Start `claude auth login` for the thread's account in a terminal.
    LogIn,
}

impl EventEmitter<ThreadAction> for ThreadView {}

/// What the composer's menus offer, provided by the workspace.
#[derive(Clone, Default)]
pub struct MenuContext {
    /// (id, name) of every account.
    pub accounts: Vec<(String, String)>,
    pub project_name: String,
    pub project_account: String,
    pub project_mode: PermissionMode,
    /// The models the thread's account offers, and the project's default model and effort.
    pub models: Vec<crate::agent::ModelChoice>,
    pub project_model: Option<String>,
    pub project_effort: Option<String>,
    /// The project folder for display (`~/Projects/TJ`), and its git branch.
    pub project_dir: String,
    pub branch: Option<String>,
}

/// The model menu's width; descriptions wrap inside it.
const MODEL_MENU_WIDTH: f32 = 400.0;

/// The widest the conversation and composer get, so lines stay readable in a wide window.
const MAX_CONVERSATION_WIDTH: f32 = 860.0;

/// Who the thread is and where it keeps its transcript.
#[derive(Clone, Default)]
pub struct ThreadSetup {
    pub title: Option<String>,
    pub session_id: Option<String>,
    pub account_id: String,
    pub account_name: String,
    /// The model (`--model` value) and effort chosen for this thread; defaults when `None`.
    pub model_choice: Option<String>,
    pub effort: Option<String>,
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

#[derive(Clone, Copy, Debug, PartialEq)]
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
    /// When this call started a subagent: what it's doing and what it did.
    subagent: Option<Subagent>,
}

/// A subagent's work, shown inside the Agent tool row that started it.
#[derive(Default)]
struct Subagent {
    agent_type: Option<String>,
    /// What it's doing right now, while it runs.
    now: Option<String>,
    /// Its tool calls and messages, in order.
    log: Vec<String>,
    tool_uses: Option<u64>,
    tokens: Option<u64>,
    /// Set when it finishes: its status ("completed", "failed", ...) and report.
    finished: Option<String>,
    summary: Option<String>,
}

/// How many log lines a subagent keeps.
const SUBAGENT_LOG_LINES: usize = 60;

struct ApprovalRow {
    request: PermissionRequest,
    kind: ApprovalKind,
    /// `None` while waiting; otherwise what happened, shown in place of the buttons.
    outcome: Option<String>,
    /// Created on first render, since an input needs a window.
    feedback: Option<Entity<InputState>>,
}

/// What the agent is waiting on: an ordinary tool call, a question for the user, or a plan.
enum ApprovalKind {
    Tool,
    /// Boxed: a question card is much larger than the other transcript rows.
    Question(Box<QuestionCard>),
    Plan(Entity<TextViewState>),
}

struct Question {
    question: String,
    header: String,
    /// (label, description)
    options: Vec<(String, String)>,
    multi: bool,
}

struct QuestionCard {
    questions: Vec<Question>,
    /// The options picked for each question.
    picked: Vec<BTreeSet<usize>>,
    /// An "Other" answer for each question, created on first render.
    other: Vec<Option<Entity<InputState>>>,
}

impl QuestionCard {
    fn parse(input: &Value) -> Option<Self> {
        let questions: Vec<Question> = input
            .get("questions")?
            .as_array()?
            .iter()
            .filter_map(|q| {
                Some(Question {
                    question: q.get("question")?.as_str()?.to_string(),
                    header: q.get("header").and_then(Value::as_str).unwrap_or_default().to_string(),
                    options: q
                        .get("options")?
                        .as_array()?
                        .iter()
                        .filter_map(|o| {
                            Some((
                                o.get("label")?.as_str()?.to_string(),
                                o.get("description").and_then(Value::as_str).unwrap_or_default().to_string(),
                            ))
                        })
                        .collect(),
                    multi: q.get("multiSelect").and_then(Value::as_bool).unwrap_or(false),
                })
            })
            .collect();
        (!questions.is_empty()).then(|| Self {
            picked: vec![BTreeSet::new(); questions.len()],
            other: (0..questions.len()).map(|_| None).collect(),
            questions,
        })
    }

    /// Question text -> answer, when every question has one. An "Other" answer wins over picks.
    fn answers(&self, cx: &App) -> Option<BTreeMap<String, String>> {
        self.questions
            .iter()
            .enumerate()
            .map(|(qi, q)| {
                let other = self.other[qi].as_ref().map(|o| o.read(cx).value().trim().to_string()).unwrap_or_default();
                let answer = if !other.is_empty() {
                    other
                } else {
                    self.picked[qi].iter().map(|oi| q.options[*oi].0.clone()).collect::<Vec<_>>().join(", ")
                };
                (!answer.is_empty()).then(|| (q.question.clone(), answer))
            })
            .collect()
    }
}

// An approval row is several times larger than the others, but a transcript holds hundreds
// of rows at most, and boxing it would complicate every match on the cards.
#[allow(clippy::large_enum_variant)]
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
    /// What the account and mode menus offer.
    pub menus: MenuContext,
    /// The model and effort picked for this thread (`None`: Claude Code's defaults).
    pub model_choice: Option<String>,
    pub effort: Option<String>,
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
            menus: MenuContext::default(),
            model_choice: setup.model_choice.clone(),
            effort: setup.effort.clone(),
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
                        row.outcome = Some(outcome);
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
                row.outcome = Some("No longer needed".into());
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

    /// Pick the model; a running session switches at once, a new one starts with it.
    pub fn set_model_choice(&mut self, model: Option<String>, cx: &mut Context<Self>) {
        if self.model_choice == model {
            return;
        }
        self.model_choice = model.clone();
        if let Some(Backend::Claude(options)) = &mut self.backend {
            options.model = model.clone();
        }
        // The new model may not offer the current effort level; drop it if so.
        let efforts = self.model_info().map(|m| m.efforts.clone()).unwrap_or_default();
        if self.effort.as_ref().is_some_and(|e| !efforts.contains(e)) {
            self.set_effort(None, cx);
        }
        if let Some(agent) = &self.agent
            && let Err(err) = agent.set_model(model.as_deref())
        {
            self.notice(NoticeKind::Error, format!("Could not switch model: {err:#}"), cx);
        }
        cx.emit(ThreadChanged);
        cx.notify();
    }

    pub fn set_effort(&mut self, effort: Option<String>, cx: &mut Context<Self>) {
        if self.effort == effort {
            return;
        }
        self.effort = effort.clone();
        if let Some(Backend::Claude(options)) = &mut self.backend {
            options.effort = effort.clone();
        }
        if let Some(agent) = &self.agent
            && let Err(err) = agent.set_effort(effort.as_deref())
        {
            self.notice(NoticeKind::Error, format!("Could not change effort: {err:#}"), cx);
        }
        cx.emit(ThreadChanged);
        cx.notify();
    }

    /// The catalog entry for the picked model (or for "default").
    fn model_info(&self) -> Option<&crate::agent::ModelChoice> {
        let value = self.model_choice.as_deref().unwrap_or("default");
        self.menus.models.iter().find(|m| m.value == value)
    }

    /// "Opus 5.5 · High": the model a session runs, else the one the picked entry resolves
    /// to (also for "default"), else the entry's name; then the effort, when one is set.
    /// Descriptions are never used: accounts word them differently (some add pricing).
    pub fn model_label(&self) -> String {
        let info = self.model_info();
        let resolved = info.and_then(|m| m.resolved.as_deref()).map(short_model);
        let name = match &self.model_choice {
            // A session reports the model that really answered; after a switch it catches up.
            None => self.model.as_deref().map(short_model).or(resolved),
            Some(value) => resolved.or_else(|| info.map(|m| m.name.clone())).or_else(|| Some(value.clone())),
        }
        .unwrap_or_else(|| "Default model".into());
        match &self.effort {
            Some(effort) => format!("{name} · {}", capitalize(effort)),
            None => name,
        }
    }

    /// A thread that hasn't started yet shows the intro instead of an empty conversation.
    pub fn is_fresh(&self) -> bool {
        self.rows.is_empty()
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
        let is_question = matches!(row.kind, ApprovalKind::Question(_));
        let is_plan = matches!(row.kind, ApprovalKind::Plan(_));
        let outcome = match &decision {
            Decision::Approve => "Approved".to_string(),
            Decision::ApproveForSession => "Approved for this session".to_string(),
            Decision::Decline { .. } if is_question => "Skipped".to_string(),
            Decision::Decline { .. } if is_plan => "Kept planning".to_string(),
            Decision::Decline { .. } => "Declined".to_string(),
            Decision::Cancel => "Cancelled the turn".to_string(),
            Decision::Answer { answers } => {
                format!("Answered: {}", answers.values().cloned().collect::<Vec<_>>().join("; "))
            }
            Decision::StartPlan { accept_edits: true } => "Started, accepting edits".to_string(),
            Decision::StartPlan { accept_edits: false } => "Started, asking first".to_string(),
        };
        row.outcome = Some(outcome.clone());
        let request = row.request.clone();
        self.write(Record::Decision { request_id: request.id.clone(), outcome: outcome.clone() });
        if let Decision::Answer { answers } = &decision {
            for (question, answer) in answers {
                self.plain.push(format!("(User answered \"{question}\": {answer})"));
            }
        }
        if is_plan {
            self.plain.push(format!("(User: {outcome})"));
        }
        // Approving a plan leaves plan mode; show the mode the session now runs in.
        if let Decision::StartPlan { accept_edits } = &decision {
            let mode = if *accept_edits { PermissionMode::AcceptEdits } else { PermissionMode::Ask };
            self.mode = mode;
            if let Some(Backend::Claude(options)) = &mut self.backend {
                options.permission_mode = mode;
            }
        }
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

    fn tool_row(&self, tool_use_id: &str) -> Option<usize> {
        self.rows.iter().rposition(|r| matches!(r, Row::Tool(t) if t.id == tool_use_id))
    }

    /// Update the subagent started by tool call `tool_use_id` (creating it if events arrive
    /// before its start was seen).
    fn with_subagent(&mut self, tool_use_id: &str, cx: &mut Context<Self>, update: impl FnOnce(&mut Subagent)) {
        let Some(ix) = self.tool_row(tool_use_id) else { return };
        if let Row::Tool(tool) = &mut self.rows[ix] {
            let sub = tool.subagent.get_or_insert_with(Subagent::default);
            update(sub);
            if sub.log.len() > SUBAGENT_LOG_LINES {
                let extra = sub.log.len() - SUBAGENT_LOG_LINES;
                sub.log.drain(..extra);
            }
        }
        self.remeasure(ix, cx);
    }

    /// Pick (or, for a multi-select, toggle) option `oi` of question `qi` on card `ix`.
    pub fn pick(&mut self, ix: usize, qi: usize, oi: usize, cx: &mut Context<Self>) {
        if let Some(Row::Approval(ApprovalRow { kind: ApprovalKind::Question(card), outcome: None, .. })) =
            self.rows.get_mut(ix)
            && let (Some(q), Some(picked)) = (card.questions.get(qi), card.picked.get_mut(qi))
        {
            if q.multi {
                if !picked.remove(&oi) {
                    picked.insert(oi);
                }
            } else {
                picked.clear();
                picked.insert(oi);
            }
        }
        self.remeasure(ix, cx);
    }

    /// Send the question card's answers, if every question has one.
    pub fn submit_answers(&mut self, ix: usize, cx: &mut Context<Self>) {
        let answers = match self.rows.get(ix) {
            Some(Row::Approval(ApprovalRow { kind: ApprovalKind::Question(card), .. })) => card.answers(cx),
            _ => None,
        };
        if let Some(answers) = answers {
            self.decide(ix, Decision::Answer { answers }, cx);
        }
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
        // Claude can start a turn on its own (to report a background subagent's result).
        if !self.replaying
            && self.status == Status::Idle
            && matches!(event, AgentEvent::TextStarted | AgentEvent::TextDelta(_) | AgentEvent::ToolStarted { .. })
        {
            self.status = Status::Running;
        }
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
            AgentEvent::SubagentStarted { tool_use_id, description, agent_type } => {
                if let Some(ix) = self.tool_row(&tool_use_id) {
                    if let Row::Tool(tool) = &mut self.rows[ix] {
                        tool.status = ToolStatus::Running;
                        if !description.is_empty() {
                            tool.summary = description;
                        }
                        tool.subagent = Some(Subagent { agent_type, ..Default::default() });
                    }
                    self.remeasure(ix, cx);
                }
            }
            AgentEvent::SubagentProgress { tool_use_id, description, tool_uses, tokens } => {
                self.with_subagent(&tool_use_id, cx, |sub| {
                    if !description.is_empty() {
                        sub.now = Some(description);
                    }
                    sub.tool_uses = tool_uses.or(sub.tool_uses);
                    sub.tokens = tokens.or(sub.tokens);
                });
            }
            AgentEvent::SubagentTool { tool_use_id, name, input } => {
                let line = format!("{name}: {}", truncate(&tool_summary(&name, &input), 200));
                self.with_subagent(&tool_use_id, cx, |sub| {
                    sub.now = Some(line.clone());
                    sub.log.push(line);
                });
            }
            AgentEvent::SubagentText { tool_use_id, text } => {
                self.with_subagent(&tool_use_id, cx, |sub| sub.log.push(truncate(text.trim(), 600)));
            }
            AgentEvent::SubagentFinished { tool_use_id, status, summary } => {
                if let Some(ix) = self.tool_row(&tool_use_id)
                    && let Row::Tool(tool) = &mut self.rows[ix]
                {
                    tool.status = if status == "completed" { ToolStatus::Done } else { ToolStatus::Failed };
                    let sub = tool.subagent.get_or_insert_with(Subagent::default);
                    sub.now = None;
                    sub.finished = Some(status.clone());
                    sub.summary = summary.clone();
                    // The report is what the overview writer most needs from a subagent.
                    self.plain.push(format!(
                        "(Subagent \"{}\" {status}: {})",
                        tool.summary,
                        truncate(summary.as_deref().unwrap_or(""), 800)
                    ));
                    self.plain_current = None;
                    self.remeasure(ix, cx);
                }
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
                // Questions and plans get their own cards; a tool row would only repeat them.
                if matches!(name.as_str(), "AskUserQuestion" | "ExitPlanMode") {
                    return;
                }
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
                        subagent: None,
                    }),
                    cx,
                );
            }
            AgentEvent::ToolFinished { id, output, is_error } => {
                if let Some(ix) = self.rows.iter().rposition(|r| matches!(r, Row::Tool(t) if t.id == id)) {
                    if let Row::Tool(tool) = &mut self.rows[ix] {
                        // A background subagent's call returns at once ("Async agent launched");
                        // the row stays working until the subagent itself finishes.
                        if tool.subagent.as_ref().is_some_and(|s| s.finished.is_none()) && !is_error {
                            self.remeasure(ix, cx);
                            return;
                        }
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
                let kind = match request.tool_name.as_str() {
                    "AskUserQuestion" => QuestionCard::parse(&request.input)
                        .map(|card| ApprovalKind::Question(Box::new(card)))
                        .unwrap_or(ApprovalKind::Tool),
                    "ExitPlanMode" => match request.input.get("plan").and_then(Value::as_str) {
                        Some(plan) => {
                            self.plain.push(format!("(Claude proposed a plan:\n{})", truncate(plan, 1500)));
                            let plan = plan.to_string();
                            ApprovalKind::Plan(cx.new(|cx| TextViewState::markdown(&plan, cx).selectable(true)))
                        }
                        None => ApprovalKind::Tool,
                    },
                    _ => ApprovalKind::Tool,
                };
                self.push_row(Row::Approval(ApprovalRow { request, kind, outcome: None, feedback: None }), cx);
                self.status = Status::AwaitingApproval;
            }
            AgentEvent::PermissionWithdrawn { id } => {
                if let Some(ix) = self.rows.iter().position(|r| matches!(r, Row::Approval(a) if a.request.id == id)) {
                    if let Row::Approval(row) = &mut self.rows[ix]
                        && row.outcome.is_none()
                    {
                        row.outcome = Some("No longer needed".into());
                    }
                    self.remeasure(ix, cx);
                }
            }
            AgentEvent::ModelInUse(model) => self.model = Some(model),
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
    pub fn approval_outcome(&self, ix: usize) -> Option<String> {
        match self.rows.get(ix) {
            Some(Row::Approval(row)) => row.outcome.clone(),
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
        {
            if row.feedback.is_none() {
                let hint = match row.kind {
                    ApprovalKind::Plan(_) => "Optional: what should change in the plan?",
                    _ => "Optional: tell Claude why, or what to do instead",
                };
                row.feedback = Some(cx.new(|cx| InputState::new(window, cx).placeholder(hint)));
            }
            if let ApprovalKind::Question(card) = &mut row.kind {
                for other in card.other.iter_mut().filter(|o| o.is_none()) {
                    *other = Some(cx.new(|cx| InputState::new(window, cx).placeholder("Or type your own answer")));
                }
            }
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
                let (dot, status) = match tool.status {
                    ToolStatus::Running => (muted, "running"),
                    ToolStatus::Done => (theme.success, "done"),
                    ToolStatus::Failed => (danger, "failed"),
                };
                let sub = tool.subagent.as_ref();
                // A subagent's row says what it is and how far along it is.
                let label = match sub {
                    Some(sub) => {
                        let mut parts =
                            vec![if sub.finished.is_none() { "working".to_string() } else { status.to_string() }];
                        if let Some(n) = sub.tool_uses {
                            parts.push(format!("{n} tool{}", if n == 1 { "" } else { "s" }));
                        }
                        if let Some(t) = sub.tokens {
                            parts.push(format!("{}k tokens", t / 1000));
                        }
                        parts.join(" · ")
                    }
                    None => status.to_string(),
                };
                let name = match sub.and_then(|s| s.agent_type.clone()) {
                    Some(agent_type) => format!("{} · {agent_type}", tool.name),
                    None => tool.name.clone(),
                };
                let now = sub.and_then(|s| s.now.clone()).filter(|_| tool.status == ToolStatus::Running);
                let details: Option<AnyElement> = if !tool.expanded {
                    None
                } else if let Some(sub) = sub {
                    Some(
                        v_flex()
                            .gap_1()
                            .children(sub.log.iter().map(|line| {
                                div().text_xs().font_family(mono.clone()).text_color(muted).child(line.clone())
                            }))
                            .when_some(sub.summary.clone(), |el, summary| {
                                el.child(div().pt_1().text_sm().child(summary))
                            })
                            .into_any_element(),
                    )
                } else {
                    tool.output.clone().map(|output| {
                        div()
                            .text_xs()
                            .font_family(mono.clone())
                            .text_color(muted)
                            .child(truncate(&output, 4000))
                            .into_any_element()
                    })
                };
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
                                    .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).child(name))
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
                            .when_some(now, |el, now| {
                                el.child(
                                    div()
                                        .px_3()
                                        .pb_1p5()
                                        .text_xs()
                                        .text_color(muted)
                                        .overflow_hidden()
                                        .whitespace_nowrap()
                                        .text_ellipsis()
                                        .child(format!("↳ {now}")),
                                )
                            })
                            .when_some(details, |el, details| {
                                el.child(div().border_t_1().border_color(border).px_3().py_2().child(details))
                            }),
                    )
                    .into_any_element()
            }
            Row::Approval(ApprovalRow { kind: ApprovalKind::Question(card), outcome, .. }) => {
                let pending = outcome.is_none();
                let complete = card.answers(cx).is_some();
                let fg = theme.foreground;
                let questions = card.questions.iter().enumerate().map(|(qi, q)| {
                    let options = q.options.iter().enumerate().map(|(oi, (label, description))| {
                        let picked = card.picked[qi].contains(&oi);
                        let mark = match (q.multi, picked) {
                            (true, true) => "☑",
                            (true, false) => "☐",
                            (false, true) => "●",
                            (false, false) => "○",
                        };
                        h_flex()
                            .id(SharedString::from(format!("option-{ix}-{qi}-{oi}")))
                            .test_support()
                            .gap_2()
                            .items_start()
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .when(pending, |el| el.cursor_pointer().hover(|el| el.bg(secondary)))
                            .when(picked, |el| el.bg(secondary))
                            .on_click(cx.listener(move |this, _, _, cx| this.pick(ix, qi, oi, cx)))
                            .child(div().text_color(if picked { accent } else { muted }).child(mark))
                            .child(
                                v_flex()
                                    .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).child(label.clone()))
                                    .when(!description.is_empty(), |el| {
                                        el.child(div().text_xs().text_color(muted).child(description.clone()))
                                    }),
                            )
                    });
                    v_flex()
                        .gap_1()
                        .child(
                            h_flex()
                                .gap_2()
                                .when(!q.header.is_empty(), |el| {
                                    el.child(
                                        div()
                                            .px_1p5()
                                            .rounded_sm()
                                            .bg(secondary)
                                            .text_xs()
                                            .text_color(muted)
                                            .child(q.header.clone()),
                                    )
                                })
                                .child(div().text_sm().text_color(fg).child(q.question.clone())),
                        )
                        .children(options)
                        .when(pending, |el| {
                            el.when_some(card.other[qi].as_ref(), |el, o| el.child(Input::new(o).small()))
                        })
                });
                pad(div())
                    .child(
                        v_flex()
                            .gap_3()
                            .p_3()
                            .rounded_md()
                            .border_1()
                            .border_color(if pending { accent } else { border })
                            .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).child("Claude is asking"))
                            .children(questions)
                            .map(|el| match outcome {
                                Some(outcome) => el.child(div().text_sm().text_color(muted).child(outcome.clone())),
                                None => el.child(
                                    h_flex()
                                        .gap_2()
                                        .child(
                                            Button::new(SharedString::from(format!("answer-{ix}")))
                                                .primary()
                                                .small()
                                                .label("Answer")
                                                .disabled(!complete)
                                                .on_click(
                                                    cx.listener(move |this, _, _, cx| this.submit_answers(ix, cx)),
                                                ),
                                        )
                                        .child(
                                            Button::new(SharedString::from(format!("skip-{ix}")))
                                                .ghost()
                                                .small()
                                                .label("Skip")
                                                .on_click(cx.listener(move |this, _, _, cx| {
                                                    this.decide(ix, Decision::Decline { feedback: None }, cx)
                                                })),
                                        ),
                                ),
                            }),
                    )
                    .into_any_element()
            }
            Row::Approval(ApprovalRow { kind: ApprovalKind::Plan(plan), outcome, feedback, .. }) => {
                let pending = outcome.is_none();
                pad(div())
                    .child(
                        v_flex()
                            .gap_2()
                            .p_3()
                            .rounded_md()
                            .border_1()
                            .border_color(if pending { accent } else { border })
                            .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).child("Claude's plan"))
                            .child(
                                div()
                                    .px_3()
                                    .py_2()
                                    .rounded_md()
                                    .bg(secondary)
                                    .child(TextView::new(plan).selectable(true)),
                            )
                            .map(|el| match outcome {
                                Some(outcome) => el.child(div().text_sm().text_color(muted).child(outcome.clone())),
                                None => el.when_some(feedback.as_ref(), |el, f| el.child(Input::new(f).small())).child(
                                    h_flex()
                                        .gap_2()
                                        .child(
                                            Button::new(SharedString::from(format!("plan-accept-{ix}")))
                                                .primary()
                                                .small()
                                                .label("Start, accepting edits")
                                                .on_click(cx.listener(move |this, _, _, cx| {
                                                    this.decide(ix, Decision::StartPlan { accept_edits: true }, cx)
                                                })),
                                        )
                                        .child(
                                            Button::new(SharedString::from(format!("plan-ask-{ix}")))
                                                .small()
                                                .label("Start, asking first")
                                                .on_click(cx.listener(move |this, _, _, cx| {
                                                    this.decide(ix, Decision::StartPlan { accept_edits: false }, cx)
                                                })),
                                        )
                                        .child(
                                            Button::new(SharedString::from(format!("plan-keep-{ix}")))
                                                .ghost()
                                                .small()
                                                .label("Keep planning")
                                                .on_click(cx.listener(move |this, _, _, cx| {
                                                    this.decide(ix, Decision::Decline { feedback: None }, cx)
                                                })),
                                        ),
                                ),
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
                            .map(|el| match &row.outcome {
                                Some(outcome) => el.child(div().text_sm().text_color(muted).child(outcome.clone())),
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
        self.render_view(cx)
    }
}

impl ThreadView {
    fn render_view(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (bg, border, muted) = (theme.background, theme.border, theme.muted_foreground);
        let this = cx.entity();
        let running = self.status != Status::Idle;

        let transcript = MessageScroller::new("transcript", self.scroller.clone(), move |ix, window, cx| {
            this.update(cx, |thread, cx| {
                let row =
                    if ix < thread.rows.len() { thread.render_row(ix, window, cx) } else { div().into_any_element() };
                // Rows stay centred and no wider than a comfortable reading width.
                h_flex().w_full().justify_center().child(div().w_full().max_w(px(MAX_CONVERSATION_WIDTH)).child(row))
            })
        })
        .scrollbar(true)
        .jump_button(true)
        .with_bottom_fade(bg);

        let menus = self.menus.clone();
        let (account_id, account_name, mode) = (self.account_id.clone(), self.account_name.clone(), self.mode);
        let thread = cx.entity();

        let account_menu = Button::new("account-menu")
            .ghost()
            .xsmall()
            .label(format!("Claude · {account_name}"))
            .icon(gpui_kit::assets::IconName::ChevronDown)
            .dropdown_menu({
                let (menus, thread, account_id, account_name) =
                    (menus.clone(), thread.clone(), account_id.clone(), account_name.clone());
                move |menu, _window, _cx| {
                    let mut menu = menu.label("Account");
                    for (id, name) in &menus.accounts {
                        let (thread, id) = (thread.clone(), id.clone());
                        menu = menu.item(PopupMenuItem::new(name.clone()).checked(id == account_id).on_click(
                            move |_, _, cx| thread.update(cx, |_, cx| cx.emit(ThreadAction::SwitchAccount(id.clone()))),
                        ));
                    }
                    let (t1, t2) = (thread.clone(), thread.clone());
                    menu.separator()
                        .item(
                            PopupMenuItem::new(format!("Use {account_name} for new threads in {}", menus.project_name))
                                .checked(account_id == menus.project_account)
                                .on_click(move |_, _, cx| {
                                    t1.update(cx, |_, cx| cx.emit(ThreadAction::MakeAccountDefault))
                                }),
                        )
                        .item(
                            PopupMenuItem::new(format!("Log in to {account_name}…"))
                                .on_click(move |_, _, cx| t2.update(cx, |_, cx| cx.emit(ThreadAction::LogIn))),
                        )
                }
            });

        let mode_menu = Button::new("mode-menu")
            .ghost()
            .xsmall()
            .label(mode.label())
            .icon(gpui_kit::assets::IconName::ChevronDown)
            .dropdown_menu({
                let (menus, thread) = (menus.clone(), thread.clone());
                move |menu, _window, _cx| {
                    let mut menu = menu.label("Permission mode");
                    for m in PermissionMode::ALL {
                        let thread = thread.clone();
                        menu = menu.item(
                            PopupMenuItem::new(m.label())
                                .checked(m == mode)
                                .on_click(move |_, _, cx| thread.update(cx, |t, cx| t.set_mode(m, cx))),
                        );
                    }
                    let thread = thread.clone();
                    menu.separator().item(
                        PopupMenuItem::new(format!("Make {} the default for {}", mode.label(), menus.project_name))
                            .checked(mode == menus.project_mode)
                            .on_click(move |_, _, cx| {
                                thread.update(cx, |_, cx| cx.emit(ThreadAction::MakeModeDefault))
                            }),
                    )
                }
            });

        let model_menu = Button::new("model-menu")
            .ghost()
            .xsmall()
            .label(self.model_label())
            .icon(gpui_kit::assets::IconName::ChevronDown)
            .dropdown_menu({
                let (menus, thread) = (menus.clone(), thread.clone());
                let (choice, effort) = (self.model_choice.clone(), self.effort.clone());
                let efforts = self.model_info().map(|m| m.efforts.clone()).unwrap_or_default();
                move |menu, _window, _cx| {
                    // Two lines per model: its name, then its description (long on some accounts,
                    // with pricing), wrapping inside a fixed-width menu.
                    let mut menu = menu.min_w(px(MODEL_MENU_WIDTH)).max_w(px(MODEL_MENU_WIDTH)).label("Model");
                    for model in &menus.models {
                        let (thread, value) = (thread.clone(), model.value.clone());
                        let picked = choice.as_deref().unwrap_or("default") == model.value;
                        let (name, description) = model_menu_text(model);
                        let item = PopupMenuItem::element(move |_, cx| {
                            let muted = cx.theme().muted_foreground;
                            v_flex()
                                .w(px(MODEL_MENU_WIDTH - 56.0))
                                .py_0p5()
                                .child(div().text_sm().child(name.clone()))
                                .when(!description.is_empty(), |el| {
                                    el.child(div().text_xs().text_color(muted).child(description.clone()))
                                })
                        });
                        menu = menu.item(item.checked(picked).on_click(move |_, _, cx| {
                            let value = (value != "default").then(|| value.clone());
                            thread.update(cx, |t, cx| t.set_model_choice(value, cx))
                        }));
                    }
                    if !efforts.is_empty() {
                        menu = menu.separator().label("Effort");
                        let t = thread.clone();
                        menu = menu.item(
                            PopupMenuItem::new("Model's default")
                                .checked(effort.is_none())
                                .on_click(move |_, _, cx| t.update(cx, |t, cx| t.set_effort(None, cx))),
                        );
                        for level in &efforts {
                            let (thread, level) = (thread.clone(), level.clone());
                            menu = menu.item(
                                PopupMenuItem::new(capitalize(&level))
                                    .checked(effort.as_deref() == Some(level.as_str()))
                                    .on_click(move |_, _, cx| {
                                        let level = level.clone();
                                        thread.update(cx, |t, cx| t.set_effort(Some(level), cx))
                                    }),
                            );
                        }
                    }
                    let is_default = choice == menus.project_model && effort == menus.project_effort;
                    let thread = thread.clone();
                    menu.separator().item(
                        PopupMenuItem::new(format!("Use for new threads in {}", menus.project_name))
                            .checked(is_default)
                            .on_click(move |_, _, cx| {
                                thread.update(cx, |_, cx| cx.emit(ThreadAction::MakeModelDefault))
                            }),
                    )
                }
            });

        let send = if running {
            Button::new("stop").small().label("Stop").on_click(cx.listener(|this, _, _, cx| this.interrupt(cx)))
        } else {
            Button::new("send")
                .primary()
                .small()
                .label("Send")
                .disabled(self.agent.is_none() && self.backend.is_none())
                .on_click(cx.listener(|this, _, window, cx| this.submit(window, cx)))
        };

        if self.is_fresh() {
            return self.render_intro(account_menu, mode_menu, model_menu, cx).into_any_element();
        }

        v_flex()
            .size_full()
            .bg(bg)
            .child(div().flex_1().min_h_0().child(transcript))
            .child(
                h_flex().w_full().justify_center().border_t_1().border_color(border).child(
                    v_flex()
                        .w_full()
                        .max_w(px(MAX_CONVERSATION_WIDTH))
                        .gap_1()
                        .px_3()
                        .pt_3()
                        .pb_2()
                        .child(Textarea::new(&self.prompt))
                        .child(
                            h_flex()
                                .gap_1()
                                .text_color(muted)
                                .child(account_menu)
                                .child(mode_menu)
                                .child(div().flex_1())
                                .child(model_menu)
                                .child(send),
                        ),
                ),
            )
            .into_any_element()
    }

    /// Before the first message: a heading, a large prompt card, and where the session will run.
    /// Like T3 Code's new-session screen.
    fn render_intro(
        &self,
        account_menu: impl IntoElement,
        mode_menu: impl IntoElement,
        model_menu: impl IntoElement,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let (bg, border, muted, fg, card) =
            (theme.background, theme.border, theme.muted_foreground, theme.foreground, theme.secondary);
        let (accent, accent_fg) = (theme.primary, theme.primary_foreground);
        let project = if self.menus.project_name.is_empty() {
            "this project".to_string()
        } else {
            self.menus.project_name.clone()
        };
        let can_send = self.agent.is_some() || self.backend.is_some();
        let send = div()
            .id("intro-send")
            .flex_none()
            .size_8()
            .rounded_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(if can_send { accent } else { muted })
            .text_color(accent_fg)
            .cursor_pointer()
            .child(gpui_kit::component::Icon::new(gpui_kit::assets::IconName::ArrowUp).small())
            .on_click(cx.listener(|this, _, window, cx| this.submit(window, cx)));
        let footnote = |icon: gpui_kit::assets::IconName, text: String| {
            h_flex()
                .gap_1()
                .text_xs()
                .text_color(muted)
                .child(gpui_kit::component::Icon::new(icon).xsmall())
                .child(text)
        };

        // A little above centre, like T3 Code's start screen.
        v_flex().size_full().bg(bg).items_center().justify_center().px_6().pb(px(140.)).child(
            v_flex()
                .w_full()
                .max_w(px(MAX_CONVERSATION_WIDTH - 80.0))
                .gap_2()
                .child(
                    div()
                        .w_full()
                        .pb_4()
                        .flex()
                        .justify_center()
                        .text_3xl()
                        .text_color(fg)
                        .child(format!("What should we build in {project}?")),
                )
                .child(
                    v_flex()
                        .w_full()
                        .gap_2()
                        .p_3()
                        .rounded_xl()
                        .border_1()
                        .border_color(border)
                        .bg(card)
                        .child(div().min_h(px(64.)).child(Textarea::new(&self.prompt).appearance(false)))
                        .child(
                            h_flex()
                                .gap_1()
                                .child(account_menu)
                                .child(mode_menu)
                                .child(div().flex_1())
                                .child(model_menu)
                                .child(send),
                        ),
                )
                .child(
                    h_flex()
                        .px_2()
                        .gap_4()
                        .child(footnote(gpui_kit::assets::IconName::Folder, self.menus.project_dir.clone()))
                        .child(div().flex_1())
                        .when_some(self.menus.branch.clone(), |el, branch| {
                            el.child(footnote(gpui_kit::assets::IconName::GitBranch, branch))
                        })
                        .child(footnote(gpui_kit::assets::IconName::Shield, self.mode.label().to_string())),
                ),
        )
    }
}

fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    chars.next().map(|c| c.to_uppercase().chain(chars).collect()).unwrap_or_default()
}

/// A thread title from its first prompt: the first line, shortened.
pub fn title_from(prompt: &str) -> String {
    let line = prompt.lines().find(|l| !l.trim().is_empty()).unwrap_or("New thread").trim();
    truncate(line, 60)
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

/// A model's two menu lines. The version goes in the title ("Opus 5.5") and leaves the
/// description, when the description starts with it ("Opus 5.5 · Best for ..."). The default
/// entry keeps its name and gets the version only if the description doesn't already say it.
pub fn model_menu_text(model: &crate::agent::ModelChoice) -> (String, String) {
    let Some(version) = model.resolved.as_deref().map(short_model).filter(|v| v.contains(' ')) else {
        return (model.name.clone(), model.description.clone());
    };
    let description = model
        .description
        .strip_prefix(version.as_str())
        .and_then(|rest| rest.strip_prefix(" · "))
        .unwrap_or(&model.description)
        .to_string();
    let family = version.split(' ').next().unwrap_or_default();
    let title = if model.name.split(' ').next().is_some_and(|word| word.eq_ignore_ascii_case(family)) {
        version
    } else if description.contains(&version) {
        model.name.clone()
    } else {
        format!("{} · {version}", model.name)
    };
    (title, description)
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
        assert_eq!(thread.read_with(cx, |t, _| t.approval_outcome(ix)).as_deref(), Some("Approved"));
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
        assert_eq!(thread.read_with(cx, |t, _| t.approval_outcome(ix)).as_deref(), Some("Declined"));
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
        assert_eq!(b.2.as_deref(), Some("Approved"));
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

    fn first_card(thread: &Entity<ThreadView>, events: Vec<AgentEvent>, cx: &mut VisualTestContext) -> usize {
        let ix = until_approval(thread, events, cx);
        cx.update(|window, cx| window.render_frame(cx));
        ix
    }

    fn responses(thread: &Entity<ThreadView>, cx: &mut VisualTestContext) -> Vec<Decision> {
        calls(thread, cx)
            .into_iter()
            .filter_map(|c| match c {
                ScriptedCall::Respond { decision, .. } => Some(decision),
                _ => None,
            })
            .collect()
    }

    #[gpui_kit::test]
    fn a_question_is_answered_by_picking_an_option(cx: &mut TestAppContext) {
        let (window, thread) = open(cx);
        let cx = &mut VisualTestContext::from_window(window, cx);
        let ix = first_card(&thread, fixture("2.1.331-question.jsonl"), cx);
        cx.update(|window, cx| {
            window.click(gpui_kit::SharedString::from(format!("option-{ix}-0-1")), cx);
            window.render_frame(cx);
            window.click(gpui_kit::SharedString::from(format!("answer-{ix}")), cx);
        });
        let expected = std::collections::BTreeMap::from([(
            "Which colour do you prefer for a button?".to_string(),
            "Blue".to_string(),
        )]);
        assert_eq!(responses(&thread, cx), vec![Decision::Answer { answers: expected }]);
        assert_eq!(thread.read_with(cx, |t, _| t.approval_outcome(ix)).as_deref(), Some("Answered: Blue"));
    }

    #[gpui_kit::test]
    fn multi_select_and_other_answers_are_combined_and_required(cx: &mut TestAppContext) {
        let (window, thread) = open(cx);
        let cx = &mut VisualTestContext::from_window(window, cx);
        let request = crate::agent::PermissionRequest {
            id: "q1".into(),
            tool_name: "AskUserQuestion".into(),
            tool_use_id: None,
            description: None,
            input: serde_json::json!({ "questions": [
                { "question": "Which checks?", "header": "Checks", "multiSelect": true,
                  "options": [{ "label": "Tests", "description": "" }, { "label": "Lint", "description": "" }] },
                { "question": "Which branch?", "header": "Branch", "multiSelect": false,
                  "options": [{ "label": "main", "description": "" }, { "label": "dev", "description": "" }] }
            ]}),
            suggestions: Vec::new(),
            reason: None,
            no_standing_grant: false,
        };
        let ix = first_card(&thread, vec![AgentEvent::PermissionRequested(request)], cx);
        cx.update(|window, cx| {
            window.click(gpui_kit::SharedString::from(format!("option-{ix}-0-0")), cx);
            window.click(gpui_kit::SharedString::from(format!("option-{ix}-0-1")), cx);
            window.render_frame(cx);
        });
        // The second question isn't answered yet, so nothing can be sent.
        thread.update(cx, |t, cx| t.submit_answers(ix, cx));
        assert!(responses(&thread, cx).is_empty());

        cx.update(|window, cx| {
            let other = thread.read_with(cx, |t, _| match &t.rows[ix] {
                super::Row::Approval(super::ApprovalRow { kind: super::ApprovalKind::Question(card), .. }) => {
                    card.other[1].clone().unwrap()
                }
                _ => panic!("not a question card"),
            });
            other.update(cx, |o, cx| o.set_value("release/2.0", window, cx));
        });
        thread.update(cx, |t, cx| t.submit_answers(ix, cx));
        let expected = std::collections::BTreeMap::from([
            ("Which checks?".to_string(), "Tests, Lint".to_string()),
            ("Which branch?".to_string(), "release/2.0".to_string()),
        ]);
        assert_eq!(responses(&thread, cx), vec![Decision::Answer { answers: expected }]);
    }

    #[gpui_kit::test]
    fn approving_a_plan_starts_work_and_switches_the_mode(cx: &mut TestAppContext) {
        let (window, thread) = open(cx);
        let cx = &mut VisualTestContext::from_window(window, cx);
        thread.update(cx, |t, cx| t.set_mode(crate::agent::PermissionMode::Plan, cx));
        let ix = first_card(&thread, fixture("2.1.331-plan.jsonl"), cx);
        cx.update(|window, cx| window.click(gpui_kit::SharedString::from(format!("plan-accept-{ix}")), cx));
        assert_eq!(responses(&thread, cx), vec![Decision::StartPlan { accept_edits: true }]);
        assert_eq!(thread.read_with(cx, |t, _| t.mode), crate::agent::PermissionMode::AcceptEdits);
        assert_eq!(thread.read_with(cx, |t, _| t.approval_outcome(ix)).as_deref(), Some("Started, accepting edits"));
        assert!(thread.read_with(cx, |t, _| t.plain.iter().any(|l| l.starts_with("(Claude proposed a plan:"))));
    }

    #[gpui_kit::test]
    fn keeping_a_plan_sends_the_feedback(cx: &mut TestAppContext) {
        let (window, thread) = open(cx);
        let cx = &mut VisualTestContext::from_window(window, cx);
        let ix = first_card(&thread, fixture("2.1.331-plan.jsonl"), cx);
        cx.update(|window, cx| {
            let feedback = thread.read(cx).feedback_input(ix).unwrap();
            feedback.update(cx, |f, cx| f.set_value("Also add a test", window, cx));
            window.click(gpui_kit::SharedString::from(format!("plan-keep-{ix}")), cx);
        });
        assert_eq!(responses(&thread, cx), vec![Decision::Decline { feedback: Some("Also add a test".into()) }]);
        assert_eq!(thread.read_with(cx, |t, _| t.approval_outcome(ix)).as_deref(), Some("Kept planning"));
    }

    #[gpui_kit::test]
    fn a_background_subagent_stays_working_until_it_reports(cx: &mut TestAppContext) {
        let (window, thread) = open(cx);
        let cx = &mut VisualTestContext::from_window(window, cx);
        let events = fixture("2.1.331-subagent.jsonl");
        let first_turn_end = events.iter().position(|e| matches!(e, AgentEvent::TurnFinished { .. })).unwrap();
        let agent_row = |t: &ThreadView| {
            t.rows
                .iter()
                .find_map(|r| match r {
                    super::Row::Tool(tool) if tool.name == "Agent" => Some((
                        tool.status,
                        tool.subagent.as_ref().map(|s| (s.finished.clone(), s.log.len(), s.summary.is_some())),
                    )),
                    _ => None,
                })
                .unwrap()
        };

        thread.update(cx, |t, cx| {
            t.send("Count the files", cx);
            for event in events[..=first_turn_end].iter().cloned() {
                t.handle(event, cx);
            }
        });
        let (status, sub) = thread.read_with(cx, |t, _| agent_row(t));
        assert_eq!(status, super::ToolStatus::Running, "the instant 'launched' result doesn't finish the row");
        assert_eq!(sub.as_ref().map(|s| s.0.clone()), Some(None));
        assert_eq!(thread.read_with(cx, |t, _| t.status), Status::Idle, "the first turn ended");

        // Claude's own follow-up turn: the subagent finishes, then Claude reports.
        let mut saw_running = false;
        for event in events[first_turn_end + 1..].iter().cloned() {
            thread.update(cx, |t, cx| t.handle(event, cx));
            saw_running |= thread.read_with(cx, |t, _| t.status) == Status::Running;
        }
        assert!(saw_running, "a turn Claude starts on its own shows as working");
        let (status, sub) = thread.read_with(cx, |t, _| agent_row(t));
        let (finished, log_lines, has_summary) = sub.unwrap();
        assert_eq!(status, super::ToolStatus::Done);
        assert_eq!(finished.as_deref(), Some("completed"));
        assert!(log_lines >= 2, "its tool call and its text are logged");
        assert!(has_summary);
        assert!(thread.read_with(cx, |t, _| t.plain.iter().any(|l| l.starts_with("(Subagent "))));
        assert_eq!(thread.read_with(cx, |t, _| t.status), Status::Idle);
    }

    #[test]
    fn model_versions_move_from_the_description_to_the_title() {
        use super::model_menu_text;
        use crate::agent::ModelChoice;
        let entry = |value: &str, name: &str, resolved: Option<&str>, description: &str| ModelChoice {
            value: value.into(),
            name: name.into(),
            description: description.into(),
            resolved: resolved.map(str::to_owned),
            efforts: vec![],
        };
        let text = |m: ModelChoice| {
            let (title, description) = model_menu_text(&m);
            (title, description)
        };
        // The Work account's wording.
        assert_eq!(
            text(entry(
                "opus",
                "Opus",
                Some("claude-opus-5-5"),
                "Opus 5.5 · Best for everyday, complex tasks · $4/$20 per Mtok"
            )),
            ("Opus 5.5".into(), "Best for everyday, complex tasks · $4/$20 per Mtok".into())
        );
        assert_eq!(
            text(entry(
                "default",
                "Default (recommended)",
                Some("claude-opus-5-5"),
                "Use the default model (currently Opus 5.5) · $4/$20 per Mtok"
            )),
            ("Default (recommended)".into(), "Use the default model (currently Opus 5.5) · $4/$20 per Mtok".into())
        );
        // A default whose description starts with the version takes it into the title.
        assert_eq!(
            text(entry(
                "default",
                "Default (recommended)",
                Some("claude-opus-5-5"),
                "Opus 5.5 · Best for everyday tasks"
            )),
            ("Default (recommended) · Opus 5.5".into(), "Best for everyday tasks".into())
        );
        // A full id, and an entry claude didn't resolve.
        assert_eq!(
            text(entry("claude-haiku-4-5-20251001", "Haiku 4.5", Some("claude-haiku-4-5-20251001"), "Fastest")),
            ("Haiku 4.5".into(), "Fastest".into())
        );
        assert_eq!(text(entry("opus", "Opus", None, "Opus 5.5 · Best")), ("Opus".into(), "Opus 5.5 · Best".into()));
    }

    #[gpui_kit::test]
    fn picking_a_model_and_effort_reaches_a_running_session(cx: &mut TestAppContext) {
        use crate::agent::ModelChoice;
        let (window, thread) = open(cx);
        let cx = &mut VisualTestContext::from_window(window, cx);
        let model = |value: &str, name: &str, efforts: &[&str]| ModelChoice {
            value: value.into(),
            name: name.into(),
            description: String::new(),
            resolved: None,
            efforts: efforts.iter().map(|e| e.to_string()).collect(),
        };
        thread.update(cx, |t, cx| {
            t.menus.models = vec![
                // Worded like the Work account's list: the label must not come from this text.
                ModelChoice {
                    description: "Use the default model (currently Opus 5.5) · $4/$20 per Mtok".into(),
                    resolved: Some("claude-opus-5-5".into()),
                    ..model("default", "Default (recommended)", &["low", "high"])
                },
                ModelChoice {
                    resolved: Some("claude-sonnet-5-5".into()),
                    ..model("sonnet", "Sonnet", &["low", "high"])
                },
                model("claude-haiku-4-5", "Haiku 4.5", &[]),
            ];
            assert_eq!(t.model_label(), "Opus 5.5", "the default's model name, before a session says otherwise");
            t.set_model_choice(Some("sonnet".into()), cx);
            t.set_effort(Some("high".into()), cx);
            assert_eq!(t.model_label(), "Sonnet 5.5 · High");
            // A model without effort levels drops the effort.
            t.set_model_choice(Some("claude-haiku-4-5".into()), cx);
            assert_eq!(t.effort, None);
            assert_eq!(t.model_label(), "Haiku 4.5");
        });
        let sent: Vec<ScriptedCall> = calls(&thread, cx)
            .into_iter()
            .filter(|c| matches!(c, ScriptedCall::SetModel(_) | ScriptedCall::SetEffort(_)))
            .collect();
        assert_eq!(
            sent,
            vec![
                ScriptedCall::SetModel(Some("sonnet".into())),
                ScriptedCall::SetEffort(Some("high".into())),
                ScriptedCall::SetEffort(None),
                ScriptedCall::SetModel(Some("claude-haiku-4-5".into())),
            ]
        );
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
