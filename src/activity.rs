//! A thread's activity: the shells Claude left running in the background and the subagents it
//! started, which the status column lists, and the ordinary shell commands it ran, which open
//! from their cards. One opens in [`ActivityViewer`], a dialog with its output, where ← and →
//! step through the others of its sort like the Markdown viewer. A background shell's output is read from the file Claude Code writes it to,
//! and follows it while it grows.

use std::{
    io::{Read as _, Seek as _, SeekFrom},
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

use gpui_kit::assets::IconName;
use gpui_kit::base::{h_flex, v_flex};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, Sizable as _,
    button::{Button, ButtonVariants as _},
    dialog::Dialog,
    spinner::Spinner,
    text::{TextView, TextViewState},
};
use gpui_kit::{TestSupportExt as _, prelude::FluentBuilder as _, *};

use crate::thread::ThreadView;
use crate::tool_card::{self, ShellText};

/// The most of a background shell's output the dialog reads: the end of it.
const TAIL_BYTES: u64 = 256 * 1024;
/// How often an open background shell's output file is checked for more.
const POLL_EVERY: Duration = Duration::from_millis(700);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A Bash call that ran to the end before Claude went on.
    Shell,
    /// A shell left running: Bash with `run_in_background`, or Monitor.
    Background,
    /// A subagent (the Agent tool).
    Subagent,
}

impl Kind {
    pub fn icon(self) -> IconName {
        match self {
            Kind::Shell => IconName::SquareTerminal,
            Kind::Background => IconName::Activity,
            Kind::Subagent => IconName::Bot,
        }
    }

    /// Background shells and subagents: what the status column lists. Ordinary commands
    /// are only opened from their cards.
    pub fn is_task(self) -> bool {
        self != Kind::Shell
    }

