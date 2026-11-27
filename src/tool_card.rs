//! How a tool call looks in the transcript. Each tool has an icon, and its family (shell,
//! reading, editing, web, ...) gives it a colour from the theme's terminal palette, so cards
//! follow the Omarchy theme. The header shows the tool, a one-line summary, a button that adds
//! the call to the prompt, and a spinner, check or cross; the body is shaped for the tool (a
//! little terminal for Bash, a diff for Edit) and scrolls once it reaches half the window. A
//! Bash card is cut off at a fixed height and shows how long the command took; the rest opens
//! in a dialog.

use std::{cell::Cell, path::Path, rc::Rc, sync::Arc, time::Duration};

use gpui_kit::assets::IconName;
use gpui_kit::base::{h_flex, v_flex};
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, dialog::Dialog, spinner::Spinner};
use gpui_kit::{TestSupportExt as _, prelude::FluentBuilder as _, *};
use serde_json::Value;

use crate::highlight::{Highlighted, Palette};

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ToolStatus {
    Running,
    Done,
    Failed,
}

/// What kind of work a tool does; each family has one colour.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Family {
    /// Drawn as a small grey terminal rather than in a palette colour.
    Shell,
    Read,
    Write,
    Web,
    Agents,
    Planning,
    Scheduling,
    Workspace,
    Other,
}

impl Family {
    /// The ANSI palette slot the family takes its colour from.
    fn ansi(self) -> usize {
        match self {
            Family::Shell => 7,
            Family::Read => 4,
            Family::Write => 3,
            Family::Web => 6,
            Family::Agents => 5,
            Family::Planning => 13,
            Family::Scheduling => 11,
            Family::Workspace => 12,
            Family::Other => 14,
        }
    }
}

/// A tool's family and icon.
pub fn kind(name: &str) -> (Family, IconName) {
    use Family::*;
    match name {
        "Bash" | "BashOutput" | "KillShell" | "KillBash" => (Shell, IconName::SquareTerminal),
        "Monitor" => (Shell, IconName::Activity),
        "TaskStop" => (Shell, IconName::CircleStop),
        "Read" => (Read, IconName::FileText),
        "Grep" => (Read, IconName::SearchCode),
        "Glob" => (Read, IconName::FolderSearch),
        "LSP" => (Read, IconName::Braces),
        "ToolSearch" => (Read, IconName::PackageSearch),
        "Edit" => (Write, IconName::FilePen),
        "MultiEdit" => (Write, IconName::Files),
        "Write" => (Write, IconName::FilePlus),
        "NotebookEdit" => (Write, IconName::NotebookPen),
        "WebSearch" => (Web, IconName::Globe),
        "WebFetch" => (Web, IconName::Link),
        "Agent" | "Task" => (Agents, IconName::Bot),
        "SendMessage" => (Agents, IconName::Send),
        "ListAgents" => (Agents, IconName::Users),
        "Workflow" => (Agents, IconName::Workflow),
        "McpTaskCheck" => (Agents, IconName::ListChecks),
        "EnterPlanMode" | "ExitPlanMode" => (Planning, IconName::Map),
        "TodoWrite" => (Planning, IconName::ListTodo),
        "CronCreate" => (Scheduling, IconName::CalendarPlus),
        "CronDelete" => (Scheduling, IconName::CalendarX),
        "CronList" => (Scheduling, IconName::CalendarClock),
        "ScheduleWakeup" => (Scheduling, IconName::AlarmClock),
        "RemoteTrigger" => (Scheduling, IconName::Zap),
        "PushNotification" => (Scheduling, IconName::BellRing),
        "EnterWorktree" => (Workspace, IconName::GitBranchPlus),
        "ExitWorktree" => (Workspace, IconName::GitBranchMinus),
        "SendToProject" => (Workspace, IconName::FolderOutput),
        "SendToProjectContext" => (Workspace, IconName::FolderInput),
        "Skill" => (Other, IconName::Sparkles),
        "DesignSync" => (Other, IconName::Palette),
        "ReportFindings" => (Other, IconName::Bug),
        n if n.starts_with("mcp__") => (Other, IconName::Plug),
        _ => (Other, IconName::Wrench),
    }
}

/// The name shown in the header: MCP tools (`mcp__server__tool`) read "server · tool".
pub fn title(name: &str) -> String {
    let Some(rest) = name.strip_prefix("mcp__") else { return name.to_string() };
    let (server, tool) = rest.split_once("__").unwrap_or(("mcp", rest));
    // claude.ai connectors all carry the same prefix; the rest is the service's name.
    let server = server.strip_prefix("claude_ai_").unwrap_or(server).replace('_', " ");
    format!("{server} · {}", tool.replace('_', " "))
}

/// Cards whose body is worth watching start open: commands and edits.
pub fn opens_by_default(name: &str) -> bool {
    matches!(name, "Bash" | "Edit" | "MultiEdit" | "Write" | "NotebookEdit")
}

