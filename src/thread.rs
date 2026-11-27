//! A conversation with an agent: the transcript, approval cards, and the prompt box.

use std::{
    collections::HashSet,
    path::PathBuf,
    sync::{Arc, Mutex},
};

use gpui_kit::base::{h_flex, v_flex};
use gpui_kit::component::{
    ActiveTheme as _, Sizable as _, WindowExt as _,
    button::{Button, ButtonVariants as _},
    input::{Input, InputEvent, InputState, Textarea, TextareaState},
    menu::{DropdownMenu as _, PopupMenuItem},
    message_scroller::{MessageScroller, MessageScrollerState},
    text::{TextView, TextViewState},
};
use gpui_kit::{TestSupportExt as _, prelude::FluentBuilder as _, *};
use serde_json::Value;

use crate::agent::{
    AgentEvent, AgentSession, Decision, PermissionMode, PermissionRequest, ScriptedAgent, ScriptedCall,
    claude::{self, ClaudeSession, SessionOptions},
};
use crate::overview::{self, Overview};
use crate::store::{Record, TranscriptWriter};
use crate::tool_card::{self, ToolStatus};

mod questions;
use questions::QuestionCard;
pub use questions::bindings as question_bindings;

/// How often a long turn's overview is refreshed while it runs.
const OVERVIEW_EVERY: std::time::Duration = std::time::Duration::from_secs(60);

/// Tells the workspace something it saves or shows has changed (title, session, status, ...).
pub struct ThreadChanged;

impl EventEmitter<ThreadChanged> for ThreadView {}

/// The user sent the thread a message, so it moves to the top of its project.
pub struct ThreadSent;

impl EventEmitter<ThreadSent> for ThreadView {}

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
    /// Open a terminal with this command on its command line, not run (a reply's code block).
    RunInTerminal(String),
}

impl EventEmitter<ThreadAction> for ThreadView {}

/// The buttons in the corner of a reply's code block: Copy, and on a shell block, Run in
/// terminal, which opens a terminal with the command typed but not run.
fn code_block_buttons(block: &gpui_kit::base::text::CodeBlock, thread: WeakEntity<ThreadView>) -> AnyElement {
    use gpui_kit::{assets::IconName, component::clipboard::Clipboard};
    let shell = block.lang().is_some_and(|lang| is_shell(lang.as_ref()));
    let code = if shell { command_text(&block.code()) } else { block.code().to_string() };
    h_flex()
        .gap_0p5()
        .when(shell, |el| {
            let command = code.clone();
            el.child(
                Button::new("run")
                    .ghost()
                    .xsmall()
                    .icon(IconName::SquareTerminal)
                    .tooltip("Run in terminal (typed, not run: press Enter there)")
                    .on_click(move |_, _, cx| {
                        let action = ThreadAction::RunInTerminal(command.clone());
                        let _ = thread.update(cx, |_, cx| cx.emit(action));
                    }),
            )
        })
        .child(Clipboard::new("copy").value(code).tooltip("Copy"))
        .into_any_element()
}

/// A code block's language that is a shell's, so its contents are a command to run.
fn is_shell(lang: &str) -> bool {
    matches!(lang.to_ascii_lowercase().as_str(), "bash" | "sh" | "shell" | "zsh" | "fish" | "console" | "shell-session")
}

/// A shell block's command as the user would type it: without a trailing newline, and without
/// the `$ ` prompts some replies put in front of every line.
fn command_text(code: &str) -> String {
    let code = code.trim_end();
    let lines: Vec<&str> = code.lines().filter(|l| !l.trim().is_empty()).collect();
    if !lines.is_empty() && lines.iter().all(|l| l.starts_with("$ ")) {
        return code.lines().map(|l| l.strip_prefix("$ ").unwrap_or(l)).collect::<Vec<_>>().join("\n");
    }
    code.to_string()
}

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
    /// The slash commands `claude` offers in this project on the thread's account.
    pub commands: Vec<crate::agent::SlashCommand>,
}

/// The model menu's width; descriptions wrap inside it.
const MODEL_MENU_WIDTH: f32 = 400.0;

/// The widest the conversation and composer get, so lines stay readable in a wide window.
const MAX_CONVERSATION_WIDTH: f32 = 860.0;
/// The stop button's square.
const STOP_SIZE: f32 = 10.0;
/// How far (pixels) the crest of the spinner verb's wave lifts a letter.
const WAVE_LIFT: f32 = 2.5;

/// The spinner verb `steps` into a turn, as Claude Code shows it under the conversation, with
/// the wave lifting its letters one at a time.
fn spinner_line(seed: u64, steps: u64, color: Hsla) -> impl IntoElement {
    let text = format!("{}…", crate::working::verb(seed, steps));
    let letters = text.chars().count();
    h_flex().id("spinner-verb").test_support().text_sm().text_color(color).children(
        text.chars()
            .zip(crate::working::wave(steps, letters))
            .map(|(letter, lift)| div().relative().top(px(-WAVE_LIFT * lift)).child(letter.to_string())),
    )
}

/// Who the thread is and where it keeps its transcript.
#[derive(Clone, Default)]
pub struct ThreadSetup {
    pub title: Option<String>,
    /// The user named the thread; the overview keeps its own title to itself.
    pub named: bool,
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

struct ToolRow {
    id: String,
    name: String,
    /// The call's arguments, which the card's body lays out (the command, the edit, ...).
    input: Value,
    /// Pictures the tool returned, shown in the card.
    images: Vec<tool_card::Picture>,
    /// A Bash call's command and output, coloured once.
    shell: Option<tool_card::ShellText>,
    /// The card body's scroll position, and whether it overflows.
    scroll: ScrollHandle,
    summary: String,
    status: ToolStatus,
    output: Option<String>,
    expanded: bool,
    /// When the call began running (after its approval, if it needed one); none when replayed.
    started: Option<std::time::Instant>,
    /// How long the call ran.
    took: Option<std::time::Duration>,
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

// An approval row is several times larger than the others, but a transcript holds hundreds
// of rows at most, and boxing it would complicate every match on the cards.
#[allow(clippy::large_enum_variant)]
enum Row {
    /// What the user sent, and the images that went with it.
    User(Entity<TextViewState>, Vec<Arc<gpui_kit::Image>>),
    Assistant(Entity<TextViewState>),
    Tool(ToolRow),
    Approval(ApprovalRow),
    Notice(NoticeKind, SharedString),
    Footer(SharedString),
}

/// An image waiting in the prompt box for the next message.
struct Attachment {
    image: crate::agent::PromptImage,
    preview: Arc<gpui_kit::Image>,
}

/// A tool call added to the prompt box, so the next message asks about it.
struct Quote {
    tool_id: String,
    /// The chip: the tool's icon, its name and its one-line summary.
    icon: gpui_kit::assets::IconName,
    label: String,
    /// What the message carries (`tool_card::quote`).
    text: String,
}

/// The `@file` picker, open while a mention is being typed.
struct MentionPicker {
    matches: Vec<String>,
    selected: usize,
}

/// The `/command` picker, open while the prompt's first word is a slash and a name.
struct CommandPicker {
    matches: Vec<crate::agent::SlashCommand>,
    selected: usize,
    /// What's typed after the slash.
    query: String,
}

/// How many commands the picker shows at once (it scrolls through the rest).
const COMMANDS_SHOWN: usize = 50;

pub struct ThreadView {
    pub title: Option<String>,
    /// The user named the thread, so the overview's title doesn't replace it.
    pub named: bool,
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
    /// While the handoff summary is being written: the accounts it's between. Bumped
    /// `handoff_run` makes an older summary that arrives late be ignored.
    summarizing: Option<(String, String)>,
    handoff_run: u64,
    /// A message sent while the summary was still being written; it goes out when it's ready.
    queued: Option<(String, Vec<crate::agent::PromptImage>)>,
    /// Set by replay when the last account switch was never followed by a message.
    unsent_switch: Option<(String, String)>,
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
    /// Where attached images are saved, next to the transcript (none for demos and tests).
    files_dir: Option<PathBuf>,
    /// Images that go with the next message.
    attachments: Vec<Attachment>,
    /// Tool calls the next message asks about.
    quotes: Vec<Quote>,
    mention: Option<MentionPicker>,
    command_picker: Option<CommandPicker>,
    /// The messages sent here, for Up and Down in the prompt box.
    history: crate::composer::PromptHistory,
    command_scroll: ScrollHandle,
    /// The slash commands the running session said it accepts; until it has, the workspace's list.
    session_commands: Vec<crate::agent::SlashCommand>,
    /// The project's files, for the picker; listed again each time a mention starts.
    project_files: Option<Arc<Vec<String>>>,
    listing_files: bool,
    backend: Option<Backend>,
    agent: Option<Box<dyn AgentSession>>,
    _pump: Option<Task<()>>,
    /// Background work the running process started (shells, subagents, monitors). Stopping
    /// the process would end it, so a process with any is never stopped for being idle.
    background: HashSet<String>,
    /// When the process last did something or was sent something.
    last_active: std::time::Instant,
    /// The "newer than tested" warning was shown; a resumed process doesn't repeat it.
    version_warned: bool,
    /// The assistant text row that deltas go to.
    current_text: Option<usize>,
    pub status: Status,
    /// While the agent works: the animation step it started at, and the seed for its spinner
    /// verbs.
    working: Option<(u64, u64)>,
    pub mode: PermissionMode,
    pub session_id: Option<String>,
    pub model: Option<String>,
    /// What the finished turns cost.
    pub cost_usd: f64,
    /// What the turn under way has cost so far, priced from its tokens; its own cost replaces
    /// this when it finishes.
    pub cost_estimate: f64,
    /// While replaying an old transcript: Claude Code's running total at the last turn, to work
    /// each turn's cost out from.
    legacy_total: f64,
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
        cx.subscribe_in(&prompt, window, |this, _, event: &InputEvent, window, cx| match event {
            // Shift+Enter has already inserted a newline; only a plain or Ctrl+Enter sends.
            InputEvent::PressEnter { shift: false, .. } => this.submit(window, cx),
            InputEvent::Change => {
                // Typing over a recalled message makes it the draft. (Recalling doesn't
                // count: setting the value is not a change.)
                this.history.stop();
                this.update_mention(cx);
                this.update_command_picker(cx);
            }
            _ => {}
        })
        .detach();

        let records = setup.transcript.as_deref().map(crate::store::read_transcript).unwrap_or_default();
        let log = setup.transcript.as_deref().and_then(|path| TranscriptWriter::open(path).ok());
        let mut this = Self {
            title: setup.title,
            named: setup.named,
            account_id: setup.account_id,
            account_name: setup.account_name,
            rows: Vec::new(),
            plain: Vec::new(),
            plain_current: None,
            log: None,
            pending_text: String::new(),
            replaying: false,
            handoff: None,
            summarizing: None,
            handoff_run: 0,
            queued: None,
            unsent_switch: None,
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
            files_dir: setup.transcript.as_deref().map(crate::composer::files_dir),
            attachments: Vec::new(),
            quotes: Vec::new(),
            mention: None,
            command_picker: None,
            history: Default::default(),
            command_scroll: ScrollHandle::new(),
            session_commands: Vec::new(),
            project_files: None,
            listing_files: false,
            backend: Some(backend),
            agent: None,
            _pump: None,
            background: HashSet::new(),
            last_active: std::time::Instant::now(),
            version_warned: false,
            current_text: None,
            status: Status::Idle,
            working: None,
            mode,
            session_id: None,
            model: None,
            cost_usd: 0.0,
            cost_estimate: 0.0,
            legacy_total: 0.0,
            scripted_log: None,
        };
        this.replay(records, cx);
        // The saved session id wins over one seen in the transcript (an account switch clears it).
        this.session_id = setup.session_id;
        this.log = log;
        // Switched accounts but never sent a message: prepare that handoff again.
        if let Some((from, to)) = this.unsent_switch.take()
            && this.session_id.is_none()
        {
            this.start_handoff(&from, &to, cx);
        }
        this
    }