    pub fn label(self) -> &'static str {
        match self {
            Kind::Shell => "Bash",
            Kind::Background => "Background shell",
            Kind::Subagent => "Subagent",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Running,
    Done,
    Failed,
    /// A background shell whose Claude process ended before it reported back.
    Gone,
}

/// One entry in the activity list.
#[derive(Clone, Debug, PartialEq)]
pub struct Item {
    /// The tool call's id.
    pub id: String,
    pub kind: Kind,
    /// The command's description (or its first line), or the subagent's task.
    pub title: String,
    pub state: State,
    /// A word on the side: how long a command took, "background", a subagent's type and tools.
    pub note: Option<String>,
}

/// Everything the dialog shows for one entry, read from the thread.
#[derive(Clone)]
pub struct Detail {
    pub item: Item,
    /// A shell's command.
    pub command: String,
    /// A finished command's text, coloured, as its card has it.
    pub shell: Option<ShellText>,
    pub took: Option<Duration>,
    /// What the tool call returned (for a background call, the line saying it started).
    pub output: Option<String>,
    /// Where a background shell's output goes.
    pub output_file: Option<PathBuf>,
    /// How a background shell or subagent ended.
    pub ended: Option<String>,
    pub agent_type: Option<String>,
    /// What the subagent was asked to do.
    pub prompt: Option<String>,
    /// What a running subagent is doing now.
    pub now: Option<String>,
    /// A subagent's tool calls and messages.
    pub log: Vec<String>,
    /// A subagent's report.
    pub report: Option<String>,
}

/// "40ms", "2.4s", "1m 12s".
pub fn took(took: Duration) -> String {
    let label = tool_card::took_label(took);
    label.strip_prefix("Took ").unwrap_or(&label).to_string()
}

/// A spinner while it runs, then a check, a cross, or a dash for one that was cut off.
pub fn state_mark(state: State, cx: &App) -> AnyElement {
    let theme = cx.theme();
    let mark = |text: &'static str, color: Hsla| div().text_xs().text_color(color).child(text).into_any_element();
    match state {
        State::Running => Spinner::new().xsmall().color(theme.primary).into_any_element(),
        State::Done => mark("✓", theme.success),
        State::Failed => mark("✗", theme.danger),
        State::Gone => mark("–", theme.muted_foreground),
    }
}

/// The end of a file: at most [`TAIL_BYTES`], starting on a whole line, and whether the start
/// was left out.
pub fn read_tail(path: &Path) -> std::io::Result<(String, bool)> {
    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    let cut = len > TAIL_BYTES;
    if cut {
        file.seek(SeekFrom::Start(len - TAIL_BYTES))?;
    }
    let mut bytes = Vec::new();
    file.take(TAIL_BYTES).read_to_end(&mut bytes)?;
    let mut text = String::from_utf8_lossy(&bytes).into_owned();
    if cut && let Some(newline) = text.find('\n') {
        text.drain(..=newline);
    }
    Ok((text, cut))
}

/// A background shell's output as last read from its file.
struct Tail {
    file: PathBuf,
    /// The file's size and change time when read, so it's read again only when it changed.
    stamp: Option<(u64, SystemTime)>,
    failed: bool,
    shell: Option<ShellText>,
    /// The start of the output was left out.
    cut: bool,
}

/// One entry of a thread's activity in a dialog, kept up to date while it runs. ← and → step
/// through the others; ↑/↓, Page Up/Down, Space, Home and End scroll.
pub struct ActivityViewer {
    thread: WeakEntity<ThreadView>,
    /// The tool call on show.
    id: String,
    /// Whether it steps through background shells and subagents, or ordinary commands.
    tasks: bool,
    /// The thread's entries of that sort, oldest first, as last read.
    ids: Vec<String>,
    detail: Option<Detail>,
    prompt: Entity<TextViewState>,
    report: Entity<TextViewState>,
    shown_prompt: Option<String>,
    shown_report: Option<String>,
    tail: Option<Tail>,
    scroll: ScrollHandle,
    focus: FocusHandle,
    _observe: Subscription,
    _poll: Task<()>,
}

impl ActivityViewer {
    /// Show `id`, one of `ids` (the thread's entries, oldest first) as `detail` has it. The
    /// thread opens the viewer while it's being updated, so it passes them in.
    pub fn new(
        thread: Entity<ThreadView>,
        id: &str,
        ids: Vec<String>,
        detail: Option<Detail>,
        cx: &mut Context<Self>,
    ) -> Self {
        let observe = cx.observe(&thread, |this, _, cx| this.sync(cx));
        let poll = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(POLL_EVERY).await;
                let alive = this.update(cx, |v: &mut ActivityViewer, cx| {
                    if v.refresh_tail() {
                        cx.notify();
                    }
                });
                if alive.is_err() {
                    break;
                }
            }
        });
        let mut viewer = Self {
            thread: thread.downgrade(),
            id: id.to_string(),
            tasks: detail.as_ref().is_none_or(|d| d.item.kind.is_task()),
            ids: Vec::new(),
            detail: None,
            prompt: cx.new(|cx| TextViewState::markdown("", cx).selectable(true)),
            report: cx.new(|cx| TextViewState::markdown("", cx).selectable(true)),
            shown_prompt: None,
            shown_report: None,
            tail: None,
            scroll: ScrollHandle::new(),
            focus: cx.focus_handle(),
            _observe: observe,
            _poll: poll,
        };
        viewer.apply(ids, detail, cx);
        viewer.start_at_top_or_end();
        viewer
    }

    pub fn focus(&self, window: &mut Window, cx: &mut App) {
        window.focus(&self.focus, cx);
    }

    /// The entry on show.
    pub fn detail(&self) -> Option<&Detail> {
        self.detail.as_ref()
    }

    /// The background shell output on show, as read from its file.
    #[cfg(test)]
    pub fn tail_text(&self) -> Option<String> {
        let shell = self.tail.as_ref()?.shell.as_ref()?;
        Some(shell.output_text())
    }

    /// Where the entry on show is in the thread's list, and how many there are.
    pub fn position(&self) -> (usize, usize) {
        (self.ids.iter().position(|id| *id == self.id).unwrap_or(0), self.ids.len())
    }

    /// Start showing `self.id`.
    fn show(&mut self, cx: &mut Context<Self>) {
        self.tail = None;
        self.sync(cx);
        self.start_at_top_or_end();
    }

    /// At the top, or at the end of a shell still printing.
    fn start_at_top_or_end(&mut self) {
        let running_shell =
            self.detail.as_ref().is_some_and(|d| d.item.kind == Kind::Background && d.item.state == State::Running);
        if running_shell {
            self.scroll.scroll_to_bottom();
        } else {
            self.scroll.set_offset(point(px(0.), px(0.)));
        }
    }

    /// Read the entry from the thread again (it changed, or another one is on show).
    fn sync(&mut self, cx: &mut Context<Self>) {
        let Some(thread) = self.thread.upgrade() else { return };
        let thread = thread.read(cx);
        let ids = thread.activity(self.tasks).into_iter().map(|item| item.id).collect();
        let detail = thread.activity_detail(&self.id);
        self.apply(ids, detail, cx);
    }

    fn apply(&mut self, ids: Vec<String>, detail: Option<Detail>, cx: &mut Context<Self>) {
        self.ids = ids;
        self.detail = detail;
        let (prompt, report) = match &self.detail {
            Some(detail) => (detail.prompt.clone(), detail.report.clone()),
            None => (None, None),
        };
        if prompt != self.shown_prompt {
            self.prompt.update(cx, |t, cx| t.set_text(prompt.as_deref().unwrap_or_default(), cx));
            self.shown_prompt = prompt;
        }
        if report != self.shown_report {
            self.report.update(cx, |t, cx| t.set_text(report.as_deref().unwrap_or_default(), cx));
            self.shown_report = report;
        }
        self.refresh_tail();
        cx.notify();
    }

    /// Read a background shell's output file again if it changed, staying at the end when
    /// that's where the reader was. Says whether anything changed.
    fn refresh_tail(&mut self) -> bool {
        let Some(detail) = self.detail.as_ref().filter(|d| d.item.kind == Kind::Background) else {
            return self.tail.take().is_some();
        };
        let Some(file) = detail.output_file.clone() else { return self.tail.take().is_some() };
        let stamp = std::fs::metadata(&file).ok().map(|m| (m.len(), m.modified().unwrap_or(SystemTime::UNIX_EPOCH)));
        let failed = detail.item.state == State::Failed;
        if self.tail.as_ref().is_some_and(|t| t.file == file && t.stamp == stamp && t.failed == failed) {
            return false;
        }
        let (shell, cut) = match read_tail(&file) {
            Ok((text, cut)) => {
                let mut shell = ShellText::new(&detail.command);
                shell.finish(&text, failed);
                (Some(shell), cut)
            }
            Err(_) => (None, false),
        };
        let max = self.scroll.max_offset().y;
        let at_end = self.scroll.offset().y <= -max + px(4.);
        self.tail = Some(Tail { file, stamp, failed, shell, cut });
        if at_end {
            self.scroll.scroll_to_bottom();
        }
        true
    }

    pub fn step(&mut self, by: isize, cx: &mut Context<Self>) {
        let (ix, count) = self.position();
        let ix = ix as isize + by;
        if ix >= 0 && (ix as usize) < count {
            self.id = self.ids[ix as usize].clone();
            self.show(cx);
        }
    }

    fn scroll_by(&self, dy: Pixels, cx: &mut Context<Self>) {
        let offset = self.scroll.offset();
        let max = self.scroll.max_offset().y;
        let y = (offset.y - dy).clamp(-max, px(0.));
        self.scroll.set_offset(point(offset.x, y));
        cx.notify();
    }

    fn on_key(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        if event.keystroke.modifiers.modified() {
            return;
        }
        let page = self.scroll.bounds().size.height * 0.9;
        let line = px(48.);
        match event.keystroke.key.as_str() {
            "left" => self.step(-1, cx),
            "right" => self.step(1, cx),
            "up" => self.scroll_by(-line, cx),
            "down" => self.scroll_by(line, cx),
            "pageup" => self.scroll_by(-page, cx),
            "pagedown" | "space" => self.scroll_by(page, cx),
            "home" => self.scroll_by(px(-1e7), cx),
            "end" => self.scroll_by(px(1e7), cx),
            _ => return,
        }
        cx.stop_propagation();
    }

    /// One line under the title: where it's at, and how long it took or how it ended.
    fn status_line(&self, detail: &Detail, cx: &App) -> AnyElement {
        let theme = cx.theme();
        let item = &detail.item;
        let text = match (item.kind, item.state) {
            (Kind::Shell, State::Running) => "Running…".to_string(),
            (Kind::Shell, State::Failed) => "Failed".to_string(),
            (Kind::Shell, _) => "Finished".to_string(),
            (Kind::Background, State::Running) => "Running in the background".to_string(),
            (Kind::Subagent, State::Running) => detail.now.clone().unwrap_or_else(|| "Working…".to_string()),
            (_, State::Failed) => detail.ended.clone().unwrap_or_else(|| "Failed".to_string()),
            _ => detail.ended.clone().unwrap_or_else(|| "Finished".to_string()),
        };
        let color = match item.state {
            State::Running => theme.primary,
            State::Failed => theme.danger,
            _ => theme.muted_foreground,
        };
        h_flex()
            .gap_2()
            .min_w_0()
            .text_sm()
            .child(state_mark(item.state, cx))
            .child(div().min_w_0().text_color(color).child(text))
            .children(self.status_note(detail).map(|note| {
                div().flex_shrink_0().text_xs().text_color(theme.muted_foreground).child(format!("· {note}"))
            }))
            .into_any_element()
    }

    /// The list's note, less what the title already says: a subagent's type, "background".
    fn status_note(&self, detail: &Detail) -> Option<String> {
        let note = detail.item.note.as_deref()?;
        let note = match (detail.item.kind, detail.agent_type.as_deref()) {
            (Kind::Background, _) => return None,
            (Kind::Subagent, Some(agent_type)) => {
                note.strip_prefix(agent_type).map(|rest| rest.trim_start_matches(" · ")).unwrap_or(note)
            }
            _ => note,
        };
        (!note.is_empty()).then(|| note.to_string())
    }

    fn body(&self, detail: &Detail, cx: &App) -> AnyElement {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let label = |text: &'static str| {
            div().text_xs().font_weight(FontWeight::SEMIBOLD).text_color(muted).child(text).into_any_element()
        };
        let note = |text: String| div().text_xs().text_color(muted).child(text).into_any_element();
        match detail.item.kind {
            Kind::Shell => {
                let shell = detail.shell.clone().unwrap_or_else(|| ShellText::new(&detail.command));
                tool_card::shell_panel(&shell, detail.took, cx)
            }
            Kind::Background => {
                let tail = self.tail.as_ref();
                let shell = tail.and_then(|t| t.shell.clone()).unwrap_or_else(|| ShellText::new(&detail.command));
                let problem = match (&detail.output_file, tail) {
                    (Some(file), Some(Tail { shell: None, .. })) => {
                        Some(format!("Its output file is gone: {}", file.display()))
                    }
                    (None, _) => detail.output.clone().filter(|_| detail.item.state == State::Failed),
                    _ => None,
                };
                v_flex()
                    .gap_2()
                    .when(tail.is_some_and(|t| t.cut), |el| {
                        el.child(note(format!("Showing the last {} KB of its output.", TAIL_BYTES / 1024)))
                    })
                    .child(tool_card::shell_panel(&shell, None, cx))
                    .children(problem.map(|p| div().text_sm().text_color(theme.danger).child(p)))
                    .children(detail.output_file.as_ref().map(|file| note(format!("Output file: {}", file.display()))))
                    .into_any_element()
            }
            Kind::Subagent => v_flex()
                .gap_3()
                .when(detail.prompt.is_some(), |el| {
                    el.child(
                        v_flex().gap_1().child(label("PROMPT")).child(TextView::new(&self.prompt).selectable(true)),
                    )
                })
                .when(!detail.log.is_empty(), |el| {
                    el.child(v_flex().gap_1().child(label("STEPS")).children(detail.log.iter().map(|line| {
                        div()
                            .text_xs()
                            .font_family(theme.mono_font_family.clone())
                            .text_color(muted)
                            .child(line.clone())
                    })))
                })
                .child(
                    v_flex().gap_1().child(label("REPORT")).child(match detail.report {
                        Some(_) => TextView::new(&self.report).selectable(true).into_any_element(),
                        None => div()
                            .text_sm()
                            .italic()
                            .text_color(muted)
                            .child(if detail.item.state == State::Running { "Not done yet." } else { "No report." })
                            .into_any_element(),
                    }),
                )
                .into_any_element(),
        }
    }
}