/// The header's one line: Bash's own description of its command, a path relative to the
/// project, and the line range for Read. Falls back to `summary`.
pub fn header_summary(name: &str, input: &Value, summary: &str, project: &Path) -> String {
    let field = |key: &str| input.get(key).and_then(Value::as_str).filter(|s| !s.trim().is_empty());
    let relative = |path: &str| {
        Path::new(path).strip_prefix(project).map(|p| p.display().to_string()).unwrap_or_else(|_| path.to_string())
    };
    match name {
        "Bash" => field("description").map(str::to_owned).unwrap_or_else(|| first_line(summary)),
        "Read" => {
            let path = field("file_path").map(relative).unwrap_or_else(|| summary.to_string());
            let offset = input.get("offset").and_then(Value::as_u64);
            let limit = input.get("limit").and_then(Value::as_u64);
            match (offset, limit) {
                (Some(o), Some(l)) => format!("{path} · lines {o}–{}", o + l.saturating_sub(1)),
                (Some(o), None) => format!("{path} · from line {o}"),
                (None, Some(l)) => format!("{path} · first {l} lines"),
                (None, None) => path,
            }
        }
        "Write" | "Edit" | "MultiEdit" => field("file_path").map(relative).unwrap_or_else(|| summary.to_string()),
        "NotebookEdit" => field("notebook_path").map(relative).unwrap_or_else(|| summary.to_string()),
        "Grep" | "Glob" => match field("path") {
            Some(path) => format!("{summary} in {}", relative(path)),
            None => summary.to_string(),
        },
        // Tools without a summary of their own: their argument values, not raw JSON.
        _ if summary.trim_start().starts_with('{') => {
            let values: Vec<String> = input
                .as_object()
                .into_iter()
                .flat_map(|m| m.values())
                .filter_map(|v| match v {
                    Value::String(s) if !s.trim().is_empty() => Some(first_line(s)),
                    Value::Number(n) => Some(n.to_string()),
                    _ => None,
                })
                .collect();
            if values.is_empty() { first_line(summary) } else { values.join(" · ") }
        }
        _ => first_line(summary),
    }
}

/// Everything the card shows; the thread view fills it in from its tool row.
pub struct Card<'a> {
    pub ix: usize,
    pub name: &'a str,
    pub input: &'a Value,
    pub summary: String,
    pub status: ToolStatus,
    pub output: Option<&'a str>,
    pub pictures: &'a [Picture],
    /// Extra words before the status mark, e.g. a subagent's "3 tools · 12k tokens".
    pub label: Option<String>,
    /// A running subagent's current step.
    pub now: Option<String>,
    pub expanded: bool,
    /// Replaces the tool-shaped body (a subagent's log and report).
    pub details: Option<AnyElement>,
    /// A Bash call's command and output, coloured.
    pub shell: Option<&'a ShellText>,
    /// How long the call took, once it's known.
    pub took: Option<Duration>,
    /// The body's scroll position.
    pub scroll: &'a ScrollHandle,
}

/// The tallest a Bash card's command and output are drawn; the rest opens in a dialog. The
/// height is what counts, since one long line can wrap many times.
const SHELL_MAX_HEIGHT: Pixels = px(300.);
/// The most lines of command and output a Bash card lays out. More than `SHELL_MAX_HEIGHT`
/// holds; it only spares laying out text that's cut off anyway.
pub const SHELL_LINES: usize = 30;
/// Lines kept for the output when the command alone would fill the card.
const SHELL_OUTPUT_LINES: usize = 10;
/// The most characters of the command, and of the output, a Bash card lays out, for the same
/// reason: a line of thousands of characters wraps far past `SHELL_MAX_HEIGHT`.
const SHELL_CHARS: usize = 4_000;

/// A Bash call's command and what it printed, coloured once and kept with the row.
#[derive(Clone)]
pub struct ShellText {
    command: Highlighted,
    output: Option<Highlighted>,
    /// What the card lays out when the whole text is longer than `SHELL_LINES` or `SHELL_CHARS`.
    short: Option<Short>,
    /// Whether the text, as last drawn, was taller than `SHELL_MAX_HEIGHT`. Shared with the
    /// card's drawing, which measures it.
    overflows: Rc<Cell<bool>>,
    failed: bool,
}

/// The first lines of a long command and its output.
#[derive(Clone)]
struct Short {
    command: Highlighted,
    command_cut: bool,
    output: Option<Highlighted>,
}

impl ShellText {
    pub fn new(command: &str) -> Self {
        let mut shell = ShellText {
            command: Highlighted::command(clean(command)),
            output: None,
            short: None,
            overflows: Rc::default(),
            failed: false,
        };
        shell.shorten();
        shell
    }

    pub fn finish(&mut self, output: &str, failed: bool) {
        let output = clean(output);
        self.output = (!output.trim().is_empty()).then(|| Highlighted::output(output));
        self.failed = failed;
        self.shorten();
    }

    /// Whether the card leaves some of the text out, so it opens in the dialog.
    pub fn is_cut(&self) -> bool {
        self.short.is_some() || self.overflows.get()
    }