    /// Rebuild the transcript from saved records, without writing them again.
    fn replay(&mut self, records: Vec<Record>, cx: &mut Context<Self>) {
        self.replaying = true;
        for record in records {
            match record {
                Record::Prompt { text, images } => {
                    self.unsent_switch = None;
                    let previews = images.iter().filter_map(|name| self.load_saved_image(name)).collect();
                    self.push_prompt_row(&text, previews, cx)
                }
                Record::Event { event } => self.handle(event, cx),
                Record::Decision { request_id, outcome } => {
                    if let Some(Row::Approval(row)) =
                        self.rows.iter_mut().find(|r| matches!(r, Row::Approval(a) if a.request.id == request_id))
                    {
                        row.outcome = Some(outcome);
                    }
                }
                Record::AccountSwitch { from, to } => {
                    self.notice(NoticeKind::Info, format!("Switched from {from} to {to}."), cx);
                    self.unsent_switch = Some((from, to));
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
        // A turn that never finished was still spent.
        self.cost_usd += std::mem::take(&mut self.cost_estimate);
        self.replaying = false;
    }

    /// What the thread has cost, the turn under way included.
    pub fn cost(&self) -> f64 {
        self.cost_usd + self.cost_estimate
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
            self.agent = None;
            self._pump = None;
            self.session_id = None;
            self.status = Status::Idle;
            self.write(Record::AccountSwitch { from: from.clone(), to: name.to_string() });
            let summarizing = self.start_handoff(&from, name, cx);
            let context = if summarizing { "a summary of this conversation" } else { "this conversation so far" };
            self.notice(
                NoticeKind::Info,
                format!("Switched from {from} to {name}. Your next message starts a new Claude session on {name}, with {context} as context."),
                cx,
            );
        }
        cx.emit(ThreadChanged);
        cx.notify();
    }

    /// Prepare what the new session is told about the conversation so far. Short ones go word
    /// for word at once; longer ones are summarized in the background, on the new account
    /// (true). Scripted threads (demos, tests) always go word for word.
    fn start_handoff(&mut self, from: &str, to: &str, cx: &mut Context<Self>) -> bool {
        self.handoff_run += 1;
        let options = match &self.backend {
            Some(Backend::Claude(options)) if crate::handoff::needs_summary(&self.plain) => options.clone(),
            _ => {
                self.summarizing = None;
                self.handoff = Some(crate::handoff::verbatim(from, to, &self.plain));
                return false;
            }
        };
        self.handoff = None;
        self.summarizing = Some((from.to_string(), to.to_string()));
        let (older, _) = crate::handoff::split_recent(&self.plain);
        let (older, overview, run) = (older.to_vec(), self.overview.clone(), self.handoff_run);
        cx.spawn(async move |this, cx| {
            let summary = cx
                .background_executor()
                .spawn(async move {
                    crate::handoff::summarize(&options.cwd, options.config_dir.as_deref(), &overview, &older)
                })
                .await;
            let _ = this.update(cx, |t: &mut ThreadView, cx| t.finish_handoff(run, summary, cx));
        })
        .detach();
        true
    }

    /// The summary is in (or failed: then the conversation goes word for word). A message
    /// that was waiting for it goes out now.
    fn finish_handoff(&mut self, run: u64, summary: anyhow::Result<String>, cx: &mut Context<Self>) {
        if run != self.handoff_run {
            return;
        }
        let Some((from, to)) = self.summarizing.take() else { return };
        self.handoff = Some(match summary {
            Ok(summary) => {
                let (_, recent) = crate::handoff::split_recent(&self.plain);
                // The waiting message is already in `plain`; it goes after the context, not in it.
                let recent = match &self.queued {
                    Some(_) => &recent[..recent.len().saturating_sub(1)],
                    None => recent,
                };
                crate::handoff::with_summary(&from, &to, &summary, &self.overview, recent)
            }
            Err(err) => {
                eprintln!("tj: handoff summary failed, sending the conversation instead: {err:#}");
                let plain = match &self.queued {
                    Some(_) => &self.plain[..self.plain.len().saturating_sub(1)],
                    None => &self.plain[..],
                };
                crate::handoff::verbatim(&from, &to, plain)
            }
        });
        if let Some((text, images)) = self.queued.take() {
            self.dispatch(&text, images, cx);
        }
        cx.notify();
    }

    /// Give the thread the user's own title; an empty one hands naming back to the overview
    /// (and to the first message, until the overview has one).
    pub fn rename(&mut self, title: &str, cx: &mut Context<Self>) {
        let title = title.trim();
        self.named = !title.is_empty();
        self.title = if self.named {
            Some(title.to_string())
        } else {
            self.overview
                .title
                .clone()
                .or_else(|| self.plain.first().and_then(|m| m.strip_prefix("User: ")).map(title_from))
        };
        cx.emit(ThreadChanged);
        cx.notify();
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
        let name = self.model_name().unwrap_or_else(|| "Default model".into());
        match &self.effort {
            Some(effort) => format!("{name} · {}", capitalize(effort)),
            None => name,
        }
    }

    /// The model's short name ("Opus 5.5"): the one a session runs, else the one the picked
    /// entry resolves to, else the entry's name.
    pub fn model_name(&self) -> Option<String> {
        let info = self.model_info();
        let resolved = info.and_then(|m| m.resolved.as_deref()).map(short_model);
        match &self.model_choice {
            // A session reports the model that really answered; after a switch it catches up.
            None => self.model.as_deref().map(short_model).or(resolved),
            Some(value) => resolved.or_else(|| info.map(|m| m.name.clone())).or_else(|| Some(value.clone())),
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

    /// Send what's in the prompt box, after any tool calls it asks about and with any
    /// attached images.
    pub fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let typed = self.prompt.read(cx).value().trim().to_string();
        if typed.is_empty() && self.attachments.is_empty() && self.quotes.is_empty() {
            return;
        }
        let mut parts: Vec<String> = self.quotes.drain(..).map(|quote| quote.text).collect();
        parts.extend((!typed.is_empty()).then_some(typed));
        let text = parts.join("\n\n");
        let attachments = std::mem::take(&mut self.attachments);
        self.mention = None;
        self.command_picker = None;
        self.prompt.update(cx, |p, cx| p.set_value("", window, cx));
        self.send_with(&text, attachments, cx);
    }

    /// Send `text` as the next user message.
    pub fn send(&mut self, text: &str, cx: &mut Context<Self>) {
        self.send_with(text, Vec::new(), cx);
    }

    fn send_with(&mut self, text: &str, attachments: Vec<Attachment>, cx: &mut Context<Self>) {
        let saved = self.save_images(&attachments);
        let (images, previews): (Vec<_>, Vec<_>) = attachments.into_iter().map(|a| (a.image, a.preview)).unzip();
        self.push_prompt_row(text, previews, cx);
        self.write(Record::Prompt { text: text.to_string(), images: saved });
        cx.emit(ThreadSent);
        if self.title.is_none() {
            self.title = Some(if text.is_empty() { "Image".into() } else { title_from(text) });
        }
        self.dispatch(text, images, cx);
    }

    /// Hand the message to the agent, after the handoff context if there is one. While the
    /// handoff summary is still being written, the message waits for it.
    fn dispatch(&mut self, text: &str, images: Vec<crate::agent::PromptImage>, cx: &mut Context<Self>) {
        if self.summarizing.is_some() {
            self.queued = Some((text.to_string(), images));
            self.status = Status::Running;
            cx.emit(ThreadChanged);
            cx.notify();
            return;
        }
        let message = match &self.handoff {
            Some(context) => format!("{context}\n\n{text}"),
            None => text.to_string(),
        };
        if let Err(err) = self.ensure_agent(cx).and_then(|agent| agent.send_message(&message, &images)) {
            self.notice(NoticeKind::Error, format!("Could not send: {err:#}"), cx);
            self.status = Status::Idle;
            cx.emit(ThreadChanged);
            cx.notify();
            return;
        }
        self.handoff = None;
        self.status = Status::Running;
        self.last_active = std::time::Instant::now();
        self.start_overview_ticker(cx);
        cx.emit(ThreadChanged);
        cx.notify();
    }

    // ---------- attachments and mentions ----------

    /// Where the agent runs, which is where mentions are relative to.
    fn project_dir(&self) -> PathBuf {
        match &self.backend {
            Some(Backend::Claude(options)) => options.cwd.clone(),
            _ => default_cwd(),
        }
    }

    /// Open, update or close the `@file` picker after the prompt text changed.
    fn update_mention(&mut self, cx: &mut Context<Self>) {
        // List the files as soon as anything is typed, so they're ready when an @ comes.
        if self.project_files.is_none() && !self.listing_files {
            self.list_project_files(cx);
        }
        let Some((_, query)) = self.mention_query(cx) else {
            if self.mention.take().is_some() {
                cx.notify();
            }
            return;
        };
        if self.mention.is_none() && !self.listing_files {
            self.list_project_files(cx);
        }
        let matches = self
            .project_files
            .as_ref()
            .map(|files| crate::composer::rank(files, &query, crate::composer::SHOWN))
            .unwrap_or_default();
        self.mention = Some(MentionPicker { matches, selected: 0 });
        cx.notify();
    }

    /// The `@query` before the cursor, if one is being typed.
    fn mention_query(&self, cx: &App) -> Option<(std::ops::Range<usize>, String)> {
        let prompt = self.prompt.read(cx);
        let tokens: Vec<_> = prompt.tokens().iter().map(|t| t.range()).collect();
        crate::composer::query_at(&prompt.value(), prompt.cursor(), &tokens)
    }

    /// List the project's files in the background, then refresh the picker's matches.
    fn list_project_files(&mut self, cx: &mut Context<Self>) {
        let dir = self.project_dir();
        self.listing_files = true;
        cx.spawn(async move |this, cx| {
            let files = cx.background_executor().spawn(async move { crate::composer::list_files(&dir) }).await;
            let _ = this.update(cx, |this: &mut ThreadView, cx| {
                this.listing_files = false;
                this.project_files = Some(Arc::new(files));
                if this.mention.is_some() {
                    this.update_mention(cx);
                }
            });
        })
        .detach();
    }

    fn move_mention(&mut self, by: isize, cx: &mut Context<Self>) -> bool {
        let Some(picker) = &mut self.mention else { return false };
        if picker.matches.is_empty() {
            return false;
        }
        let len = picker.matches.len() as isize;
        picker.selected = (picker.selected as isize + by).rem_euclid(len) as usize;
        cx.notify();
        true
    }

    // ---------- slash commands ----------

    /// The commands this thread can run: the live session's, else the workspace's list.
    fn commands(&self) -> &[crate::agent::SlashCommand] {
        if self.session_commands.is_empty() { &self.menus.commands } else { &self.session_commands }
    }

    /// Open, update or close the `/command` picker after the prompt text changed.
    fn update_command_picker(&mut self, cx: &mut Context<Self>) {
        let query = {
            let prompt = self.prompt.read(cx);
            crate::composer::command_query(&prompt.value(), prompt.cursor()).map(|(_, query)| query)
        };
        let Some(query) = query else {
            if self.command_picker.take().is_some() {
                cx.notify();
            }
            return;
        };
        // Typing re-ranks the list, so the highlight goes back to the best match.
        let matches = crate::composer::rank_commands(self.commands(), &query, COMMANDS_SHOWN);
        self.command_picker = Some(CommandPicker { matches, selected: 0, query });
        cx.notify();
    }

    fn move_command(&mut self, by: isize, cx: &mut Context<Self>) -> bool {
        let Some(picker) = &mut self.command_picker else { return false };
        if picker.matches.is_empty() {
            return false;
        }
        let len = picker.matches.len() as isize;
        picker.selected = (picker.selected as isize + by).rem_euclid(len) as usize;
        cx.notify();
        true
    }

    /// Put the picked command in place of the first word, then a space for its arguments.
    /// False when there was nothing to pick, or (with `send_if_typed`, for Enter) when the
    /// command is already typed out in full, so Enter sends it.
    fn accept_command(&mut self, send_if_typed: bool, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(picker) = &self.command_picker else { return false };
        let Some(command) = picker.matches.get(picker.selected).cloned() else { return false };
        if send_if_typed && picker.query.eq_ignore_ascii_case(&command.name) {
            self.command_picker = None;
            return false;
        }
        let range = {
            let prompt = self.prompt.read(cx);
            crate::composer::command_query(&prompt.value(), prompt.cursor()).map(|(range, _)| range)
        };
        let Some(range) = range else {
            self.command_picker = None;
            return false;
        };
        let name = format!("/{}", command.name);
        self.prompt.update(cx, |prompt, cx| {
            // Replacing just the word keeps the rest, mentions included, and the undo history.
            let spaced = prompt.value()[range.end..].starts_with(char::is_whitespace);
            prompt.set_selected_range(range, cx);
            prompt.replace(if spaced { name.clone() } else { format!("{name} ") }, window, cx);
            let after = name.len() + 1;
            prompt.set_selected_range(after..after, cx);
        });
        self.command_picker = None;
        cx.notify();
        true
    }

    /// Put the picked file in place of the `@query`. False when there was nothing to pick.
    /// While the files are still being listed (the first mention), the key is held (true):
    /// sending a half-typed mention would be the wrong guess.
    fn accept_mention(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.mention.is_some() && self.project_files.is_none() {
            return true;
        }
        let Some(path) = self.mention.as_ref().and_then(|p| p.matches.get(p.selected).cloned()) else {
            return false;
        };
        let Some((range, _)) = self.mention_query(cx) else {
            self.mention = None;
            return false;
        };
        self.insert_mention(Some(range), &path, window, cx);
        self.mention = None;
        cx.notify();
        true
    }

    /// Insert `@path` as one token (shown as a chip, deleted as one), then a space. Replaces
    /// `range`, or the selection.
    fn insert_mention(
        &mut self,
        range: Option<std::ops::Range<usize>>,
        path: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let text = crate::composer::mention_text(path);
        let token =
            gpui_kit::component::input::InlineToken::new(path.to_string(), text.clone()).with_label(path.to_string());
        self.prompt.update(cx, |prompt, cx| {
            let placed = match range {
                Some(range) => prompt.replace_range_with_token(range, token, window, cx),
                None => prompt.replace_with_token(token, window, cx),
            };
            if placed.is_err() {
                prompt.insert(text, window, cx);
            }
            prompt.insert(" ", window, cx);
        });
    }

    /// Ask for files to attach (images) or mention (anything else).
    fn pick_files(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let picked = cx.prompt_for_paths(gpui_kit::PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some("Attach".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(paths))) = picked.await {
                let _ = this.update_in(cx, |this, window, cx| this.add_files(paths, window, cx));
            }
        })
        .detach();
    }

    /// Dropped or picked files: images are attached, other files are mentioned.
    fn add_files(&mut self, paths: Vec<PathBuf>, window: &mut Window, cx: &mut Context<Self>) {
        let project = self.project_dir();
        for path in paths {
            if crate::composer::is_image(&path) {
                match std::fs::read(&path) {
                    Ok(bytes) => self.attach_image(bytes, cx),
                    Err(err) => self.notice(NoticeKind::Error, format!("Could not read {}: {err}", path.display()), cx),
                }
            } else {
                let mention = crate::composer::mention_path(&path, &project);
                self.insert_mention(None, &mention, window, cx);
            }
        }
        self.prompt.update(cx, |p, cx| p.focus(window, cx));
    }

    /// Prepare an image in the background (large ones are scaled down), then attach it.
    fn attach_image(&mut self, bytes: Vec<u8>, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let prepared = cx.background_executor().spawn(async move { crate::composer::prepare_image(bytes) }).await;
            let _ = this.update(cx, |this: &mut ThreadView, cx| match prepared {
                Ok(image) => {
                    let preview =
                        Arc::new(gpui_kit::Image::from_bytes(image_format(&image.media_type), image.data.to_vec()));
                    this.attachments.push(Attachment { image, preview });
                    cx.notify();
                }
                Err(err) => this.notice(NoticeKind::Error, format!("Could not attach the image: {err:#}"), cx),
            });
        })
        .detach();
    }