impl Render for ActivityViewer {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (ix, count) = self.position();
        let navigator = (count > 1).then(|| {
            let (back, next) = (cx.entity().downgrade(), cx.entity().downgrade());
            h_flex()
                .gap_1()
                .items_center()
                .child(
                    Button::new("activity-previous")
                        .xsmall()
                        .ghost()
                        .icon(IconName::ChevronLeft)
                        .tooltip("Previous (←)")
                        .disabled(ix == 0)
                        .on_click(move |_, _, cx| {
                            back.update(cx, |v, cx| v.step(-1, cx)).ok();
                        }),
                )
                .child(div().text_xs().text_color(theme.muted_foreground).child(format!("{} of {count}", ix + 1)))
                .child(
                    Button::new("activity-next")
                        .xsmall()
                        .ghost()
                        .icon(IconName::ChevronRight)
                        .tooltip("Next (→)")
                        .disabled(ix + 1 >= count)
                        .on_click(move |_, _, cx| {
                            next.update(cx, |v, cx| v.step(1, cx)).ok();
                        }),
                )
        });
        let (status, body) = match &self.detail {
            Some(detail) => (Some(self.status_line(detail, cx)), self.body(detail, cx)),
            None => (
                None,
                div()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child("This call is no longer in the thread.")
                    .into_any_element(),
            ),
        };
        v_flex()
            .id("activity-viewer")
            .test_support()
            .size_full()
            .min_h_0()
            .gap_2()
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::on_key))
            .child(h_flex().gap_2().child(div().flex_1().min_w_0().children(status)).children(navigator))
            .child(
                div()
                    .id("activity-viewer-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .pr_2()
                    .child(body),
            )
    }
}