    fn shorten(&mut self) {
        let command = self.command.text.as_ref();
        let output = self.output.as_ref().map(|o| o.text.as_ref()).unwrap_or_default();
        let (c, o) = (command.lines().count(), output.lines().count());
        let too_long = c + o > SHELL_LINES || command.len() > SHELL_CHARS || output.len() > SHELL_CHARS;
        let short = too_long.then(|| {
            // A long script leaves room for the start of what it printed.
            let keep_c = c.min(SHELL_LINES - o.min(SHELL_OUTPUT_LINES));
            let keep_o = o.min(SHELL_LINES - keep_c);
            let kept_command = first_chars(first_lines(command, keep_c), SHELL_CHARS);
            Short {
                command_cut: kept_command.len() < command.trim_end().len(),
                command: Highlighted::command(kept_command),
                output: (keep_o > 0)
                    .then(|| Highlighted::output(first_chars(first_lines(output, keep_o), SHELL_CHARS))),
            }
        });
        self.short = short;
    }
}

fn first_lines(text: &str, count: usize) -> String {
    text.lines().take(count).collect::<Vec<_>>().join("\n")
}

/// At most `max` bytes of `text`, cut at a character boundary.
fn first_chars(mut text: String, max: usize) -> String {
    if text.len() > max {
        text.truncate(text.floor_char_boundary(max));
    }
    text
}

/// "Took 40ms", "Took 2.4s", "Took 1m 12s", "Took 1h 3m".
pub fn took_label(took: Duration) -> String {
    let secs = took.as_secs();
    match secs {
        0 if took.as_millis() < 100 => format!("Took {}ms", took.as_millis()),
        0..60 => format!("Took {:.1}s", took.as_secs_f32()),
        60..3600 => format!("Took {}m {}s", secs / 60, secs % 60),
        _ => format!("Took {}h {}m", secs / 3600, secs % 3600 / 60),
    }
}

/// What the prompt carries when a card is added to it: which call the user is asking about.
/// Claude already has the call and its output, so the command is enough to point at it.
pub fn quote(name: &str, input: &Value, summary: &str) -> String {
    let what = match summary.trim() {
        "" => format!("About your earlier {} call:", title(name)),
        summary => format!("About your earlier {} call ({summary}):", title(name)),
    };
    match input.get("command").and_then(Value::as_str).filter(|_| name == "Bash") {
        Some(command) => {
            let command = clean(command);
            let cut = command.lines().count() > QUOTE_LINES;
            let shown = first_lines(&command, QUOTE_LINES);
            format!("{what}\n```\n$ {shown}{}\n```", if cut { "\n…" } else { "" })
        }
        None => what,
    }
}

/// The most lines of a command a quote repeats.
const QUOTE_LINES: usize = 12;

/// A picture a tool returned, with its size so small ones aren't blown up.
pub struct Picture {
    image: Arc<Image>,
    width: u32,
    height: u32,
    format: &'static str,
}

/// A picture from its bytes; `None` when they aren't an image TJ can read.
pub fn picture(media_type: &str, bytes: Vec<u8>) -> Option<Picture> {
    let (width, height) =
        image::ImageReader::new(std::io::Cursor::new(&bytes)).with_guessed_format().ok()?.into_dimensions().ok()?;
    let (format, label) = match media_type {
        "image/jpeg" | "image/jpg" => (ImageFormat::Jpeg, "JPEG"),
        "image/gif" => (ImageFormat::Gif, "GIF"),
        "image/webp" => (ImageFormat::Webp, "WebP"),
        _ => (ImageFormat::Png, "PNG"),
    };
    Some(Picture { image: Arc::new(Image::from_bytes(format, bytes)), width, height, format: label })
}

/// The tallest a picture is drawn; larger ones are scaled down to fit.
const PICTURE_MAX_HEIGHT: f32 = 360.0;

fn picture_block(picture: &Picture, look: &Look) -> AnyElement {
    let (w, h) = (picture.width.max(1) as f32, picture.height.max(1) as f32);
    let height = h.min(PICTURE_MAX_HEIGHT);
    let width = w * height / h;
    v_flex()
        .gap_1()
        .child(
            div().max_w_full().child(
                img(picture.image.clone())
                    .w(px(width))
                    .h(px(height))
                    .max_w_full()
                    .object_fit(ObjectFit::Contain)
                    .rounded_sm(),
            ),
        )
        .child(div().text_color(look.dim).child(format!("{} × {} · {}", picture.width, picture.height, picture.format)))
        .into_any_element()
}

/// The colours a card is drawn in.
struct Look {
    /// The colours for highlighted text.
    palette: Palette,
    accent: Hsla,
    /// The header's summary line.
    subtle: Hsla,
    header_bg: Hsla,
    body_bg: Option<Hsla>,
    border: Hsla,
    text: Hsla,
    dim: Hsla,
    removed: Hsla,
    added: Hsla,
    error: Hsla,
}