    /// Ctrl+V with an image on the clipboard attaches it. The text box ignores clipboards
    /// without text, so this only has to take the images. On Wayland the clipboard is read
    /// asynchronously, so the text box pastes any text by itself, and this attaches any
    /// images once they arrive.
    fn paste_images(&mut self, cx: &mut Context<Self>) -> bool {
        fn images(item: &gpui_kit::ClipboardItem) -> Vec<Vec<u8>> {
            if item.text().is_some() {
                return Vec::new();
            }
            item.entries()
                .iter()
                .filter_map(|e| match e {
                    gpui_kit::ClipboardEntry::Image(image) => Some(image.bytes().to_vec()),
                    _ => None,
                })
                .collect()
        }
        if let Some(item) = cx.read_from_clipboard() {
            let found = images(&item);
            let any = !found.is_empty();
            for bytes in found {
                self.attach_image(bytes, cx);
            }
            return any;
        }
        let read = cx.read_from_clipboard_async();
        cx.spawn(async move |this, cx| {
            if let Ok(Some(item)) = read.await {
                let _ = this.update(cx, |this: &mut ThreadView, cx| {
                    for bytes in images(&item) {
                        this.attach_image(bytes, cx);
                    }
                });
            }
        })
        .detach();
        false
    }

    fn remove_attachment(&mut self, ix: usize, cx: &mut Context<Self>) {
        if ix < self.attachments.len() {
            self.attachments.remove(ix);
            cx.notify();
        }
    }