/// The dialog around an [`ActivityViewer`]: what kind of entry it is and its title on top, as
/// tall as the window allows and wide enough for a terminal's lines.
pub fn dialog(dialog: Dialog, viewer: &Entity<ActivityViewer>, window: &mut Window, cx: &mut App) -> Dialog {
    let size = window.viewport_size();
    let gap = cx.theme().spacing_tokens().lg;
    let muted = cx.theme().muted_foreground;
    let (kind, title) = match viewer.read(cx).detail() {
        Some(detail) => {
            let label = match &detail.agent_type {
                Some(agent_type) if detail.item.kind == Kind::Subagent => format!("Subagent · {agent_type}"),
                _ => detail.item.kind.label().to_string(),
            };
            (Some((detail.item.kind.icon(), label)), detail.item.title.clone())
        }
        None => (None, String::new()),
    };
    let title = h_flex()
        .gap_2()
        .min_w_0()
        .children(kind.map(|(icon, label)| {
            h_flex()
                .flex_shrink_0()
                .gap_2()
                .child(Icon::new(icon).small())
                .child(div().font_weight(FontWeight::SEMIBOLD).child(label))
        }))
        .child(div().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().text_color(muted).child(title));
    dialog
        .w((size.width * 0.85).min(px(1400.)))
        .margin_top(gap)
        .h(size.height - gap * 2.)
        .title(title)
        .child(div().size_full().min_h_0().child(viewer.clone()))
}

#[cfg(test)]
mod tests {
    use super::{TAIL_BYTES, read_tail, took};
    use std::time::Duration;

    #[test]
    fn took_drops_the_word() {
        assert_eq!(took(Duration::from_millis(2400)), "2.4s");
        assert_eq!(took(Duration::from_secs(72)), "1m 12s");
    }

    #[test]
    fn a_long_output_file_is_read_from_its_end_on_a_whole_line() {
        let dir = std::env::temp_dir().join(format!("tj-activity-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let short = dir.join("short.output");
        std::fs::write(&short, "tick 1\ntick 2\n").unwrap();
        assert_eq!(read_tail(&short).unwrap(), ("tick 1\ntick 2\n".to_string(), false));

        let long = dir.join("long.output");
        let line = "x".repeat(99) + "\n";
        let text = line.repeat((TAIL_BYTES as usize / line.len()) + 50) + "last\n";
        std::fs::write(&long, &text).unwrap();
        let (tail, cut) = read_tail(&long).unwrap();
        assert!(cut);
        assert!(tail.len() <= TAIL_BYTES as usize);
        assert!(tail.starts_with(&line), "starts on a whole line");
        assert!(tail.ends_with("last\n"));

        assert!(read_tail(&dir.join("gone.output")).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }
}