fn look(family: Family, cx: &App) -> Look {
    let (ansi, _, term_bg) = crate::terminal::palette(cx);
    let theme = cx.theme();
    if family == Family::Shell {
        // Grey on near-black, like a small terminal: the theme's terminal background when it's
        // dark (made greyer and darker), otherwise plain near-black.
        let dark = term_bg.l < 0.3;
        let bg =
            if dark { hsla(term_bg.h, term_bg.s.min(0.15), term_bg.l * 0.7, 1.0) } else { hsla(0.0, 0.0, 0.08, 1.0) };
        let grey = if dark && ansi[7].l > 0.55 { ansi[7] } else { hsla(0.0, 0.0, 0.78, 1.0) };
        let text = hsla(grey.h, grey.s.min(0.12), grey.l, 1.0);
        // A light theme's terminal colours are made for a light background; on near-black the
        // built-in ones read better.
        let ansi = if dark { ansi } else { crate::terminal::default_ansi() };
        return Look {
            palette: Palette { ansi, text, dim: text.opacity(0.72) },
            accent: text,
            subtle: text.opacity(0.72),
            header_bg: hsla(bg.h, bg.s, (bg.l + 0.035).min(1.0), 1.0),
            body_bg: Some(bg),
            border: text.opacity(0.18),
            text,
            dim: text.opacity(0.72),
            removed: ansi[1],
            added: ansi[2],
            error: ansi[9],
        };
    }
    let accent = ansi[family.ansi()];
    Look {
        palette: Palette { ansi, text: theme.foreground, dim: theme.muted_foreground },
        accent,
        subtle: theme.foreground.opacity(0.68),
        header_bg: accent.opacity(0.08),
        body_bg: None,
        border: accent.opacity(0.3),
        text: theme.foreground,
        dim: theme.muted_foreground,
        removed: theme.danger,
        added: theme.success,
        error: theme.danger,
    }
}

/// The most of a tool's output a card holds; Claude Code already trims long command output.
const OUTPUT_LIMIT: usize = 60_000;
/// The most diff or file lines a card draws one by one.
const LINE_LIMIT: usize = 600;

/// The corner radius of a card's parts, inside its one-pixel border.
const INNER_RADIUS: Pixels = px(5.);

/// What a card's clicks do.
pub struct Actions<T, E, Q> {
    /// The header of any card but a Bash one: open or fold the card.
    pub toggle: T,
    /// "Click to expand" over the bottom of a cut-off Bash card: open all of it in a dialog.
    pub expand: E,
    /// The header's button: add the call to the prompt, to ask about it.
    pub quote: Q,
}

pub fn render<T, E, Q>(card: Card, window: &Window, cx: &App, actions: Actions<T, E, Q>) -> AnyElement
where
    T: Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    E: Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    Q: Fn(&ClickEvent, &mut Window, &mut App) + 'static,
{
    let Actions { toggle, expand, quote } = actions;
    let (family, icon) = kind(card.name);
    let look = look(family, cx);
    let theme = cx.theme();
    let mono = theme.mono_font_family.clone();
    let (success, danger) = (theme.success, theme.danger);

    let mark = match card.status {
        ToolStatus::Running => Spinner::new().xsmall().color(look.accent).into_any_element(),
        ToolStatus::Done => Icon::new(IconName::Check).xsmall().text_color(success).into_any_element(),
        ToolStatus::Failed => Icon::new(IconName::X).xsmall().text_color(danger).into_any_element(),
    };
    let header = h_flex()
        .id(SharedString::from(format!("tool-{}", card.ix)))
        .test_support()
        .gap_2()
        .px_3()
        .py_1p5()
        .bg(look.header_bg)
        // A Bash card stays open: its body is short, and the rest is in the dialog.
        .when(card.shell.is_none(), |el| el.cursor_pointer().on_click(toggle))
        .child(Icon::new(icon).small().text_color(look.accent))
        .child(
            div()
                .flex_shrink_0()
                .text_sm()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(if family == Family::Shell { look.text } else { theme.foreground })
                .child(title(card.name)),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .whitespace_nowrap()
                .text_ellipsis()
                .text_sm()
                .text_color(look.subtle)
                .child(card.summary.clone()),
        )
        .when_some(card.label.clone(), |el, label| el.child(div().text_xs().text_color(look.dim).child(label)))
        .child(
            div()
                .id(SharedString::from(format!("tool-ask-{}", card.ix)))
                .test_support()
                .flex_shrink_0()
                .rounded_sm()
                .p_0p5()
                .cursor_pointer()
                .hover(|el| el.bg(look.text.opacity(0.12)))
                .tooltip(|window, cx| {
                    gpui_kit::component::tooltip::Tooltip::new("Add to the prompt, to ask about it").build(window, cx)
                })
                .on_click(move |event, window, cx| {
                    cx.stop_propagation();
                    quote(event, window, cx);
                })
                .child(Icon::new(IconName::MessageSquarePlus).xsmall().text_color(look.subtle)),
        )
        .child(div().flex_shrink_0().child(mark));

    let body = if !card.expanded {
        None
    } else if let Some(details) = card.details {
        Some(details)
    } else if let Some(shell) = card.shell {
        Some(shell_card_body(&card, shell, &look, expand))
    } else {
        tool_body(&card, &look, mono.clone())
    };
    // The border's corners are round, so the parts that touch them are too.
    let header_ends = card.now.is_none() && body.is_none();

    v_flex()
        .rounded_md()
        .border_1()
        .border_color(look.border)
        .child(header.rounded_t(INNER_RADIUS).when(header_ends, |el| el.rounded_b(INNER_RADIUS)))
        .when_some(card.now.clone(), |el, now| {
            el.child(
                div()
                    .px_3()
                    .pb_1p5()
                    .bg(look.header_bg)
                    .when(body.is_none(), |el| el.rounded_b(INNER_RADIUS))
                    .text_xs()
                    .text_color(look.dim)
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .child(format!("↳ {now}")),
            )
        })
        .when_some(body, |el, body| {
            el.child(
                div()
                    .id(SharedString::from(format!("tool-body-{}", card.ix)))
                    .border_t_1()
                    .border_color(look.border)
                    .rounded_b(INNER_RADIUS)
                    .when_some(look.body_bg, |el, bg| el.bg(bg))
                    // A Bash card is already cut off at its own height, and opens the rest in a
                    // dialog.
                    .when(card.shell.is_none(), |el| {
                        el.max_h(window.viewport_size().height * 0.5).overflow_y_scroll().track_scroll(card.scroll)
                    })
                    .px_3()
                    .py_2()
                    .text_xs()
                    .font_family(mono)
                    .child(body),
            )
        })
        .into_any_element()
}

