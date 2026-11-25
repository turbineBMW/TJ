//! How a tool call looks in the transcript. Each tool has an icon, and its family (shell,
//! reading, editing, web, ...) gives it a colour from the theme's terminal palette, so cards
//! follow the Omarchy theme. The header shows the tool, a one-line summary and a spinner, check
//! or cross; the body is shaped for the tool (a little terminal for Bash, a diff for Edit) and
//! scrolls once it reaches half the window.

use std::{path::Path, sync::Arc};

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
    /// The body's scroll position; it also tells whether the body overflows.
    pub scroll: &'a ScrollHandle,
}

/// A Bash call's command and what it printed, coloured once and kept with the row.
#[derive(Clone)]
pub struct ShellText {
    command: Highlighted,
    output: Option<Highlighted>,
    failed: bool,
}

impl ShellText {
    pub fn new(command: &str) -> Self {
        ShellText { command: Highlighted::command(clean(command)), output: None, failed: false }
    }

    pub fn finish(&mut self, output: &str, failed: bool) {
        let output = clean(output);
        self.output = (!output.trim().is_empty()).then(|| Highlighted::output(output));
        self.failed = failed;
    }
}

/// Whether a card's body is taller than the space it gets, going by its last layout.
pub fn overflows(scroll: &ScrollHandle) -> bool {
    scroll.max_offset().y > px(1.)
}

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

pub fn render(
    card: Card,
    window: &Window,
    cx: &App,
    on_toggle: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    on_collapse: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    // A shell card too big for its space opens in a dialog from its header, so it gets its own
    // button to fold away.
    let opens_dialog = card.shell.is_some() && card.expanded && overflows(card.scroll);
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
        .cursor_pointer()
        .on_click(on_toggle)
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
        .when(opens_dialog, |el| {
            el.child(Icon::new(IconName::Maximize2).xsmall().text_color(look.dim)).child(
                div()
                    .id(SharedString::from(format!("tool-fold-{}", card.ix)))
                    .test_support()
                    .flex_shrink_0()
                    .rounded_sm()
                    .p_0p5()
                    .hover(|el| el.bg(look.text.opacity(0.12)))
                    .on_click(move |event, window, cx| {
                        cx.stop_propagation();
                        on_collapse(event, window, cx);
                    })
                    .child(Icon::new(IconName::ChevronsDownUp).xsmall().text_color(look.dim)),
            )
        })
        .child(div().flex_shrink_0().child(mark));

    let body = if !card.expanded {
        None
    } else if let Some(details) = card.details {
        Some(details)
    } else {
        tool_body(&card, &look, mono.clone())
    };

    v_flex()
        .rounded_md()
        .overflow_hidden()
        .border_1()
        .border_color(look.border)
        .child(header)
        .when_some(card.now.clone(), |el, now| {
            el.child(
                div()
                    .px_3()
                    .pb_1p5()
                    .bg(look.header_bg)
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
                    .when_some(look.body_bg, |el, bg| el.bg(bg))
                    .max_h(window.viewport_size().height * 0.5)
                    .overflow_y_scroll()
                    .track_scroll(card.scroll)
                    .px_3()
                    .py_2()
                    .text_xs()
                    .font_family(mono)
                    .child(body),
            )
        })
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
        "Bash" if card.shell.is_some() => vec![shell_body(card.shell?, look)],
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
                div()
                    .when(i + 1 < count && card.name != "Bash", |el| el.pb_2().border_b_1().border_color(look.border))
                    .child(part)
            }))
            .into_any_element(),
    )
}

/// A command after a `$`, then what it printed, both coloured.
fn shell_body(shell: &ShellText, look: &Look) -> AnyElement {
    v_flex()
        .gap_2()
        .child(
            h_flex()
                .items_start()
                .gap_2()
                .child(div().flex_shrink_0().text_color(look.dim).child("$"))
                .child(div().flex_1().min_w_0().text_color(look.text).child(shell.command.styled(&look.palette))),
        )
        .when_some(shell.output.as_ref(), |el, output| {
            el.child(
                div().text_color(if shell.failed { look.error } else { look.dim }).child(output.styled(&look.palette)),
            )
        })
        .into_any_element()
}

/// A Bash card opened in a dialog: the whole command and output, scrolling inside it.
pub fn shell_dialog(dialog: Dialog, summary: &str, shell: &ShellText, window: &mut Window, cx: &mut App) -> Dialog {
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
            .child(shell_body(shell, &look)),
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
    use super::{Diff, Family, Path, clean, header_summary, kind, line_diff, opens_by_default, title};
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
    fn commands_and_edits_start_open() {
        assert!(opens_by_default("Bash"));
        assert!(opens_by_default("Edit"));
        assert!(!opens_by_default("Read"));
        assert!(!opens_by_default("Grep"));
    }
}