    /// Add the tool call in row `ix` to the prompt box (once), and put the keyboard there.
    fn quote_tool(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Row::Tool(tool)) = self.rows.get(ix) else { return };
        if !self.quotes.iter().any(|quote| quote.tool_id == tool.id) {
            let summary = tool_card::header_summary(&tool.name, &tool.input, &tool.summary, &self.project_dir());
            self.quotes.push(Quote {
                tool_id: tool.id.clone(),
                icon: tool_card::kind(&tool.name).1,
                label: format!("{} · {}", tool_card::title(&tool.name), truncate(&summary, 60)),
                text: tool_card::quote(&tool.name, &tool.input, &summary),
            });
        }
        self.prompt.update(cx, |p, cx| p.focus(window, cx));
        cx.notify();
    }

    fn remove_quote(&mut self, ix: usize, cx: &mut Context<Self>) {
        if ix < self.quotes.len() {
            self.quotes.remove(ix);
            cx.notify();
        }
    }

    /// Save attached images next to the transcript, so a reopened thread still shows them.
    /// Gives the saved file names (none when the thread isn't saved).
    fn save_images(&self, attachments: &[Attachment]) -> Vec<String> {
        let Some(dir) = &self.files_dir else { return Vec::new() };
        if attachments.is_empty() || std::fs::create_dir_all(dir).is_err() {
            return Vec::new();
        }
        attachments
            .iter()
            .filter_map(|a| {
                let name = format!("{}.{}", uuid::Uuid::new_v4(), crate::composer::extension(&a.image.media_type));
                std::fs::write(dir.join(&name), a.image.data.as_slice()).ok().map(|_| name)
            })
            .collect()
    }

    /// Save the pictures a tool returned beside the transcript, keeping only their file names
    /// in the event. Without a files folder (demos, tests) they stay in the event.
    fn save_tool_images(&self, images: &mut [crate::agent::ToolImage]) {
        use base64::Engine as _;
        let Some(dir) = &self.files_dir else { return };
        for image in images.iter_mut().filter(|i| i.file.is_none() && !i.data.is_empty()) {
            let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(&image.data) else { continue };
            if std::fs::create_dir_all(dir).is_err() {
                return;
            }
            let name = format!("{}.{}", uuid::Uuid::new_v4(), crate::composer::extension(&image.media_type));
            if std::fs::write(dir.join(&name), bytes).is_ok() {
                image.file = Some(name);
                image.data.clear();
            }
        }
    }

    /// A tool's picture, from its saved file or the bytes still in the event.
    fn tool_image(&self, image: &crate::agent::ToolImage) -> Option<tool_card::Picture> {
        use base64::Engine as _;
        let bytes = match &image.file {
            Some(name) => std::fs::read(self.files_dir.as_ref()?.join(name)).ok()?,
            None => base64::engine::general_purpose::STANDARD.decode(&image.data).ok()?,
        };
        tool_card::picture(&image.media_type, bytes)
    }

    fn load_saved_image(&self, name: &str) -> Option<Arc<gpui_kit::Image>> {
        let bytes = std::fs::read(self.files_dir.as_ref()?.join(name)).ok()?;
        let format = image_format(&format!("image/{}", name.rsplit('.').next().unwrap_or("png")));
        Some(Arc::new(gpui_kit::Image::from_bytes(format, bytes)))
    }

    /// The prompt box, with picked mentions shown as chips.
    fn prompt_box(&self, appearance: bool, cx: &App) -> Textarea {
        // A tint of the primary colour: the accent can match the card in light themes.
        let (chip, chip_fg) = (cx.theme().primary.opacity(0.16), cx.theme().foreground);
        Textarea::new(&self.prompt).appearance(appearance).token(move |token, _, _| {
            h_flex()
                .gap_1()
                .px_1()
                .rounded_sm()
                .bg(chip)
                .text_color(chip_fg)
                .child(gpui_kit::component::Icon::new(gpui_kit::assets::IconName::File).xsmall())
                .child(token.token().label().clone())
        })
    }

    /// The composer's outer box: it takes the picker's keys while it's open, pasted images,
    /// and dropped files, and shows the picker above itself.
    fn composer_frame(&self, cx: &mut Context<Self>) -> Div {
        use gpui_kit::component::input::{Enter, Escape, IndentInline, MoveDown, MoveUp, Paste};
        let primary = cx.theme().primary;
        v_flex()
            .relative()
            .capture_action(cx.listener(|this, _: &MoveUp, window, cx| {
                if this.move_mention(-1, cx) || this.move_command(-1, cx) {
                    cx.stop_propagation();
                } else {
                    this.recall_when_stuck(true, window, cx);
                }
            }))
            .capture_action(cx.listener(|this, _: &MoveDown, window, cx| {
                if this.move_mention(1, cx) || this.move_command(1, cx) {
                    cx.stop_propagation();
                } else {
                    this.recall_when_stuck(false, window, cx);
                }
            }))
            // Enter picks the highlighted mention or command; a command already typed in full
            // is sent instead.
            .capture_action(cx.listener(|this, enter: &Enter, window, cx| {
                if !enter.shift && (this.accept_mention(window, cx) || this.accept_command(true, window, cx)) {
                    cx.stop_propagation();
                }
            }))
            // Tab picks a mention or command too; otherwise it stays in the prompt box (it
            // never moves focus).
            .capture_action(cx.listener(|this, _: &IndentInline, window, cx| {
                if this.accept_mention(window, cx) || this.accept_command(false, window, cx) {
                    cx.stop_propagation();
                }
            }))
            // Escape closes a picker; with none open it stops the agent, as in Claude Code.
            .capture_action(cx.listener(|this, _: &Escape, _, cx| {
                if this.mention.take().is_some() || this.command_picker.take().is_some() {
                    cx.notify();
                    cx.stop_propagation();
                } else if this.status != Status::Idle {
                    this.interrupt(cx);
                    cx.stop_propagation();
                }
            }))
            .capture_action(cx.listener(|this, _: &Paste, _, cx| {
                if this.paste_images(cx) {
                    cx.stop_propagation();
                }
            }))
            .on_drop(cx.listener(|this, paths: &gpui_kit::ExternalPaths, window, cx| {
                this.add_files(paths.paths().to_vec(), window, cx)
            }))
            .drag_over::<gpui_kit::ExternalPaths>(move |style, _, _, _| style.border_color(primary))
            .children(self.render_mention_picker(cx))
            .children(self.render_command_picker(cx))
    }

    /// Up on the prompt's top row recalls the message sent before, Down on its bottom row the
    /// one after (or what was being typed), as in Claude Code. The prompt box moves the caret
    /// first, wrapped lines included; when it didn't move, it was on that row. A recalled
    /// message starts with the caret at its start going up and at its end going down, so the
    /// next press keeps walking.
    fn recall_when_stuck(&mut self, older: bool, window: &mut Window, cx: &mut Context<Self>) {
        let caret = self.prompt.read(cx).selected_range();
        if !caret.is_empty() {
            return;
        }
        cx.defer_in(window, move |this, window, cx| {
            let prompt = this.prompt.read(cx);
            if prompt.selected_range() != caret {
                return;
            }
            let recalled = if older { this.history.older(&prompt.value()) } else { this.history.newer() };
            let Some(text) = recalled else { return };
            this.prompt.update(cx, |prompt, cx| {
                prompt.set_value(text.as_str(), window, cx);
                if !older {
                    prompt.set_selected_range(text.len()..text.len(), cx);
                }
            });
            // A recalled `/command` or `@mention` doesn't open its picker: the next Up or
            // Down keeps walking the history.
            this.mention = None;
            this.command_picker = None;
            cx.notify();
        });
    }

    /// What goes with the next message, each with a remove button: the tool calls it asks
    /// about as chips, and thumbnails of its images.
    fn render_attachments(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.attachments.is_empty() && self.quotes.is_empty() {
            return None;
        }
        use gpui_kit::{assets::IconName, component::Icon};
        let (border, bg) = (cx.theme().border, cx.theme().background);
        let (chip, muted) = (cx.theme().primary.opacity(0.16), cx.theme().muted_foreground);
        Some(
            h_flex()
                .flex_wrap()
                .items_end()
                .gap_2()
                .children(self.quotes.iter().enumerate().map(|(ix, quote)| {
                    h_flex()
                        .gap_1p5()
                        .max_w_full()
                        .pl_2()
                        .pr_1()
                        .py_0p5()
                        .rounded_md()
                        .bg(chip)
                        .text_sm()
                        .child(Icon::new(quote.icon).xsmall())
                        .child(
                            div()
                                .min_w_0()
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_ellipsis()
                                .child(quote.label.clone()),
                        )
                        .child(
                            div()
                                .id(("remove-quote", ix))
                                .test_support()
                                .flex_shrink_0()
                                .rounded_sm()
                                .p_0p5()
                                .cursor_pointer()
                                .hover(move |el| el.bg(muted.opacity(0.2)))
                                .child(Icon::new(IconName::X).xsmall().text_color(muted))
                                .on_click(cx.listener(move |this, _, _, cx| this.remove_quote(ix, cx))),
                        )
                }))
                .children(self.attachments.iter().enumerate().map(|(ix, a)| {
                    div()
                        .relative()
                        .size_16()
                        .rounded_md()
                        .overflow_hidden()
                        .border_1()
                        .border_color(border)
                        .child(gpui_kit::img(a.preview.clone()).size_full().object_fit(gpui_kit::ObjectFit::Cover))
                        .child(
                            div()
                                .id(("remove-attachment", ix))
                                .test_support()
                                .absolute()
                                .top_0p5()
                                .right_0p5()
                                .size_4()
                                .rounded_full()
                                .bg(bg)
                                .flex()
                                .items_center()
                                .justify_center()
                                .cursor_pointer()
                                .child(gpui_kit::component::Icon::new(gpui_kit::assets::IconName::X).xsmall())
                                .on_click(cx.listener(move |this, _, _, cx| this.remove_attachment(ix, cx))),
                        )
                }))
                .into_any_element(),
        )
    }

    /// The `@file` picker, floating just above the composer.
    fn render_mention_picker(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let picker = self.mention.as_ref()?;
        let theme = cx.theme();
        let (popover, border, muted, selected_bg) = (theme.popover, theme.border, theme.muted_foreground, theme.accent);
        let empty = match &self.project_files {
            None => Some("Listing files…"),
            Some(_) if picker.matches.is_empty() => Some("No matching files"),
            Some(_) => None,
        };
        let list = v_flex()
            .absolute()
            .bottom_full()
            .left_0()
            .right_0()
            .mb_1()
            .p_1()
            .rounded_lg()
            .border_1()
            .border_color(border)
            .bg(popover)
            .shadow_md()
            .when_some(empty, |el, text| el.child(div().px_2().py_1().text_sm().text_color(muted).child(text)))
            .children(picker.matches.iter().enumerate().map(|(ix, path)| {
                let (dir, name) = match path.rsplit_once('/') {
                    Some((dir, name)) => (dir.to_string(), name.to_string()),
                    None => (String::new(), path.clone()),
                };
                h_flex()
                    .id(("mention", ix))
                    .test_support()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .text_sm()
                    .cursor_pointer()
                    .when(ix == picker.selected, |el| el.bg(selected_bg))
                    .child(gpui_kit::component::Icon::new(gpui_kit::assets::IconName::File).xsmall())
                    .child(name)
                    .child(div().text_xs().text_color(muted).truncate().child(dir))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        if let Some(picker) = &mut this.mention {
                            picker.selected = ix;
                        }
                        this.accept_mention(window, cx);
                    }))
            }));
        Some(gpui_kit::deferred(list).with_priority(1).into_any_element())
    }

    /// The `/command` picker, floating just above the composer: each command's name, what it
    /// takes, and its description on one line.
    fn render_command_picker(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let picker = self.command_picker.as_ref()?;
        let theme = cx.theme();
        let (popover, border, muted, selected_bg) = (theme.popover, theme.border, theme.muted_foreground, theme.accent);
        let mono = theme.mono_font_family.clone();
        let empty = match self.commands().is_empty() {
            true => Some("Claude hasn't listed its commands yet. They show once it has started."),
            false if picker.matches.is_empty() => Some("No matching commands. Enter sends it as typed."),
            false => None,
        };
        let list = v_flex()
            .id("command-picker")
            .absolute()
            .bottom_full()
            .left_0()
            .right_0()
            .mb_1()
            .p_1()
            .max_h(px(320.))
            .overflow_y_scroll()
            .track_scroll(&self.command_scroll)
            .rounded_lg()
            .border_1()
            .border_color(border)
            .bg(popover)
            .shadow_md()
            .when_some(empty, |el, text| el.child(div().px_2().py_1().text_sm().text_color(muted).child(text)))
            .children(picker.matches.iter().enumerate().map(|(ix, command)| {
                h_flex()
                    .id(("command", ix))
                    .test_support()
                    .flex_shrink_0()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .text_sm()
                    .cursor_pointer()
                    .when(ix == picker.selected, |el| el.bg(selected_bg))
                    .child(div().flex_shrink_0().font_family(mono.clone()).child(format!("/{}", command.name)))
                    .when(!command.argument_hint.is_empty(), |el| {
                        el.child(div().flex_shrink_0().text_xs().text_color(muted).child(command.argument_hint.clone()))
                    })
                    .child(
                        div()
                            .min_w_0()
                            .flex_1()
                            .text_xs()
                            .text_color(muted)
                            .truncate()
                            .child(command.description.clone()),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        if let Some(picker) = &mut this.command_picker {
                            picker.selected = ix;
                        }
                        this.accept_command(false, window, cx);
                    }))
            }));
        self.command_scroll.scroll_to_item(picker.selected);
        Some(gpui_kit::deferred(list).with_priority(1).into_any_element())
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
        if let Some(title) = overview.title.as_ref().filter(|_| !self.named) {
            self.title = Some(title.clone());
        }
        self.overview = overview.clone();
        self.overview_mark = mark;
        self.write(Record::Overview { overview, mark });
        cx.emit(ThreadChanged);
        cx.notify();
    }

    fn push_prompt_row(&mut self, text: &str, images: Vec<Arc<gpui_kit::Image>>, cx: &mut Context<Self>) {
        let count = images.len();
        let body = cx.new(|cx| TextViewState::markdown(text, cx).selectable(true));
        self.push_row(Row::User(body, images), cx);
        self.history.push(text);
        self.plain.push(match count {
            0 => format!("User: {text}"),
            n => format!("User: {text} [{n} image{}]", if n == 1 { "" } else { "s" }),
        });
        self.plain_current = None;
        self.current_text = None;
    }

    pub fn interrupt(&mut self, cx: &mut Context<Self>) {
        // Stop a message that is still waiting for the handoff summary: it is not sent.
        if self.queued.take().is_some() {
            self.status = Status::Idle;
            self.notice(NoticeKind::Info, "Stopped. That message was not sent.", cx);
            cx.emit(ThreadChanged);
            return;
        }
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
        // The call starts running now; the wait for this answer isn't part of how long it took.
        if let Some(tool_ix) = request.tool_use_id.as_deref().and_then(|id| self.tool_row(id))
            && let Row::Tool(tool) = &mut self.rows[tool_ix]
        {
            tool.started = tool.started.map(|_| std::time::Instant::now());
        }
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

    /// Open a Bash card in a dialog, where all of its command and output can be read.
    fn expand_tool(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Row::Tool(tool)) = self.rows.get(ix) else { return };
        let Some(shell) = tool.shell.clone() else { return };
        let took = tool.took;
        let summary = tool_card::header_summary(&tool.name, &tool.input, &tool.summary, &self.project_dir());
        window.open_dialog(cx, move |dialog, window, cx| {
            tool_card::shell_dialog(dialog, &summary, &shell, took, window, cx)
        });
    }

    fn fold_tool(&mut self, ix: usize, cx: &mut Context<Self>) {
        if let Some(Row::Tool(tool)) = self.rows.get_mut(ix) {
            tool.expanded = !tool.expanded;
        }
        self.remeasure(ix, cx);
    }

    fn tool_row(&self, tool_use_id: &str) -> Option<usize> {
        self.rows.iter().rposition(|r| matches!(r, Row::Tool(t) if t.id == tool_use_id))
    }

    /// The Agent (or Task) call `tool_use_id`, which started a subagent. Claude Code reports a
    /// long Bash command as a task too, with the same events and its description as the
    /// "report"; those are left out, so the card keeps its command and output.
    fn subagent_row(&self, tool_use_id: &str) -> Option<usize> {
        self.tool_row(tool_use_id)
            .filter(|&ix| matches!(&self.rows[ix], Row::Tool(t) if matches!(t.name.as_str(), "Agent" | "Task")))
    }

    /// Update the subagent started by tool call `tool_use_id` (creating it if events arrive
    /// before its start was seen).
    fn with_subagent(&mut self, tool_use_id: &str, cx: &mut Context<Self>, update: impl FnOnce(&mut Subagent)) {
        let Some(ix) = self.subagent_row(tool_use_id) else { return };
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

    /// Note work the process keeps doing after its turn ends.
    fn track_background(&mut self, event: &AgentEvent) {
        match event {
            AgentEvent::ToolStarted { id, name, input } => {
                if name == "Monitor" || input.get("run_in_background").and_then(Value::as_bool) == Some(true) {
                    self.background.insert(id.clone());
                }
            }
            // A background subagent reports when it's done; a background shell doesn't, so its
            // process stays until TJ quits.
            AgentEvent::SubagentFinished { tool_use_id, .. } => {
                self.background.remove(tool_use_id);
            }
            AgentEvent::Exited { .. } => self.background.clear(),
            _ => {}
        }
    }

    /// How long the Claude process has had nothing to do, when stopping it now loses nothing:
    /// the turn is over, nothing waits on the user or runs in the background. `None` when there
    /// is no process to stop.
    pub fn idle_for(&self) -> Option<std::time::Duration> {
        let stoppable = self.agent.is_some()
            && matches!(self.backend, Some(Backend::Claude(_)))
            && self.status == Status::Idle
            && self.background.is_empty()
            && self.queued.is_none()
            && self.summarizing.is_none()
            && !self.has_pending_approval();
        stoppable.then(|| self.last_active.elapsed())
    }

    /// Stop the idle Claude process to free its memory. The conversation stays on screen, and
    /// the next message resumes the session in a new process with the thread's current model,
    /// effort, mode and account.
    pub fn stop_idle_process(&mut self) {
        if self.idle_for().is_none() {
            return;
        }
        // Dropping the session kills the process; with the pump gone its exit isn't reported.
        self.agent = None;
        self._pump = None;
        self.current_text = None;
    }

    #[cfg(test)]
    pub fn has_agent(&self) -> bool {
        self.agent.is_some()
    }

    #[cfg(test)]
    /// Use a scripted agent that is already running (tests).
    pub fn attach_scripted(&mut self, agent: ScriptedAgent) {
        self.scripted_log = Some(agent.log.clone());
        self.agent = Some(Box::new(agent));
    }

    // ---------- events ----------

    pub fn handle(&mut self, mut event: AgentEvent, cx: &mut Context<Self>) {
        if let AgentEvent::ToolFinished { images, .. } = &mut event {
            self.save_tool_images(images);
        }
        // A call's time is measured here and saved with it, so a reopened thread shows it too.
        if let AgentEvent::ToolFinished { id, took_ms, .. } = &mut event
            && took_ms.is_none()
        {
            let started = self.tool_row(id).and_then(|ix| match &self.rows[ix] {
                Row::Tool(tool) => tool.started,
                _ => None,
            });
            *took_ms = started.map(|at| at.elapsed().as_millis() as u64);
        }
        match &event {
            // Process lifecycle and live usage aren't part of the conversation.
            AgentEvent::Started { .. } | AgentEvent::Exited { .. } | AgentEvent::Usage(_) | AgentEvent::Plan(_) => {}
            AgentEvent::TextDelta(text) if !self.replaying => self.pending_text.push_str(text),
            _ => self.write(Record::Event { event: event.clone() }),
        }
        if !self.replaying {
            self.last_active = std::time::Instant::now();
            self.track_background(&event);
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
            AgentEvent::Started { version, permission_mode, commands } => {
                if !commands.is_empty() {
                    self.session_commands = commands;
                }
                if let Some(version) = version.filter(|v| !self.version_warned && claude::is_newer_than_tested(v)) {
                    self.version_warned = true;
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
                if let Some(ix) = self.subagent_row(&tool_use_id) {
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
                if let Some(ix) = self.subagent_row(&tool_use_id)
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
            AgentEvent::CommandOutput { command, text } => {
                self.current_text = None;
                if !text.trim().is_empty() {
                    self.push_text_row(false, &text, cx);
                    self.plain.push(format!("(/{command} printed: {})", truncate(text.trim(), 600)));
                    self.plain_current = None;
                }
            }
            AgentEvent::ConversationReset => {
                self.current_text = None;
                self.context_tokens = None;
                self.plain.push("(The user cleared Claude's context; it starts again from here.)".into());
                self.plain_current = None;
                self.notice(
                    NoticeKind::Info,
                    "Context cleared. Claude starts a new conversation from here; the messages above are only kept for you.",
                    cx,
                );
            }
            AgentEvent::Compacted { pre_tokens, manual } => {
                self.current_text = None;
                self.context_tokens = None;
                let how = if manual {
                    "Conversation compacted"
                } else {
                    "Context was full, so the conversation was compacted"
                };
                let from = pre_tokens.map(|t| format!(" (from {}k tokens)", t.div_ceil(1000))).unwrap_or_default();
                self.notice(NoticeKind::Info, format!("{how}{from}. Claude carries on from a summary."), cx);
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
                let expanded = tool_card::opens_by_default(&name);
                let shell = (name == "Bash").then(|| {
                    tool_card::ShellText::new(input.get("command").and_then(Value::as_str).unwrap_or_default())
                });
                self.push_row(
                    Row::Tool(ToolRow {
                        id,
                        name,
                        input,
                        images: Vec::new(),
                        shell,
                        scroll: ScrollHandle::new(),
                        summary,
                        status: ToolStatus::Running,
                        output: None,
                        expanded,
                        started: (!self.replaying).then(std::time::Instant::now),
                        took: None,
                        subagent: None,
                    }),
                    cx,
                );
            }
            AgentEvent::ToolFinished { id, output, is_error, images, took_ms } => {
                let images: Vec<_> = images.iter().filter_map(|image| self.tool_image(image)).collect();
                if let Some(ix) = self.rows.iter().rposition(|r| matches!(r, Row::Tool(t) if t.id == id)) {
                    if let Row::Tool(tool) = &mut self.rows[ix] {
                        // A picture is the point of reading one, so its card opens.
                        if !images.is_empty() {
                            tool.expanded = true;
                            tool.images = images;
                        }
                        // A background subagent's call returns at once ("Async agent launched");
                        // the row stays working until the subagent itself finishes.
                        if tool.subagent.as_ref().is_some_and(|s| s.finished.is_none()) && !is_error {
                            self.remeasure(ix, cx);
                            return;
                        }
                        tool.status = if is_error { ToolStatus::Failed } else { ToolStatus::Done };
                        tool.took = took_ms.map(std::time::Duration::from_millis);
                        if let Some(shell) = &mut tool.shell {
                            shell.finish(&output, is_error);
                        }
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
                    "AskUserQuestion" => QuestionCard::parse(&request.input, cx)
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
                // An old transcript's window may be out of date; only a live one is kept.
                if let (false, Some(window), Some(model)) = (self.replaying, window, self.model_name()) {
                    crate::usage::Usage::set_context_window(cx, model, window);
                }
            }
            AgentEvent::Usage(windows) => crate::usage::Usage::update(cx, &self.account_id, |u| u.merge(windows)),
            AgentEvent::Plan(plan) => crate::usage::Usage::update(cx, &self.account_id, |u| u.plan = Some(plan)),
            AgentEvent::CostEstimate(dollars) => self.cost_estimate += dollars,
            AgentEvent::TurnFinished { cost_usd, process_total_usd, duration_ms, is_error, error } => {
                // Older transcripts kept the process's running total; a smaller one is a new
                // process, counting from zero again.
                let cost_usd = cost_usd.or_else(|| {
                    let total = process_total_usd?;
                    let turn = if total >= self.legacy_total { total - self.legacy_total } else { total };
                    self.legacy_total = total;
                    Some(turn)
                });
                self.current_text = None;
                self.status = Status::Idle;
                self.update_overview(cx);
                // Spending limits only come from get_usage, so ask again after each turn.
                if let (false, Some(agent)) = (self.replaying, &self.agent) {
                    let _ = agent.request_usage();
                }
                // Claude Code's figure replaces the running estimate. A turn without one (a slash
                // command) leaves it waiting: a background subagent may still be running up cost.
                if let Some(cost) = cost_usd {
                    self.cost_usd += cost;
                    self.cost_estimate = 0.0;
                }
                if is_error {
                    self.notice(NoticeKind::Error, error.unwrap_or_else(|| "The turn failed.".into()), cx);
                }
                let mut parts = Vec::new();
                // A local slash command's turn has no cost or time; a footer would only name the model.
                if let Some(model) = self.model.as_ref().filter(|_| cost_usd.is_some() || duration_ms.is_some()) {
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
                // The turn it was on won't report its cost now; keep what it was estimated at.
                self.cost_usd += std::mem::take(&mut self.cost_estimate);
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
        self.push_row(if user { Row::User(body, Vec::new()) } else { Row::Assistant(body) }, cx)
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
            && row.feedback.is_none()
        {
            let hint = match row.kind {
                ApprovalKind::Plan(_) => "Optional: what should change in the plan?",
                _ => "Optional: tell Claude why, or what to do instead",
            };
            row.feedback = Some(cx.new(|cx| InputState::new(window, cx).placeholder(hint)));
        }
        let theme = cx.theme();
        let (border, muted, secondary, accent) = (theme.border, theme.muted_foreground, theme.secondary, theme.primary);
        let mono = theme.mono_font_family.clone();
        let (danger, warning) = (theme.danger, theme.warning);
        let pad = |el: Div| el.w_full().px_4().py_1p5();

        match &self.rows[ix] {
            Row::User(body, images) => h_flex()
                .w_full()
                .px_4()
                .py_2()
                .justify_end()
                .child(
                    v_flex()
                        .max_w(relative(0.8))
                        .gap_2()
                        .px_3()
                        .py_2()
                        .rounded_lg()
                        .bg(secondary)
                        .when(!images.is_empty(), |el| {
                            el.child(h_flex().flex_wrap().gap_2().children(images.iter().map(|image| {
                                // A fixed height lets the width follow the picture, so tall ones
                                // don't sit in a wide box.
                                gpui_kit::img(image.clone()).h(px(160.)).max_w(px(280.)).rounded_md()
                            })))
                        })
                        .child(TextView::new(body).selectable(true)),
                )
                .into_any_element(),
            Row::Assistant(body) => {
                let thread = cx.entity().downgrade();
                pad(div())
                    .py_2()
                    .child(
                        TextView::new(body)
                            .selectable(true)
                            .stream_fade(true)
                            .code_block_actions(move |block, _, _| code_block_buttons(block, thread.clone())),
                    )
                    .into_any_element()
            }
            Row::Tool(tool) => {
                let sub = tool.subagent.as_ref();
                // A subagent's row says how far along it is.
                let label = sub
                    .map(|sub| {
                        let mut parts = Vec::new();
                        if let Some(n) = sub.tool_uses {
                            parts.push(format!("{n} tool{}", if n == 1 { "" } else { "s" }));
                        }
                        if let Some(t) = sub.tokens {
                            parts.push(format!("{}k tokens", t / 1000));
                        }
                        parts.join(" · ")
                    })
                    .filter(|label| !label.is_empty());
                let summary = match sub.and_then(|s| s.agent_type.clone()) {
                    Some(agent_type) => format!("{agent_type} · {}", tool.summary),
                    None => tool_card::header_summary(&tool.name, &tool.input, &tool.summary, &self.project_dir()),
                };
                let now = sub.and_then(|s| s.now.clone()).filter(|_| tool.status == ToolStatus::Running);
                let details = sub.filter(|_| tool.expanded).map(|sub| {
                    v_flex()
                        .gap_1()
                        .font_family(cx.theme().font_family.clone())
                        .children(sub.log.iter().map(|line| {
                            div().text_xs().font_family(mono.clone()).text_color(muted).child(line.clone())
                        }))
                        .when_some(sub.summary.clone(), |el, summary| el.child(div().pt_1().text_sm().child(summary)))
                        .into_any_element()
                });
                let card = tool_card::Card {
                    ix,
                    name: &tool.name,
                    input: &tool.input,
                    summary,
                    status: tool.status,
                    output: tool.output.as_deref(),
                    pictures: &tool.images,
                    label,
                    now,
                    expanded: tool.expanded,
                    details,
                    shell: tool.shell.as_ref(),
                    took: tool.took,
                    scroll: &tool.scroll,
                };
                let actions = tool_card::Actions {
                    toggle: cx.listener(move |this, _, _, cx| this.fold_tool(ix, cx)),
                    expand: cx.listener(move |this, _, window, cx| this.expand_tool(ix, window, cx)),
                    quote: cx.listener(move |this, _, window, cx| this.quote_tool(ix, window, cx)),
                };
                pad(div()).child(tool_card::render(card, window, cx, actions)).into_any_element()
            }
            Row::Approval(ApprovalRow { kind: ApprovalKind::Question(card), outcome, .. }) => {
                // The questions are answered in the modal over the conversation; this card
                // keeps them in the transcript, and opens the modal again once it's closed.
                let pending = outcome.is_none();
                let fg = theme.foreground;
                let questions = card.questions.iter().map(|q| {
                    h_flex()
                        .gap_2()
                        .when(!q.header.is_empty(), |el| {
                            el.child(
                                div()
                                    .flex_none()
                                    .px_1p5()
                                    .rounded_sm()
                                    .bg(secondary)
                                    .text_xs()
                                    .text_color(muted)
                                    .child(q.header.clone()),
                            )
                        })
                        .child(div().text_sm().text_color(fg).child(q.question.clone()))
                });
                pad(div())
                    .child(
                        v_flex()
                            .gap_2()
                            .p_3()
                            .rounded_md()
                            .border_1()
                            .border_color(if pending { accent } else { border })
                            .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).child(if pending {
                                "Claude is asking"
                            } else {
                                "Claude asked"
                            }))
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
                                                .label("Answer…")
                                                .on_click(
                                                    cx.listener(move |this, _, _, cx| this.open_questions(ix, cx)),
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
                                None => el
                                    .when_some(feedback.as_ref(), |el, f| {
                                        el.child(Input::new(f).small()).child(crate::caret::overlay(f))
                                    })
                                    .child(
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
                                None => el
                                    .when_some(row.feedback.as_ref(), |el, f| {
                                        el.child(Input::new(f).small()).child(crate::caret::overlay(f))
                                    })
                                    .child(
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
                                    ),
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
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.render_view(window, cx)
    }
}

impl ThreadView {
    fn render_view(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let bg = cx.theme().background;
        let this = cx.entity();
        // While the agent works (not while it waits on the user), a spinner verb waves under the
        // conversation.
        let working = if self.status == Status::Running {
            let step = crate::working::tick(window, cx);
            let (start, seed) = *self.working.get_or_insert_with(|| (step, crate::working::seed()));
            Some((step, step.saturating_sub(start), seed))
        } else {
            self.working = None;
            None
        };
        let accent = cx.theme().primary;
        let spinner = working.map(|(_, steps, seed)| spinner_line(seed, steps, accent));

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

        let attach = Button::new("attach")
            .ghost()
            .xsmall()
            .icon(gpui_kit::assets::IconName::Paperclip)
            .tooltip("Attach images or files (or paste or drop them here)")
            .on_click(cx.listener(|this, _, window, cx| this.pick_files(window, cx)));

        let card = self.render_composer_card(attach, account_menu, mode_menu, model_menu, cx).into_any_element();
        if self.is_fresh() {
            return self.render_intro(card, cx).into_any_element();
        }

        let questions = self.render_questions(window, cx);
        v_flex()
            .relative()
            .size_full()
            .rounded_t(px(crate::panes::CONTENT_RADIUS))
            .bg(bg)
            .child(div().flex_1().min_h_0().child(transcript))
            .when_some(spinner, |el, spinner| {
                el.child(
                    h_flex()
                        .w_full()
                        .justify_center()
                        // Lined up with the messages' text.
                        .child(div().w_full().max_w(px(MAX_CONVERSATION_WIDTH)).px_4().pt_1().pb_2().child(spinner)),
                )
            })
            .child(
                h_flex()
                    .w_full()
                    .justify_center()
                    .pb_3()
                    // Same width and inset as the messages above, so the edges line up.
                    .child(div().w_full().max_w(px(MAX_CONVERSATION_WIDTH)).px_3().child(card)),
            )
            .children(questions)
            .into_any_element()
    }

    /// The prompt card: attachments, the prompt, and a row of menus with a round send button
    /// (a red stop button while the agent works). The same card on the start screen and in a
    /// session.
    fn render_composer_card(
        &self,
        attach: impl IntoElement,
        account_menu: impl IntoElement,
        mode_menu: impl IntoElement,
        model_menu: impl IntoElement,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let (border, muted, card) = (theme.border, theme.muted_foreground, theme.secondary);
        let (accent, accent_fg) = (theme.primary, theme.primary_foreground);
        let can_send = self.agent.is_some() || self.backend.is_some();
        let fresh = self.is_fresh();
        let button = div().flex_none().size_8().rounded_full().flex().items_center().justify_center().cursor_pointer();
        let send = if self.status != Status::Idle {
            // The theme's red, so stopping reads as stopping.
            let (stop, stop_fg) = (theme.danger, theme.danger_foreground);
            button
                .id("stop")
                .bg(stop)
                .child(div().flex_none().size(px(STOP_SIZE)).rounded(px(2.)).bg(stop_fg))
                .on_click(cx.listener(|this, _, _, cx| this.interrupt(cx)))
        } else {
            button
                .id("send")
                .text_color(accent_fg)
                .bg(if can_send { accent } else { muted })
                .child(gpui_kit::component::Icon::new(gpui_kit::assets::IconName::ArrowUp).small())
                .on_click(cx.listener(|this, _, window, cx| this.submit(window, cx)))
        };

        self.composer_frame(cx)
            .w_full()
            .gap_2()
            .p_3()
            .rounded_xl()
            .border_1()
            .border_color(border)
            .bg(card)
            .children(self.render_attachments(cx))
            // Roomier on the start screen; in a session it grows with what's typed.
            .child(div().when(fresh, |el| el.min_h(px(64.))).child(self.prompt_box(false, cx)))
            // The prompt sits on the card, so typed letters grow in over its colour.
            .child(crate::caret::overlay_growing(&self.prompt, card))
            .child(
                h_flex()
                    .gap_1()
                    .text_color(muted)
                    .child(attach)
                    .child(account_menu)
                    .child(mode_menu)
                    .child(div().flex_1())
                    .child(model_menu)
                    .child(send),
            )
    }

    /// Before the first message: a heading, a large prompt card, and where the session will run.
    /// Like T3 Code's new-session screen.
    fn render_intro(&self, card: impl IntoElement, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (bg, muted, fg) = (theme.background, theme.muted_foreground, theme.foreground);
        let project = if self.menus.project_name.is_empty() {
            "this project".to_string()
        } else {
            self.menus.project_name.clone()
        };
        let footnote = |icon: gpui_kit::assets::IconName, text: String| {
            h_flex()
                .gap_1()
                .text_xs()
                .text_color(muted)
                .child(gpui_kit::component::Icon::new(icon).xsmall())
                .child(text)
        };

        // A little above centre, like T3 Code's start screen.
        v_flex()
            .size_full()
            .rounded_t(px(crate::panes::CONTENT_RADIUS))
            .bg(bg)
            .items_center()
            .justify_center()
            .px_6()
            .pb(px(140.))
            .child(
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
                    .child(card)
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

/// gpui's image format for a media type (`image/png` ...); PNG when unknown.
fn image_format(media_type: &str) -> gpui_kit::ImageFormat {
    match media_type {
        "image/jpeg" | "image/jpg" => gpui_kit::ImageFormat::Jpeg,
        "image/gif" => gpui_kit::ImageFormat::Gif,
        "image/webp" => gpui_kit::ImageFormat::Webp,
        _ => gpui_kit::ImageFormat::Png,
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
    use super::{Backend, Row, Status, ThreadView, command_text, is_shell};

    #[test]
    fn shell_blocks_give_the_command_as_typed() {
        assert!(is_shell("bash") && is_shell("Shell") && is_shell("zsh") && !is_shell("json") && !is_shell("rust"));
        assert_eq!(command_text("ls -la\n"), "ls -la");
        assert_eq!(command_text("$ cd src\n$ ls\n"), "cd src\nls");
        // A `$` that isn't a prompt on every line stays.
        assert_eq!(command_text("echo $HOME\n"), "echo $HOME");
        assert_eq!(command_text("curl x \\\n  | jq .\n"), "curl x \\\n  | jq .");
    }
    use crate::agent::{AgentEvent, Decision, ScriptedAgent, ScriptedCall, claude};
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{
        AppContext as _, Entity, TestAppContext, VisualTestContext, WindowBounds, WindowOptions, bounds, point, px,
        size,
    };

    fn fixture(name: &str) -> Vec<AgentEvent> {
        let path = format!("{}/tests/fixtures/claude/{name}", env!("CARGO_MANIFEST_DIR"));
        let mut parser = claude::Parser::default();
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
            t.handle(
                AgentEvent::TurnFinished {
                    cost_usd: None,
                    process_total_usd: None,
                    duration_ms: None,
                    is_error: false,
                    error: None,
                },
                cx,
            );
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
    fn the_cost_climbs_during_a_turn_and_settles_on_what_it_cost(cx: &mut TestAppContext) {
        let (window, thread) = open(cx);
        let cx = &mut VisualTestContext::from_window(window, cx);
        let finished = |cost: Option<f64>| AgentEvent::TurnFinished {
            cost_usd: cost,
            process_total_usd: None,
            duration_ms: None,
            is_error: false,
            error: None,
        };
        thread.update(cx, |t, cx| {
            t.send("Look around", cx);
            t.handle(AgentEvent::CostEstimate(0.25), cx);
            t.handle(AgentEvent::CostEstimate(0.5), cx);
            assert_eq!(t.cost(), 0.75, "shown while the agent works");
            t.handle(finished(Some(0.8)), cx);
            assert_eq!((t.cost(), t.cost_estimate), (0.8, 0.0), "Claude Code's figure replaces the estimate");

            t.send("And again", cx);
            t.handle(AgentEvent::CostEstimate(0.25), cx);
            t.handle(finished(None), cx);
            assert_eq!(t.cost(), 1.05, "with no figure, the estimate stands");
            assert_eq!(t.cost_estimate, 0.25, "until a figure comes or the process ends");
        });
    }

    #[gpui_kit::test]
    fn old_transcripts_count_each_turn_once(cx: &mut TestAppContext) {
        let (window, thread) = open(cx);
        let cx = &mut VisualTestContext::from_window(window, cx);
        // Saved before the turn's own cost was kept: Claude Code's running total per process,
        // which starts again from zero in a new process.
        let old = |total: f64| super::Record::Event {
            event: serde_json::from_value(serde_json::json!({
                "TurnFinished": { "cost_usd": total, "duration_ms": 1000, "is_error": false, "error": null }
            }))
            .unwrap(),
        };
        thread.update(cx, |t, cx| {
            t.replay(vec![old(1.0), old(1.5), old(2.25), old(0.5), old(0.75)], cx);
            // 1.0 + 0.5 + 0.75, then 0.5 + 0.25 in the next process.
            assert!((t.cost() - 3.0).abs() < 1e-9, "{}", t.cost());
        });
    }

    #[gpui_kit::test]
    fn an_idle_process_is_stopped_only_when_nothing_runs_in_it(cx: &mut TestAppContext) {
        let (window, thread) = open(cx);
        let cx = &mut VisualTestContext::from_window(window, cx);
        let finished = || AgentEvent::TurnFinished {
            cost_usd: None,
            process_total_usd: None,
            duration_ms: None,
            is_error: false,
            error: None,
        };
        thread.update(cx, |t, cx| {
            // Only a Claude process is stopped; the scripted agent stands in for its session.
            t.backend = Some(Backend::Claude(Default::default()));
            t.send("Look around", cx);
            assert_eq!(t.idle_for(), None, "a turn is running");
            let input = serde_json::json!({ "description": "survey", "run_in_background": true });
            t.handle(AgentEvent::ToolStarted { id: "bg".into(), name: "Agent".into(), input }, cx);
            t.handle(finished(), cx);
            assert_eq!(t.idle_for(), None, "a background subagent still works in the process");
            t.stop_idle_process();
            assert!(t.has_agent(), "so it isn't stopped");

            t.handle(
                AgentEvent::SubagentFinished { tool_use_id: "bg".into(), status: "completed".into(), summary: None },
                cx,
            );
            assert!(t.idle_for().is_some());
            t.stop_idle_process();
            assert!(!t.has_agent());
            assert_eq!(t.idle_for(), None, "nothing left to stop");
        });
    }

    /// A thread with one exchange that has just switched to Work, with the summary still
    /// being written (as for a long conversation on a real account).
    fn switched_and_summarizing(cx: &mut TestAppContext) -> (Entity<ThreadView>, VisualTestContext) {
        let (window, thread) = open(cx);
        let mut cx = VisualTestContext::from_window(window, cx);
        thread.update(&mut cx, |t, cx| {
            t.send("What does main.rs do?", cx);
            t.handle(AgentEvent::TextStarted, cx);
            t.handle(AgentEvent::TextDelta("It opens the window.".into()), cx);
            t.handle(
                AgentEvent::TurnFinished {
                    cost_usd: None,
                    process_total_usd: None,
                    duration_ms: None,
                    is_error: false,
                    error: None,
                },
                cx,
            );
            t.set_account("work", "Work", None, cx);
            t.handoff = None;
            t.summarizing = Some(("Personal".into(), "Work".into()));
        });
        (thread, cx)
    }

    fn prompts(thread: &Entity<ThreadView>, cx: &mut VisualTestContext) -> Vec<String> {
        calls(thread, cx)
            .into_iter()
            .filter_map(|c| match c {
                ScriptedCall::Prompt(p) => Some(p),
                _ => None,
            })
            .collect()
    }

    #[gpui_kit::test]
    fn a_message_sent_during_the_summary_waits_for_it(cx: &mut TestAppContext) {
        let (thread, mut cx) = switched_and_summarizing(cx);
        let cx = &mut cx;
        thread.update(cx, |t, cx| t.send("And store.rs?", cx));
        assert_eq!(prompts(&thread, cx).len(), 1, "only the first message has gone out");
        assert_eq!(thread.read_with(cx, |t, _| t.status), Status::Running, "it shows as working");

        thread.update(cx, |t, cx| {
            let run = t.handoff_run;
            t.finish_handoff(run, Ok("- main.rs opens the window".into()), cx)
        });
        let sent = prompts(&thread, cx).pop().unwrap();
        assert!(sent.starts_with("[Context from TJ") && sent.contains("account: Work"), "{sent}");
        assert!(sent.contains("Summary:\n- main.rs opens the window"));
        assert!(sent.contains("Newest messages:"));
        assert!(sent.ends_with("My new message follows.]\n\nAnd store.rs?"));
        assert_eq!(sent.matches("And store.rs?").count(), 1, "the new message isn't also in the context");
        assert!(!thread.read_with(cx, |t, _| t.has_pending_handoff()));
    }

    #[gpui_kit::test]
    fn a_failed_summary_falls_back_to_the_conversation(cx: &mut TestAppContext) {
        let (thread, mut cx) = switched_and_summarizing(cx);
        let cx = &mut cx;
        thread.update(cx, |t, cx| {
            let stale = t.handoff_run - 1;
            t.finish_handoff(stale, Ok("from an older switch".into()), cx);
            assert!(t.summarizing.is_some(), "a late summary from an older switch is ignored");
            t.send("And store.rs?", cx);
            let run = t.handoff_run;
            t.finish_handoff(run, Err(anyhow::anyhow!("offline")), cx);
        });
        let sent = prompts(&thread, cx).pop().unwrap();
        assert!(sent.contains("User: What does main.rs do?") && sent.contains("Claude: It opens the window."));
        assert_eq!(sent.matches("And store.rs?").count(), 1);
    }

    #[gpui_kit::test]
    fn stop_cancels_a_message_waiting_for_the_summary(cx: &mut TestAppContext) {
        let (thread, mut cx) = switched_and_summarizing(cx);
        let cx = &mut cx;
        thread.update(cx, |t, cx| {
            t.send("And store.rs?", cx);
            t.interrupt(cx);
            let run = t.handoff_run;
            t.finish_handoff(run, Ok("summary".into()), cx);
        });
        assert_eq!(prompts(&thread, cx).len(), 1, "the stopped message never went out");
        assert_eq!(thread.read_with(cx, |t, _| t.status), Status::Idle);
        assert!(thread.read_with(cx, |t, _| t.has_pending_handoff()), "the handoff waits for the next message");
    }

    #[gpui_kit::test]
    fn a_switch_without_a_message_is_handed_off_after_a_restart(cx: &mut TestAppContext) {
        let path = std::env::temp_dir().join(format!("tj-handoff-{}.jsonl", crate::store::new_id()));
        let setup = super::ThreadSetup { transcript: Some(path.clone()), ..Default::default() };
        let (window, _thread) = open(cx);
        let cx = &mut VisualTestContext::from_window(window, cx);
        let first = cx.update(|window, cx| {
            cx.new(|cx| ThreadView::open(setup.clone(), Backend::Scripted(Vec::new()), window, cx))
        });
        first.update(cx, |t, cx| {
            t.attach_scripted(ScriptedAgent::start(Vec::new()).0);
            t.send("What does main.rs do?", cx);
            t.set_account("work", "Work", None, cx);
        });
        let reopened =
            cx.update(|window, cx| cx.new(|cx| ThreadView::open(setup, Backend::Scripted(Vec::new()), window, cx)));
        assert!(reopened.read_with(cx, |t, _| t.has_pending_handoff()), "the handoff is prepared again");
        let _ = std::fs::remove_file(path);
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

    /// A thread that ran `echo short` (printing "ok") and `echo long` (printing 300 lines);
    /// returns the two cards' rows.
    fn two_bash_cards(thread: &Entity<ThreadView>, cx: &mut VisualTestContext) -> (usize, usize) {
        let long: String = (1..=300).map(|i| format!("line {i}\n")).collect();
        thread.update(cx, |t, cx| {
            t.send("Run two commands", cx);
            for (id, output) in [("short", "ok"), ("long", long.as_str())] {
                let input = serde_json::json!({ "command": format!("echo {id}"), "description": format!("Say {id}") });
                t.handle(AgentEvent::ToolStarted { id: id.into(), name: "Bash".into(), input }, cx);
                t.handle(
                    AgentEvent::ToolFinished {
                        id: id.into(),
                        output: output.to_string(),
                        is_error: false,
                        images: Vec::new(),
                        took_ms: None,
                    },
                    cx,
                );
            }
        });
        cx.update(|window, cx| window.render_frame(cx));
        let rows = thread.read_with(cx, |t, _| t.rows.len());
        (rows - 2, rows - 1)
    }

    #[gpui_kit::test]
    fn a_long_bash_command_reported_as_a_task_keeps_its_output(cx: &mut TestAppContext) {
        let (window, thread) = open(cx);
        let cx = &mut VisualTestContext::from_window(window, cx);
        thread.update(cx, |t, cx| {
            t.send("Build it", cx);
            let input = serde_json::json!({ "command": "cargo build", "description": "Build the app" });
            t.handle(AgentEvent::ToolStarted { id: "b".into(), name: "Bash".into(), input }, cx);
            // Claude Code reports a command that runs a while as a task, like a subagent.
            let description = "Build the app".to_string();
            t.handle(AgentEvent::SubagentStarted { tool_use_id: "b".into(), description, agent_type: None }, cx);
            let output = "Finished".to_string();
            t.handle(
                AgentEvent::ToolFinished { id: "b".into(), output, is_error: false, images: Vec::new(), took_ms: None },
                cx,
            );
            let summary = Some("Build the app".to_string());
            t.handle(AgentEvent::SubagentFinished { tool_use_id: "b".into(), status: "completed".into(), summary }, cx);
        });
        thread.read_with(cx, |t, _| {
            let Some(Row::Tool(tool)) = t.rows.last() else { panic!("not a tool row") };
            assert!(tool.subagent.is_none());
            assert_eq!(tool.status, crate::tool_card::ToolStatus::Done);
            assert_eq!(tool.output.as_deref(), Some("Finished"));
            assert!(!t.plain.iter().any(|line| line.starts_with("(Subagent")));
        });
    }

    #[gpui_kit::test]
    fn a_long_bash_card_is_cut_short_and_opens_in_a_dialog(cx: &mut TestAppContext) {
        use gpui_kit::component::WindowExt as _;
        let (window, thread) = open(cx);
        let cx = &mut VisualTestContext::from_window(window, cx);
        let (short, long) = two_bash_cards(&thread, cx);
        let tool = |ix: usize, cx: &mut VisualTestContext| {
            thread.read_with(cx, |t, _| match &t.rows[ix] {
                Row::Tool(tool) => (tool.expanded, tool.shell.as_ref().unwrap().hidden_lines(), tool.took.is_some()),
                _ => panic!("not a tool row"),
            })
        };
        // Both are timed; only the long one leaves lines out (its command, then 29 of 300).
        assert_eq!(tool(short, cx), (true, 0, true));
        assert_eq!(tool(long, cx), (true, 271, true));

        // The header neither folds a Bash card nor opens the dialog.
        for ix in [short, long] {
            cx.update(|window, cx| window.click(gpui_kit::SharedString::from(format!("tool-{ix}")), cx));
            assert!(tool(ix, cx).0);
        }
        assert!(!cx.update(|window, cx| window.has_active_dialog(cx)));

        // "Click to expand" under the cut text opens all of it.
        cx.update(|window, cx| window.click(gpui_kit::SharedString::from(format!("tool-expand-{long}")), cx));
        assert!(cx.update(|window, cx| window.has_active_dialog(cx)));
        assert!(tool(long, cx).0);
    }

    #[gpui_kit::test]
    fn a_tool_card_is_added_to_the_prompt_and_sent_with_the_question(cx: &mut TestAppContext) {
        let (window, thread) = open(cx);
        let cx = &mut VisualTestContext::from_window(window, cx);
        let (short, long) = two_bash_cards(&thread, cx);
        let click = |id: String, cx: &mut VisualTestContext| {
            cx.update(|window, cx| {
                window.click(gpui_kit::SharedString::from(id), cx);
                window.render_frame(cx);
            });
        };
        let labels = |cx: &mut VisualTestContext| {
            thread.read_with(cx, |t, _| t.quotes.iter().map(|q| q.label.clone()).collect::<Vec<_>>())
        };

        // The header's button adds the call once, and gives the prompt box the keyboard.
        click(format!("tool-ask-{short}"), cx);
        click(format!("tool-ask-{short}"), cx);
        click(format!("tool-ask-{long}"), cx);
        assert_eq!(labels(cx), ["Bash · Say short", "Bash · Say long"]);
        assert!(cx.update(|window, cx| thread.read(cx).prompt_focus(cx).is_focused(window)));

        // A chip's own button takes it out again.
        cx.update(|window, cx| window.click(("remove-quote", 1usize), cx));
        assert_eq!(labels(cx), ["Bash · Say short"]);

        // The message names the call before what was typed.
        thread.update_in(cx, |t, window, cx| {
            t.prompt.update(cx, |p, cx| p.set_value("Why did it print that?", window, cx));
            t.submit(window, cx);
        });
        let sent = thread.read_with(cx, |t, _| t.plain.last().cloned().unwrap());
        assert_eq!(
            sent,
            "User: About your earlier Bash call (Say short):\n```\n$ echo short\n```\n\nWhy did it print that?"
        );
        assert!(labels(cx).is_empty());
    }

    #[gpui_kit::test]
    fn a_read_picture_is_saved_beside_the_transcript_and_shown_again(cx: &mut TestAppContext) {
        use base64::Engine as _;
        let path = std::env::temp_dir().join(format!("tj-picture-{}.jsonl", crate::store::new_id()));
        let setup = super::ThreadSetup { transcript: Some(path.clone()), ..Default::default() };
        let (window, _thread) = open(cx);
        let cx = &mut VisualTestContext::from_window(window, cx);
        let png = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/data/icons/tj-32.png")).unwrap();
        let data = base64::engine::general_purpose::STANDARD.encode(&png);
        let first = cx.update(|window, cx| {
            cx.new(|cx| ThreadView::open(setup.clone(), Backend::Scripted(Vec::new()), window, cx))
        });
        first.update(cx, |t, cx| {
            let input = serde_json::json!({ "file_path": "/p/icon.png" });
            t.handle(AgentEvent::ToolStarted { id: "1".into(), name: "Read".into(), input }, cx);
            let image = crate::agent::ToolImage { media_type: "image/png".into(), data: data.clone(), file: None };
            t.handle(
                AgentEvent::ToolFinished {
                    id: "1".into(),
                    output: String::new(),
                    is_error: false,
                    images: vec![image],
                    took_ms: None,
                },
                cx,
            );
        });
        cx.update(|window, cx| window.render_frame(cx));

        // The transcript keeps a file name, not the picture.
        let transcript = std::fs::read_to_string(&path).unwrap();
        assert!(!transcript.contains(&data[..40]));
        let files: Vec<_> = std::fs::read_dir(crate::composer::files_dir(&path)).unwrap().flatten().collect();
        assert_eq!(files.len(), 1);
        assert_eq!(std::fs::read(files[0].path()).unwrap(), png);

        let second =
            cx.update(|window, cx| cx.new(|cx| ThreadView::open(setup, Backend::Scripted(Vec::new()), window, cx)));
        let shown = |t: &ThreadView| match t.rows.last() {
            Some(Row::Tool(tool)) => (tool.images.len(), tool.expanded),
            _ => (0, false),
        };
        assert_eq!(first.read_with(cx, |t, _| shown(t)), (1, true));
        assert_eq!(second.read_with(cx, |t, _| shown(t)), (1, true));
        let _ = std::fs::remove_dir_all(crate::composer::files_dir(&path));
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
    fn a_lone_question_is_sent_when_an_option_is_clicked(cx: &mut TestAppContext) {
        let (window, thread) = open(cx);
        let cx = &mut VisualTestContext::from_window(window, cx);
        let ix = first_card(&thread, fixture("2.1.331-question.jsonl"), cx);
        cx.update(|window, cx| window.click(gpui_kit::SharedString::from("question-option-0-1"), cx));
        let expected = std::collections::BTreeMap::from([(
            "Which colour do you prefer for a button?".to_string(),
            "Blue".to_string(),
        )]);
        assert_eq!(responses(&thread, cx), vec![Decision::Answer { answers: expected }]);
        assert_eq!(thread.read_with(cx, |t, _| t.approval_outcome(ix)).as_deref(), Some("Answered: Blue"));
        let prompt_focused = cx.update(|window, cx| thread.read(cx).prompt_focus(cx).is_focused(window));
        assert!(prompt_focused, "the prompt box gets the keyboard back");
    }

    fn two_questions() -> AgentEvent {
        AgentEvent::PermissionRequested(crate::agent::PermissionRequest {
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
        })
    }

    fn current_question(thread: &Entity<ThreadView>, ix: usize, cx: &mut VisualTestContext) -> usize {
        thread.read_with(cx, |t, _| match &t.rows[ix] {
            super::Row::Approval(super::ApprovalRow { kind: super::ApprovalKind::Question(card), .. }) => card.current,
            _ => panic!("not a question card"),
        })
    }

    fn frame(cx: &mut VisualTestContext) {
        cx.update(|window, cx| window.render_frame(cx));
    }

    #[gpui_kit::test]
    fn questions_are_answered_from_the_keyboard(cx: &mut TestAppContext) {
        let (window, thread) = open(cx);
        let cx = &mut VisualTestContext::from_window(window, cx);
        let ix = first_card(&thread, vec![two_questions()], cx);
        frame(cx);
        // Space ticks options of a multi-select; Enter finishes it and opens the next question.
        cx.simulate_keystrokes("space down space");
        frame(cx);
        cx.simulate_keystrokes("enter");
        frame(cx);
        assert_eq!(current_question(&thread, ix, cx), 1);
        // Down past the options lands in the "Other" field, where typing (spaces included) works.
        cx.simulate_keystrokes("down down");
        frame(cx);
        cx.simulate_input("release 2.0");
        frame(cx);
        assert!(responses(&thread, cx).is_empty(), "nothing is sent while typing");
        cx.simulate_keystrokes("enter");
        frame(cx);
        assert_eq!(current_question(&thread, ix, cx), 2, "both answered: the review step");
        // Back to the first question and forward again, then Enter sends.
        cx.simulate_keystrokes("left");
        frame(cx);
        assert_eq!(current_question(&thread, ix, cx), 1);
        cx.simulate_keystrokes("right");
        frame(cx);
        assert_eq!(current_question(&thread, ix, cx), 2);
        cx.simulate_keystrokes("enter");
        let expected = std::collections::BTreeMap::from([
            ("Which checks?".to_string(), "Tests, Lint".to_string()),
            ("Which branch?".to_string(), "release 2.0".to_string()),
        ]);
        assert_eq!(responses(&thread, cx), vec![Decision::Answer { answers: expected }]);
    }

    #[gpui_kit::test]
    fn number_keys_pick_and_escape_closes_the_modal_until_it_is_reopened(cx: &mut TestAppContext) {
        let (window, thread) = open(cx);
        let cx = &mut VisualTestContext::from_window(window, cx);
        let ix = first_card(&thread, vec![two_questions()], cx);
        frame(cx);
        cx.simulate_keystrokes("2 enter");
        frame(cx);
        assert_eq!(current_question(&thread, ix, cx), 1);

        // Escape from the "Other" field goes back to the options; from there it closes.
        cx.simulate_keystrokes("up");
        frame(cx);
        cx.simulate_keystrokes("escape");
        frame(cx);
        let modal_open = cx.update(|window, _| window.try_find("question-modal").is_some());
        assert!(modal_open, "the first Escape only leaves the field");
        cx.simulate_keystrokes("escape");
        frame(cx);
        let modal_open = cx.update(|window, _| window.try_find("question-modal").is_some());
        assert!(!modal_open, "the second closes the modal");
        assert!(responses(&thread, cx).is_empty(), "closing doesn't answer");
        assert_eq!(thread.read_with(cx, |t, _| t.approval_outcome(ix)), None, "the question still waits");

        // The card in the conversation opens it again, with the answers so far.
        cx.update(|window, cx| window.click(gpui_kit::SharedString::from(format!("answer-{ix}")), cx));
        frame(cx);
        cx.simulate_keystrokes("1");
        frame(cx);
        cx.simulate_keystrokes("enter");
        let expected = std::collections::BTreeMap::from([
            ("Which checks?".to_string(), "Lint".to_string()),
            ("Which branch?".to_string(), "main".to_string()),
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

    /// The prompt's growing letters match the field's own layout (font, size, place).
    #[gpui_kit::test]
    fn a_letter_typed_into_the_prompt_grows_in(cx: &mut TestAppContext) {
        let (window, thread) = open(cx);
        let cx = &mut VisualTestContext::from_window(window, cx);
        cx.update(|_, cx| crate::caret::init(cx));
        type_into_prompt(&thread, "", cx);
        let prompt = thread.read_with(cx, |t, _| t.prompt.clone());
        prompt.update_in(cx, |p, window, cx| p.set_value("hell", window, cx));
        cx.update(|window, cx| window.render_frame(cx));
        assert_eq!(cx.update(|_, cx| crate::caret::growing_letters(cx)), 0, "set by the app, not typed");
        cx.simulate_keystrokes("end");
        cx.update(|window, cx| window.render_frame(cx));
        cx.simulate_keystrokes("o");
        cx.update(|window, cx| window.render_frame(cx));
        assert_eq!(prompt_text(&thread, cx), "hello");
        assert_eq!(cx.update(|_, cx| crate::caret::growing_letters(cx)), 1, "the o grows in");
        cx.simulate_keystrokes("backspace");
        cx.update(|window, cx| window.render_frame(cx));
        assert_eq!(cx.update(|_, cx| crate::caret::growing_letters(cx)), 0, "and stops when it's deleted");
    }

    /// Focus the prompt box and type into it.
    fn type_into_prompt(thread: &Entity<ThreadView>, text: &str, cx: &mut VisualTestContext) {
        cx.update(|window, cx| {
            window.render_frame(cx);
            let focus = thread.read(cx).prompt_focus(cx);
            window.focus(&focus, cx);
        });
        cx.simulate_input(text);
        cx.run_until_parked();
        cx.update(|window, cx| window.render_frame(cx));
    }

    fn prompt_text(thread: &Entity<ThreadView>, cx: &mut VisualTestContext) -> String {
        thread.read_with(cx, |t, cx| t.prompt.read(cx).value().to_string())
    }

    fn png(w: u32, h: u32) -> Vec<u8> {
        let mut bytes = Vec::new();
        image::RgbImage::from_pixel(w, h, image::Rgb([200, 80, 40]))
            .write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageFormat::Png)
            .unwrap();
        bytes
    }

    #[gpui_kit::test]
    fn at_opens_a_file_picker_and_enter_picks_instead_of_sending(cx: &mut TestAppContext) {
        let (window, thread) = open(cx);
        let cx = &mut VisualTestContext::from_window(window, cx);
        // Tests run in the crate, so the picker lists TJ's own files.
        type_into_prompt(&thread, "look at @thread.r", cx);
        let first = thread.read_with(cx, |t, _| t.mention.as_ref().and_then(|m| m.matches.first().cloned()));
        assert_eq!(first.as_deref(), Some("src/thread.rs"));

        cx.simulate_keystrokes("enter");
        assert_eq!(prompt_text(&thread, cx), "look at @src/thread.rs ");
        assert!(thread.read_with(cx, |t, _| t.mention.is_none()), "picking closes the picker");
        assert!(!calls(&thread, cx).iter().any(|c| matches!(c, ScriptedCall::Prompt(_))), "Enter picked, not sent");
        let tokens = thread.read_with(cx, |t, cx| t.prompt.read(cx).tokens().len());
        assert_eq!(tokens, 1, "the mention is one token");

        cx.update(|window, cx| window.render_frame(cx));
        cx.simulate_keystrokes("enter");
        assert!(calls(&thread, cx).contains(&ScriptedCall::Prompt("look at @src/thread.rs".into())));
    }

    #[gpui_kit::test]
    fn enter_waits_while_the_first_file_list_loads(cx: &mut TestAppContext) {
        let (window, thread) = open(cx);
        let cx = &mut VisualTestContext::from_window(window, cx);
        cx.update(|window, cx| {
            window.render_frame(cx);
            let focus = thread.read(cx).prompt_focus(cx);
            window.focus(&focus, cx);
        });
        cx.simulate_input("@thread.r");
        // As if Enter came before the background listing finished (tests finish it at once).
        thread.update(cx, |t, _| {
            t.project_files = None;
            t.listing_files = true;
        });
        cx.update(|window, cx| window.render_frame(cx));
        cx.simulate_keystrokes("enter");
        assert!(!calls(&thread, cx).iter().any(|c| matches!(c, ScriptedCall::Prompt(_))), "nothing was sent");
        assert_eq!(prompt_text(&thread, cx), "@thread.r");
        thread.update(cx, |t, cx| t.list_project_files(cx));
        cx.run_until_parked();
        cx.update(|window, cx| window.render_frame(cx));
        cx.simulate_keystrokes("enter");
        assert_eq!(prompt_text(&thread, cx), "@src/thread.rs ");
    }

    fn commands() -> Vec<crate::agent::SlashCommand> {
        [
            ("compact", "Summarize the conversation", "<instructions>"),
            ("context", "Show context usage", ""),
            ("clear", "Start over", ""),
        ]
        .map(|(name, description, hint)| crate::agent::SlashCommand {
            name: name.into(),
            description: description.into(),
            argument_hint: hint.into(),
        })
        .to_vec()
    }

    #[gpui_kit::test]
    fn a_slash_opens_the_command_picker_and_enter_picks_one(cx: &mut TestAppContext) {
        let (window, thread) = open(cx);
        let cx = &mut VisualTestContext::from_window(window, cx);
        thread.update(cx, |t, _| t.menus.commands = commands());
        type_into_prompt(&thread, "/co", cx);
        let names = thread.read_with(cx, |t, _| {
            t.command_picker.as_ref().map(|p| p.matches.iter().map(|c| c.name.clone()).collect::<Vec<_>>())
        });
        assert_eq!(names, Some(vec!["compact".to_string(), "context".to_string()]));

        cx.simulate_keystrokes("down enter");
        assert_eq!(prompt_text(&thread, cx), "/context ");
        assert!(thread.read_with(cx, |t, _| t.command_picker.is_none()), "picking closes the picker");
        assert!(!calls(&thread, cx).iter().any(|c| matches!(c, ScriptedCall::Prompt(_))), "Enter picked, not sent");

        cx.update(|window, cx| window.render_frame(cx));
        cx.simulate_keystrokes("enter");
        assert!(
            calls(&thread, cx).contains(&ScriptedCall::Prompt("/context".into())),
            "sent as typed, for Claude to run"
        );
    }

    #[gpui_kit::test]
    fn enter_on_a_command_typed_in_full_sends_it(cx: &mut TestAppContext) {
        let (window, thread) = open(cx);
        let cx = &mut VisualTestContext::from_window(window, cx);
        thread.update(cx, |t, _| t.menus.commands = commands());
        type_into_prompt(&thread, "/clear", cx);
        assert!(thread.read_with(cx, |t, _| t.command_picker.is_some()));
        cx.simulate_keystrokes("enter");
        assert!(calls(&thread, cx).contains(&ScriptedCall::Prompt("/clear".into())));
    }

    #[gpui_kit::test]
    fn the_command_picker_is_only_for_the_first_word(cx: &mut TestAppContext) {
        let (window, thread) = open(cx);
        let cx = &mut VisualTestContext::from_window(window, cx);
        thread.update(cx, |t, _| t.menus.commands = commands());
        type_into_prompt(&thread, "/comp", cx);
        cx.simulate_keystrokes("tab");
        assert_eq!(prompt_text(&thread, cx), "/compact ");
        type_into_prompt(&thread, "keep /co", cx);
        assert!(thread.read_with(cx, |t, _| t.command_picker.is_none()), "arguments don't open it");

        // The live session's own list wins over the one read ahead of time.
        thread.update(cx, |t, cx| {
            t.handle(
                AgentEvent::Started {
                    version: None,
                    permission_mode: None,
                    commands: vec![crate::agent::SlashCommand {
                        name: "deploy".into(),
                        description: String::new(),
                        argument_hint: String::new(),
                    }],
                },
                cx,
            );
            assert_eq!(t.commands().len(), 1);
        });
    }

    #[gpui_kit::test]
    fn a_local_command_shows_what_it_printed_without_a_footer(cx: &mut TestAppContext) {
        let (window, thread) = open(cx);
        let cx = &mut VisualTestContext::from_window(window, cx);
        thread.update(cx, |t, cx| {
            let before = t.rows.len();
            t.handle(
                AgentEvent::CommandOutput { command: "effort".into(), text: "Set effort level to high".into() },
                cx,
            );
            t.handle(AgentEvent::TurnFinished { cost_usd: None, process_total_usd: None, duration_ms: None, is_error: false, error: None }, cx);
            assert_eq!(t.rows.len(), before + 1, "one row, and no footer");
            assert!(matches!(t.rows.last(), Some(Row::Assistant(_))));
            t.handle(AgentEvent::ConversationReset, cx);
            assert!(
                matches!(t.rows.last(), Some(Row::Notice(super::NoticeKind::Info, text)) if text.contains("Context cleared"))
            );
        });
    }

    #[gpui_kit::test]
    fn up_and_down_in_the_prompt_walk_the_sent_messages(cx: &mut TestAppContext) {
        let (window, thread) = open(cx);
        let cx = &mut VisualTestContext::from_window(window, cx);
        thread.update(cx, |t, cx| {
            t.send("first", cx);
            t.send("second\nline two", cx);
        });
        type_into_prompt(&thread, "draft", cx);
        let caret = |cx: &mut VisualTestContext| thread.read_with(cx, |t, cx| t.prompt.read(cx).cursor());

        cx.simulate_keystrokes("up");
        assert_eq!(prompt_text(&thread, cx), "second\nline two");
        assert_eq!(caret(cx), 0, "going back, the caret starts at the top");
        cx.simulate_keystrokes("up");
        assert_eq!(prompt_text(&thread, cx), "first");
        cx.simulate_keystrokes("up");
        assert_eq!(prompt_text(&thread, cx), "first", "the oldest");
        cx.simulate_keystrokes("down");
        assert_eq!(prompt_text(&thread, cx), "second\nline two");
        assert_eq!(caret(cx), "second\nline two".len(), "going forward, at the end");
        cx.simulate_keystrokes("down");
        assert_eq!(prompt_text(&thread, cx), "draft", "past the newest, the draft comes back");

        // Up in a message of several lines moves up a line before it recalls anything.
        thread.update_in(cx, |t, window, cx| t.prompt.update(cx, |p, cx| p.set_value("", window, cx)));
        type_into_prompt(&thread, "one\ntwo", cx);
        cx.simulate_keystrokes("up");
        assert_eq!(prompt_text(&thread, cx), "one\ntwo");
        cx.simulate_keystrokes("up");
        assert_eq!(prompt_text(&thread, cx), "second\nline two");
    }

    #[gpui_kit::test]
    fn escape_in_the_prompt_stops_a_working_agent(cx: &mut TestAppContext) {
        let (window, thread) = open(cx);
        let cx = &mut VisualTestContext::from_window(window, cx);
        type_into_prompt(&thread, "", cx);
        cx.simulate_keystrokes("escape");
        let interrupts = |cx: &mut VisualTestContext| {
            calls(&thread, cx).into_iter().filter(|c| matches!(c, ScriptedCall::Interrupt)).count()
        };
        assert_eq!(interrupts(cx), 0, "nothing to stop");

        thread.update(cx, |t, cx| t.send("Look around", cx));
        type_into_prompt(&thread, "@cargo", cx);
        assert!(thread.read_with(cx, |t, _| t.mention.is_some()));
        cx.simulate_keystrokes("escape");
        assert_eq!(interrupts(cx), 0, "the first Escape closes the picker");
        cx.simulate_keystrokes("escape");
        assert_eq!(interrupts(cx), 1, "the next one stops the agent");
        assert_eq!(prompt_text(&thread, cx), "@cargo", "and keeps what was typed");
    }

    #[gpui_kit::test]
    fn tab_picks_a_mention_and_escape_closes_the_picker(cx: &mut TestAppContext) {
        let (window, thread) = open(cx);
        let cx = &mut VisualTestContext::from_window(window, cx);
        type_into_prompt(&thread, "@cargo", cx);
        cx.simulate_keystrokes("down up tab");
        assert_eq!(prompt_text(&thread, cx), "@Cargo.lock ");

        type_into_prompt(&thread, "and @READ", cx);
        assert!(thread.read_with(cx, |t, _| t.mention.is_some()));
        cx.simulate_keystrokes("escape");
        assert!(thread.read_with(cx, |t, _| t.mention.is_none()));
        assert_eq!(prompt_text(&thread, cx), "@Cargo.lock and @READ", "Escape keeps what was typed");
    }

    #[gpui_kit::test]
    fn a_pasted_image_goes_with_the_next_message_and_survives_a_reopen(cx: &mut TestAppContext) {
        let path = std::env::temp_dir().join(format!("tj-image-{}.jsonl", crate::store::new_id()));
        let setup = super::ThreadSetup { transcript: Some(path.clone()), ..Default::default() };
        let (window, _thread) = open(cx);
        let cx = &mut VisualTestContext::from_window(window, cx);
        let thread = cx.update(|window, cx| {
            cx.new(|cx| ThreadView::open(setup.clone(), Backend::Scripted(Vec::new()), window, cx))
        });
        thread.update(cx, |t, _| t.attach_scripted(ScriptedAgent::start(Vec::new()).0));
        cx.update(|_, cx| {
            cx.write_to_clipboard(gpui_kit::ClipboardItem {
                entries: vec![gpui_kit::ClipboardEntry::Image(gpui_kit::Image::from_bytes(
                    gpui_kit::ImageFormat::Png,
                    png(8, 6),
                ))],
            });
        });
        thread.update_in(cx, |t, _, cx| {
            assert!(t.paste_images(cx), "an image-only clipboard is taken");
        });
        cx.run_until_parked();
        assert_eq!(thread.read_with(cx, |t, _| t.attachments.len()), 1);

        thread.update_in(cx, |t, window, cx| t.submit(window, cx));
        assert_eq!(
            calls(&thread, cx),
            vec![ScriptedCall::Images(vec!["image/png".into()]), ScriptedCall::Prompt(String::new())]
        );
        let shown = |t: &ThreadView| match t.rows.last() {
            Some(Row::User(_, images)) => images.len(),
            _ => 0,
        };
        assert_eq!(thread.read_with(cx, |t, _| (shown(t), t.attachments.len())), (1, 0));
        assert_eq!(thread.read_with(cx, |t, _| t.title.clone()).as_deref(), Some("Image"));

        let reopened =
            cx.update(|window, cx| cx.new(|cx| ThreadView::open(setup, Backend::Scripted(Vec::new()), window, cx)));
        assert_eq!(reopened.read_with(cx, |t, _| shown(t)), 1, "the saved image is shown again");
        let _ = std::fs::remove_dir_all(crate::composer::files_dir(&path));
        let _ = std::fs::remove_file(path);
    }
}