/// A Bash card's body: the command and its output cut off at `SHELL_MAX_HEIGHT`, with a
/// "Click to expand" over the bottom when there's more, then any pictures, and how long the
/// command took.
fn shell_card_body(
    card: &Card,
    shell: &ShellText,
    look: &Look,
    on_expand: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    let text = match &shell.short {
        Some(short) => shell_body(&short.command, short.command_cut, short.output.as_ref(), shell.failed, look),
        None => shell_body(&shell.command, false, shell.output.as_ref(), shell.failed, look),
    };
    // Whether the text is taller than the card is only known once it's laid out, so the
    // drawing measures it and, when that changes, draws the window again. The "Click to
    // expand" lies over the text, so the card's height doesn't change with it.
    let overflows = shell.overflows.clone();
    let measure = move |children: Vec<Bounds<Pixels>>, window: &mut Window, cx: &mut App| {
        let height = children.iter().map(|b| b.size.height).fold(px(0.), |a, b| a.max(b));
        let over = height > SHELL_MAX_HEIGHT + px(0.5);
        if overflows.replace(over) != over {
            window.defer(cx, |window, _| window.refresh());
        }
    };
    let bg = look.body_bg.unwrap_or(look.header_bg);
    let hover = look.text;
    let clipped = div()
        .id(SharedString::from(format!("tool-expand-{}", card.ix)))
        .test_support()
        .relative()
        .max_h(SHELL_MAX_HEIGHT)
        .overflow_hidden()
        .child(div().on_children_prepainted(measure).child(text))
        .when(shell.is_cut(), |el| {
            el.cursor_pointer().on_click(on_expand).child(
                h_flex()
                    .absolute()
                    .bottom_0()
                    .left_0()
                    .right_0()
                    .h_10()
                    .items_end()
                    .justify_center()
                    .bg(linear_gradient(180., linear_color_stop(bg.opacity(0.), 0.), linear_color_stop(bg, 0.7)))
                    .text_color(look.dim)
                    .hover(move |el| el.text_color(hover))
                    .child("Click to expand"),
            )
        });
    v_flex()
        .gap_2()
        .child(clipped)
        .children(card.pictures.iter().map(|p| picture_block(p, look)))
        .when_some(card.took, |el, took| el.child(div().text_color(look.dim.opacity(0.8)).child(took_label(took))))
        .into_any_element()
}

/// The body shaped for the tool, or `None` when there's nothing to show yet.
fn tool_body(card: &Card, look: &Look, mono: SharedString) -> Option<AnyElement> {
    let input = card.input;
    let str_field = |key: &str| input.get(key).and_then(Value::as_str);
    let failed = card.status == ToolStatus::Failed;
    let output = card.output.map(clean).filter(|o| !o.trim().is_empty());
    let output_block = |text: String, color: Hsla| div().text_color(color).child(text).into_any_element();
    // Edits and writes only show their result when it went wrong; "file updated" adds nothing.
    let error_only = |out: Option<String>| out.filter(|_| failed).map(|o| output_block(o, look.error));

    let parts: Vec<AnyElement> = match card.name {
        "Edit" => {
            let old = str_field("old_string").unwrap_or_default();
            let new = str_field("new_string").unwrap_or_default();
            let mut parts = vec![diff_block(old, new, look)];
            if input.get("replace_all").and_then(Value::as_bool) == Some(true) {
                parts.push(div().text_color(look.dim).child("every occurrence").into_any_element());
            }
            parts.extend(error_only(output));
            parts
        }
        "MultiEdit" => {
            let edits = input.get("edits").and_then(Value::as_array).cloned().unwrap_or_default();
            let mut parts: Vec<AnyElement> = edits
                .iter()
                .map(|e| {
                    let get = |k: &str| e.get(k).and_then(Value::as_str).unwrap_or_default();
                    diff_block(get("old_string"), get("new_string"), look)
                })
                .collect();
            parts.extend(error_only(output));
            parts
        }
        "Write" | "NotebookEdit" => {
            let content = clean(str_field("content").or(str_field("new_source")).unwrap_or_default());
            let mut parts = vec![numbered(&content, look)];
            parts.extend(error_only(output));
            parts
        }
        "TodoWrite" => {
            let todos = input.get("todos").and_then(Value::as_array).cloned().unwrap_or_default();
            vec![
                v_flex()
                    .font_family(mono)
                    .children(todos.iter().map(|t| {
                        let status = t.get("status").and_then(Value::as_str).unwrap_or_default();
                        let text = t.get("content").and_then(Value::as_str).unwrap_or_default().to_string();
                        let (mark, color) = match status {
                            "completed" => ("☑", look.dim),
                            "in_progress" => ("◐", look.accent),
                            _ => ("☐", look.text),
                        };
                        h_flex().gap_2().items_start().text_color(color).child(mark).child(
                            div().flex_1().min_w_0().when(status == "completed", |el| el.line_through()).child(text),
                        )
                    }))
                    .into_any_element(),
            ]
        }
        "Read" => output.map(|o| output_block(o, if failed { look.error } else { look.dim })).into_iter().collect(),
        _ => {
            // Anything else: its arguments, then what came back.
            let mut parts = Vec::new();
            if let Some(args) = arguments(input) {
                parts.push(div().text_color(look.text).child(args).into_any_element());
            }
            if let Some(out) = output {
                parts.push(output_block(pretty_json(&out), if failed { look.error } else { look.dim }));
            }
            parts
        }
    };
    let mut parts = parts;
    parts.extend(card.pictures.iter().map(|p| picture_block(p, look)));
    if parts.is_empty() {
        return None;
    }
    let count = parts.len();
    Some(
        v_flex()
            .gap_2()
            .children(parts.into_iter().enumerate().map(|(i, part)| {
                div().when(i + 1 < count, |el| el.pb_2().border_b_1().border_color(look.border)).child(part)
            }))
            .into_any_element(),
    )
}

/// A command after a `$`, then what it printed, both coloured. `command_cut` marks a command
/// whose last lines are left out.
fn shell_body(
    command: &Highlighted,
    command_cut: bool,
    output: Option<&Highlighted>,
    failed: bool,
    look: &Look,
) -> AnyElement {
    v_flex()
        .gap_2()
        .child(
            h_flex().items_start().gap_2().child(div().flex_shrink_0().text_color(look.dim).child("$")).child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .text_color(look.text)
                    .child(command.styled(&look.palette))
                    .when(command_cut, |el| el.child(div().text_color(look.dim).child("…"))),
            ),
        )
        .when_some(output, |el, output| {
            el.child(div().text_color(if failed { look.error } else { look.dim }).child(output.styled(&look.palette)))
        })
        .into_any_element()
}

/// A Bash card opened in a dialog: the whole command and output, scrolling inside it.
pub fn shell_dialog(
    dialog: Dialog,
    summary: &str,
    shell: &ShellText,
    took: Option<Duration>,
    window: &mut Window,
    cx: &mut App,
) -> Dialog {
    let look = look(Family::Shell, cx);
    let theme = cx.theme();
    let width = (window.viewport_size().width * 0.85).min(px(1400.));
    let title = h_flex()
        .gap_2()
        .min_w_0()
        .child(Icon::new(IconName::SquareTerminal).small())
        .child(div().flex_shrink_0().font_weight(FontWeight::SEMIBOLD).child("Bash"))
        .child(
            div()
                .min_w_0()
                .overflow_hidden()
                .whitespace_nowrap()
                .text_ellipsis()
                .text_color(theme.muted_foreground)
                .child(summary.to_string()),
        );
    dialog.w(width).title(title).child(
        div()
            .rounded_md()
            .border_1()
            .border_color(look.border)
            .when_some(look.body_bg, |el, bg| el.bg(bg))
            .p_4()
            .text_sm()
            .font_family(theme.mono_font_family.clone())
            .child(shell_body(&shell.command, false, shell.output.as_ref(), shell.failed, &look))
            .when_some(took, |el, took| {
                el.child(div().pt_2().text_color(look.dim.opacity(0.8)).child(took_label(took)))
            }),
    )
}

/// Old lines in red with `-`, new ones in green with `+`, unchanged ones plain.
fn diff_block(old: &str, new: &str, look: &Look) -> AnyElement {
    let lines = line_diff(&clean(old), &clean(new));
    let shown = lines.len().min(LINE_LIMIT);
    v_flex()
        .children(lines.iter().take(shown).map(|(op, text)| {
            let (sign, color, bg) = match op {
                Diff::Removed => ("-", look.removed, Some(look.removed.opacity(0.1))),
                Diff::Added => ("+", look.added, Some(look.added.opacity(0.1))),
                Diff::Same => (" ", look.dim, None),
            };
            h_flex()
                .items_start()
                .gap_2()
                .px_1()
                .when_some(bg, |el, bg| el.bg(bg))
                .text_color(color)
                .child(div().flex_shrink_0().child(sign))
                .child(div().flex_1().min_w_0().child(if text.is_empty() { " ".to_string() } else { text.clone() }))
        }))
        .when(lines.len() > shown, |el| {
            el.child(div().text_color(look.dim).child(format!("… {} more lines", lines.len() - shown)))
        })
        .into_any_element()
}

/// File contents with line numbers in the gutter.
fn numbered(content: &str, look: &Look) -> AnyElement {
    let lines: Vec<&str> = content.lines().collect();
    let width = lines.len().max(1).to_string().len();
    let shown = lines.len().min(LINE_LIMIT);
    v_flex()
        .children(lines.iter().take(shown).enumerate().map(|(i, line)| {
            h_flex()
                .items_start()
                .gap_3()
                .child(div().flex_shrink_0().text_color(look.dim.opacity(0.6)).child(format!("{:>width$}", i + 1)))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_color(look.text)
                        .child(if line.is_empty() { " " } else { line }.to_string()),
                )
        }))
        .when(lines.len() > shown, |el| {
            el.child(div().text_color(look.dim).child(format!("… {} more lines", lines.len() - shown)))
        })
        .into_any_element()
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Diff {
    Same,
    Removed,
    Added,
}

/// A line diff of `old` against `new` (longest common subsequence). Very large inputs skip
/// the search and show every old line removed and every new line added.
fn line_diff(old: &str, new: &str) -> Vec<(Diff, String)> {
    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();
    // Shared first and last lines need no search.
    let head = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let tail = a[head..].iter().rev().zip(b[head..].iter().rev()).take_while(|(x, y)| x == y).count();
    let (am, bm) = (&a[head..a.len() - tail], &b[head..b.len() - tail]);
    let same = |lines: &[&str]| lines.iter().map(|l| (Diff::Same, l.to_string())).collect::<Vec<_>>();
    let mut out = same(&a[..head]);
    if am.len() * bm.len() > 250_000 {
        out.extend(am.iter().map(|l| (Diff::Removed, l.to_string())));
        out.extend(bm.iter().map(|l| (Diff::Added, l.to_string())));
    } else {
        // lcs[i][j]: the common subsequence length of am[i..] and bm[j..].
        let mut lcs = vec![vec![0u32; bm.len() + 1]; am.len() + 1];
        for i in (0..am.len()).rev() {
            for j in (0..bm.len()).rev() {
                lcs[i][j] = if am[i] == bm[j] { lcs[i + 1][j + 1] + 1 } else { lcs[i + 1][j].max(lcs[i][j + 1]) };
            }
        }
        let (mut i, mut j) = (0, 0);
        while i < am.len() || j < bm.len() {
            if i < am.len() && j < bm.len() && am[i] == bm[j] {
                out.push((Diff::Same, am[i].to_string()));
                i += 1;
                j += 1;
            } else if i < am.len() && (j == bm.len() || lcs[i + 1][j] >= lcs[i][j + 1]) {
                out.push((Diff::Removed, am[i].to_string()));
                i += 1;
            } else {
                out.push((Diff::Added, bm[j].to_string()));
                j += 1;
            }
        }
    }
    out.extend(same(&a[a.len() - tail..]));
    out
}

/// A tool's arguments as `key: value` lines, or `None` when it has none.
fn arguments(input: &Value) -> Option<String> {
    let map = input.as_object().filter(|m| !m.is_empty())?;
    let lines: Vec<String> = map
        .iter()
        .map(|(k, v)| match v {
            Value::String(s) => format!("{k}: {}", clean(s)),
            other => format!("{k}: {}", serde_json::to_string_pretty(other).unwrap_or_default()),
        })
        .collect();
    Some(lines.join("\n"))
}

/// JSON output (common from MCP tools) laid out readably; anything else as it came.
fn pretty_json(text: &str) -> String {
    let trimmed = text.trim_start();
    if (trimmed.starts_with('{') || trimmed.starts_with('['))
        && let Ok(value) = serde_json::from_str::<Value>(text)
    {
        return serde_json::to_string_pretty(&value).unwrap_or_else(|_| text.to_string());
    }
    text.to_string()
}

/// Text ready to draw: terminal colour codes stripped, tabs as spaces, trimmed to the limit.
fn clean(text: &str) -> String {
    let mut out = String::with_capacity(text.len().min(OUTPUT_LIMIT));
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if out.len() >= OUTPUT_LIMIT {
            out.push_str("\n… (cut short)");
            break;
        }
        match c {
            '\u{1b}' => {
                // CSI (`ESC [ ... letter`) and OSC (`ESC ] ... BEL/ESC \`) sequences.
                match chars.peek() {
                    Some('[') => {
                        chars.next();
                        for c in chars.by_ref() {
                            if c.is_ascii_alphabetic() || c == '~' {
                                break;
                            }
                        }
                    }
                    Some(']') => {
                        chars.next();
                        while let Some(c) = chars.next() {
                            if c == '\u{7}' || (c == '\u{1b}' && chars.next_if_eq(&'\\').is_some()) {
                                break;
                            }
                        }
                    }
                    _ => {
                        chars.next();
                    }
                }
            }
            '\t' => out.push_str("    "),
            '\r' => {}
            c => out.push(c),
        }
    }
    out.trim_end().to_string()
}

fn first_line(text: &str) -> String {
    text.lines().find(|l| !l.trim().is_empty()).unwrap_or_default().trim().to_string()
}

#[cfg(test)]
mod tests {
    // Named imports: `super::*` would bring in gpui's `test` attribute in place of the standard one.
    use super::{
        Diff, Duration, Family, Path, SHELL_CHARS, SHELL_LINES, ShellText, clean, header_summary, kind, line_diff,
        opens_by_default, quote, title, took_label,
    };
    use serde_json::json;

    #[test]
    fn every_known_tool_has_a_family_and_unknown_ones_fall_back() {
        assert_eq!(kind("Bash").0, Family::Shell);
        assert_eq!(kind("Edit").0, Family::Write);
        assert_eq!(kind("WebSearch").0, Family::Web);
        assert_eq!(kind("mcp__claude_ai_Gmail__search_threads").0, Family::Other);
        assert_eq!(kind("SomethingNew").0, Family::Other);
    }

    #[test]
    fn mcp_tools_are_titled_by_server_and_tool() {
        assert_eq!(title("mcp__claude_ai_Gmail__search_threads"), "Gmail · search threads");
        assert_eq!(title("mcp__github__create_issue"), "github · create issue");
        assert_eq!(title("Bash"), "Bash");
    }

    #[test]
    fn headers_use_the_description_and_project_relative_paths() {
        let project = Path::new("/p");
        let bash = json!({ "command": "ls -la", "description": "List files" });
        assert_eq!(header_summary("Bash", &bash, "ls -la", project), "List files");
        let bare = json!({ "command": "cd x\nls" });
        assert_eq!(header_summary("Bash", &bare, "cd x\nls", project), "cd x");
        let read = json!({ "file_path": "/p/src/a.rs", "offset": 10, "limit": 5 });
        assert_eq!(header_summary("Read", &read, "/p/src/a.rs", project), "src/a.rs · lines 10–14");
        let mcp = json!({ "query": "from:ci", "limit": 5 });
        assert_eq!(header_summary("mcp__x__search", &mcp, &mcp.to_string(), project), "from:ci · 5");
    }

    #[test]
    fn line_diff_keeps_shared_lines_and_marks_the_rest() {
        let diff = line_diff("a\nb\nc\nd", "a\nB\nc\nd\ne");
        let ops: Vec<Diff> = diff.iter().map(|(op, _)| *op).collect();
        use Diff::*;
        assert_eq!(ops, vec![Same, Removed, Added, Same, Same, Added]);
        assert_eq!(diff[1].1, "b");
        assert_eq!(diff[2].1, "B");
    }

    #[test]
    fn colour_codes_and_tabs_are_cleaned_from_output() {
        assert_eq!(clean("\u{1b}[31mred\u{1b}[0m\tx\r\n"), "red    x");
        assert_eq!(clean("\u{1b}]0;title\u{7}ok"), "ok");
    }

    #[test]
    fn a_bash_card_lays_out_at_most_thirty_lines_and_a_few_thousand_characters() {
        let lines = |n: usize| (1..=n).map(|i| format!("line {i}\n")).collect::<String>();
        let shown = |shell: &ShellText| {
            let short = shell.short.as_ref().expect("cut short");
            short.command.text.lines().count() + short.output.as_ref().map_or(0, |o| o.text.lines().count())
        };
        // Short text is laid out whole.
        let mut shell = ShellText::new("echo hi");
        shell.finish(&lines(SHELL_LINES - 1), false);
        assert!(shell.short.is_none());
        // Long output is cut after the command.
        shell.finish(&lines(300), false);
        assert_eq!(shown(&shell), SHELL_LINES);
        assert!(shell.is_cut());
        // A long script is cut too, and leaves room for the start of its output.
        let mut shell = ShellText::new(&lines(100));
        assert_eq!(shown(&shell), SHELL_LINES);
        shell.finish(&lines(50), false);
        let short = shell.short.as_ref().unwrap();
        assert!(short.command_cut);
        assert_eq!(short.output.as_ref().unwrap().text.lines().count(), 10);
        assert_eq!(shown(&shell), SHELL_LINES);
        // One very long line is cut by its length, at a character boundary.
        let mut shell = ShellText::new("cat one-line.json");
        shell.finish(&"é".repeat(SHELL_CHARS), false);
        let output = &shell.short.as_ref().expect("cut short").output.as_ref().unwrap().text;
        assert_eq!(output.len(), SHELL_CHARS);
        assert!(!shell.short.as_ref().unwrap().command_cut);
    }

    #[test]
    fn how_long_a_call_took_reads_in_seconds_minutes_and_hours() {
        assert_eq!(took_label(Duration::from_millis(2_440)), "Took 2.4s");
        assert_eq!(took_label(Duration::from_millis(40)), "Took 40ms");
        assert_eq!(took_label(Duration::from_secs(72)), "Took 1m 12s");
        assert_eq!(took_label(Duration::from_secs(3_790)), "Took 1h 3m");
    }

    #[test]
    fn a_quoted_call_names_the_tool_and_repeats_a_bash_command() {
        let bash = json!({ "command": "git status\ngit diff" });
        assert_eq!(
            quote("Bash", &bash, "Check the tree"),
            "About your earlier Bash call (Check the tree):\n```\n$ git status\ngit diff\n```"
        );
        let long = json!({ "command": "x\n".repeat(30) });
        assert!(quote("Bash", &long, "").ends_with("x\n…\n```"));
        assert_eq!(quote("Read", &json!({}), "src/a.rs"), "About your earlier Read call (src/a.rs):");
    }

    #[test]
    fn commands_and_edits_start_open() {
        assert!(opens_by_default("Bash"));
        assert!(opens_by_default("Edit"));
        assert!(!opens_by_default("Read"));
        assert!(!opens_by_default("Grep"));
    }
}
