//! Markdown files named in Claude's replies, as links that open in a dialog.
//!
//! [`linkify`] turns a mention of an existing `.md` file (`` `docs/plan.md` ``, a bare
//! `README.md`, `TODO.md:12`) into a Markdown link to it, leaving code blocks and existing
//! links alone. A click on such a link, or on a link Claude wrote itself, goes to [`target`],
//! and [`MarkdownViewer`] shows the file, rendered, with the thread's other mentioned files a
//! keypress away.

use std::path::{Path, PathBuf};

use gpui_kit::assets::IconName;
use gpui_kit::base::{h_flex, v_flex};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, Sizable as _,
    button::{Button, ButtonVariants as _},
    dialog::Dialog,
    text::{TextView, TextViewState},
};
use gpui_kit::{prelude::FluentBuilder as _, *};

/// `path` resolved for reading: `~/` from the home folder, a relative path from `root`.
pub fn resolve(root: &Path, path: &str) -> PathBuf {
    let path = path.strip_prefix("file://").unwrap_or(path);
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    root.join(path)
}

/// Whether `path` names a Markdown file, by its extension.
fn is_markdown(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    (lower.ends_with(".md") && lower.len() > 3) || (lower.ends_with(".markdown") && lower.len() > 9)
}

/// The Markdown file a clicked link points to, if it is one that exists. Web links and links
/// to anything else give `None`, and open as they always have.
pub fn target(url: &str, root: &Path) -> Option<PathBuf> {
    if url.contains("://") && !url.starts_with("file://") {
        return None;
    }
    let path = url.split('#').next().unwrap_or(url);
    let (path, _) = split_line(path);
    if !is_markdown(path) {
        return None;
    }
    let file = resolve(root, path);
    file.is_file().then_some(file)
}

/// `TODO.md:12` or `TODO.md:12-20` split into the path and the line part (`:12`, `:12-20`).
fn split_line(text: &str) -> (&str, &str) {
    if let Some(colon) = text.rfind(':') {
        let tail = &text[colon + 1..];
        let mut parts = tail.splitn(2, '-');
        let first = parts.next().unwrap_or("");
        let rest_ok = parts.next().is_none_or(|r| !r.is_empty() && r.bytes().all(|b| b.is_ascii_digit()));
        if !first.is_empty() && first.bytes().all(|b| b.is_ascii_digit()) && rest_ok {
            return (&text[..colon], &text[colon..]);
        }
    }
    (text, "")
}

/// The Markdown file `candidate` names (with any `:line` dropped), if it looks like one and
/// `exists` says it's there.
fn markdown_path<'a>(candidate: &'a str, exists: &mut impl FnMut(&str) -> bool) -> Option<&'a str> {
    let (path, _) = split_line(candidate);
    let plausible = is_markdown(path)
        && !path.contains("://")
        && !path.starts_with("//")
        && !path.chars().any(|c| c.is_whitespace() || matches!(c, '<' | '>' | '[' | ']' | '(' | ')' | '`'));
    (plausible && exists(path)).then_some(path)
}

/// `text` with each mention of an existing Markdown file made a link to it. `exists` is asked
/// about each path that looks like one (relative paths are relative to the project).
///
/// Mentions in inline code and in plain text are linked; fenced code blocks, links and
/// autolinks are left as they are. The link keeps the text as written, so the reply reads the
/// same, and points at the path without its `:line`.
pub fn linkify(text: &str, mut exists: impl FnMut(&str) -> bool) -> String {
    let mut out = String::with_capacity(text.len() + 32);
    // The open fence: its character and how many of them.
    let mut fence: Option<(char, usize)> = None;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim_start();
        let run = |c: char| trimmed.chars().take_while(|&x| x == c).count();
        match fence {
            Some((c, n)) => {
                if run(c) >= n && trimmed.trim_start_matches(c).trim().is_empty() {
                    fence = None;
                }
                out.push_str(line);
            }
            None => {
                let opens = ['`', '~'].into_iter().find(|&c| run(c) >= 3);
                if let Some(c) = opens {
                    fence = Some((c, run(c)));
                    out.push_str(line);
                } else {
                    link_line(line, &mut exists, &mut out);
                }
            }
        }
    }
    out
}

/// Characters a bare path is made of.
fn path_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | '/' | '~' | '+')
}

fn push_link(out: &mut String, label: &str, path: &str) {
    out.push('[');
    out.push_str(label);
    out.push_str("](<");
    out.push_str(path);
    out.push_str(">)");
}

/// One line outside a code block: see [`linkify`].
fn link_line(line: &str, exists: &mut impl FnMut(&str) -> bool, out: &mut String) {
    let mut i = 0;
    let mut prev: Option<char> = None;
    while i < line.len() {
        let rest = &line[i..];
        let c = rest.chars().next().unwrap_or_default();
        match c {
            '`' => {
                let n = rest.chars().take_while(|&x| x == '`').count();
                // The closing run is exactly as long as the opening one.
                let mut close = None;
                let mut j = n;
                while let Some(at) = rest[j..].find('`').map(|at| j + at) {
                    let m = rest[at..].chars().take_while(|&x| x == '`').count();
                    if m == n {
                        close = Some(at);
                        break;
                    }
                    j = at + m;
                }
                match close {
                    Some(at) => {
                        let span = &rest[..at + n];
                        let code = rest[n..at].trim();
                        match markdown_path(code, exists) {
                            Some(path) => push_link(out, span, path),
                            None => out.push_str(span),
                        }
                        i += at + n;
                    }
                    // Not closed (yet, while the reply streams in): the rest of the line is
                    // left alone, so it doesn't turn into a link and back.
                    None => {
                        out.push_str(rest);
                        return;
                    }
                }
                prev = Some('`');
                continue;
            }
            // A link or an image: copied as it is, so it isn't linked twice.
            '[' | '!' if rest.starts_with('[') || rest.starts_with("![") => {
                let open = if c == '!' { 1 } else { 0 };
                let link = rest[open..].find("](").and_then(|close| {
                    let after = open + close + 2;
                    rest[after..].find(')').map(|end| after + end + 1)
                });
                let len = link.unwrap_or(c.len_utf8());
                out.push_str(&rest[..len]);
                i += len;
                prev = rest[..len].chars().last();
                continue;
            }
            // An autolink or HTML.
            '<' => {
                let len = rest.find('>').map(|end| end + 1).unwrap_or(1);
                out.push_str(&rest[..len]);
                i += len;
                prev = Some('>');
                continue;
            }
            _ => {}
        }
        let boundary = prev.is_none_or(|p| !path_char(p) && !matches!(p, ':' | '@' | '\\' | '$' | '%'));
        if path_char(c) && boundary {
            let len = rest.find(|ch: char| !path_char(ch)).unwrap_or(rest.len());
            // Sentence punctuation after a name isn't part of it.
            let token = rest[..len].trim_end_matches('.');
            // A `:12` or `:12-20` straight after it is.
            let after = &rest[token.len()..];
            let line_len = after
                .strip_prefix(':')
                .map(|t| {
                    let digits = t.find(|ch: char| !ch.is_ascii_digit() && ch != '-').unwrap_or(t.len());
                    let digits = t[..digits].trim_end_matches('-');
                    if digits.is_empty() { 0 } else { digits.len() + 1 }
                })
                .unwrap_or(0);
            let label = &rest[..token.len() + line_len];
            match markdown_path(label, exists) {
                Some(path) if !token.is_empty() => {
                    push_link(out, label, path);
                    i += label.len();
                    prev = label.chars().last();
                }
                _ => {
                    out.push_str(&rest[..len]);
                    i += len;
                    prev = rest[..len].chars().last();
                }
            }
            continue;
        }
        out.push(c);
        i += c.len_utf8();
        prev = Some(c);
    }
}

/// The Markdown files a linkified reply links to, in the order they appear, as resolved paths.
pub fn linked_files(text: &str, root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut rest = text;
    while let Some(at) = rest.find("](") {
        rest = &rest[at + 2..];
        let (url, len) = match rest.strip_prefix('<') {
            Some(inner) => match inner.find('>') {
                Some(end) => (&inner[..end], end + 2),
                None => break,
            },
            None => match rest.find(|c: char| c == ')' || c.is_whitespace()) {
                Some(end) => (&rest[..end], end),
                None => break,
            },
        };
        if let Some(file) = target(url, root)
            && !files.contains(&file)
        {
            files.push(file);
        }
        rest = &rest[len..];
    }
    files
}

/// A Markdown file shown in a dialog, rendered, with ← and → stepping through the others the
/// thread mentioned. ↑/↓, Page Up/Down, Home and End scroll it.
pub struct MarkdownViewer {
    files: Vec<PathBuf>,
    ix: usize,
    root: PathBuf,
    text: Entity<TextViewState>,
    problem: Option<String>,
    scroll: ScrollHandle,
    focus: FocusHandle,
}

impl MarkdownViewer {
    /// Show `files[ix]`. `root` is the project folder, for showing paths relative to it.
    pub fn new(files: Vec<PathBuf>, ix: usize, root: PathBuf, cx: &mut Context<Self>) -> Self {
        let text = cx.new(|cx| TextViewState::markdown("", cx).selectable(true));
        let mut viewer =
            Self { files, ix, root, text, problem: None, scroll: ScrollHandle::new(), focus: cx.focus_handle() };
        viewer.load(cx);
        viewer
    }

    pub fn focus(&self, window: &mut Window, cx: &mut App) {
        window.focus(&self.focus, cx);
    }

    /// Read the file on show again, so it shows what's on disk now.
    fn load(&mut self, cx: &mut Context<Self>) {
        let Some(file) = self.files.get(self.ix) else { return };
        let (text, problem) = match std::fs::read_to_string(file) {
            Ok(text) => (text, None),
            Err(err) => (String::new(), Some(format!("Could not read {}: {err}", file.display()))),
        };
        self.problem = problem;
        self.text.update(cx, |t, cx| t.set_text(&text, cx));
        self.scroll.set_offset(point(px(0.), px(0.)));
        cx.notify();
    }

    fn step(&mut self, by: isize, cx: &mut Context<Self>) {
        let ix = self.ix as isize + by;
        if ix >= 0 && (ix as usize) < self.files.len() {
            self.ix = ix as usize;
            self.load(cx);
        }
    }

    /// The file on show, relative to the project when it's inside it.
    pub fn shown_path(&self) -> String {
        let Some(file) = self.files.get(self.ix) else { return String::new() };
        file.strip_prefix(&self.root).unwrap_or(file).display().to_string()
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
}

impl Render for MarkdownViewer {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (count, ix) = (self.files.len(), self.ix);
        let navigator = (count > 1).then(|| {
            let (back, next) = (cx.entity().downgrade(), cx.entity().downgrade());
            h_flex()
                .gap_1()
                .items_center()
                .child(
                    Button::new("md-previous")
                        .xsmall()
                        .ghost()
                        .icon(IconName::ChevronLeft)
                        .tooltip("Previous file (←)")
                        .disabled(ix == 0)
                        .on_click(move |_, _, cx| {
                            back.update(cx, |v, cx| v.step(-1, cx)).ok();
                        }),
                )
                .child(div().text_xs().text_color(theme.muted_foreground).child(format!("File {} of {count}", ix + 1)))
                .child(
                    Button::new("md-next")
                        .xsmall()
                        .ghost()
                        .icon(IconName::ChevronRight)
                        .tooltip("Next file (→)")
                        .disabled(ix + 1 >= count)
                        .on_click(move |_, _, cx| {
                            next.update(cx, |v, cx| v.step(1, cx)).ok();
                        }),
                )
        });
        v_flex()
            .size_full()
            .min_h_0()
            .gap_2()
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::on_key))
            .children(navigator.map(|n| h_flex().justify_end().child(n)))
            .when_some(self.problem.clone(), |el, problem| {
                el.child(div().text_sm().text_color(theme.danger).child(problem))
            })
            .child(
                div()
                    .id("md-viewer-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .pr_2()
                    .child(TextView::new(&self.text).selectable(true)),
            )
    }
}

/// The dialog around a [`MarkdownViewer`]: the file's name as its title, as tall as the window
/// allows and wide enough to read.
pub fn dialog(dialog: Dialog, viewer: &Entity<MarkdownViewer>, window: &mut Window, cx: &mut App) -> Dialog {
    let size = window.viewport_size();
    let gap = cx.theme().spacing_tokens().lg;
    let path = viewer.read(cx).shown_path();
    let title = h_flex().gap_2().min_w_0().child(Icon::new(IconName::File).small()).child(
        div()
            .min_w_0()
            .overflow_hidden()
            .whitespace_nowrap()
            .text_ellipsis()
            .font_weight(FontWeight::SEMIBOLD)
            .child(path),
    );
    dialog
        .w((size.width * 0.85).min(px(960.)))
        .margin_top(gap)
        .h(size.height - gap * 2.)
        .title(title)
        .child(div().size_full().min_h_0().child(viewer.clone()))
}

#[cfg(test)]
mod tests {
    use super::{linked_files, linkify, split_line, target};

    fn exists(path: &str) -> bool {
        matches!(path, "README.md" | "docs/plan.md" | "TODO.md" | "~/notes.md")
    }

    #[test]
    fn inline_code_mentions_become_links() {
        assert_eq!(linkify("See `docs/plan.md` for it.", exists), "See [`docs/plan.md`](<docs/plan.md>) for it.");
        assert_eq!(linkify("At `TODO.md:12`.", exists), "At [`TODO.md:12`](<TODO.md>).");
        // Not there, or not Markdown: left alone.
        assert_eq!(linkify("`missing.md` and `src/main.rs`", exists), "`missing.md` and `src/main.rs`");
    }

    #[test]
    fn bare_mentions_become_links_without_their_punctuation() {
        assert_eq!(linkify("Updated README.md.", exists), "Updated [README.md](<README.md>).");
        assert_eq!(linkify("**TODO.md:12-20**, done", exists), "**[TODO.md:12-20](<TODO.md>)**, done");
        assert_eq!(linkify("(~/notes.md)", exists), "([~/notes.md](<~/notes.md>))");
    }

    #[test]
    fn links_urls_and_code_blocks_are_left_alone() {
        for text in [
            "[the plan](docs/plan.md) and ![pic](README.md)",
            "https://github.com/a/b/blob/main/README.md",
            "<https://x.dev/README.md>",
            "@README.md",
            "```\ncat README.md\n```\n",
            "~~~md\n`README.md`\n~~~\n",
        ] {
            assert_eq!(linkify(text, exists), text, "{text}");
        }
        // After a code block closes, mentions link again.
        assert_eq!(
            linkify("```\nREADME.md\n```\nREADME.md\n", exists),
            "```\nREADME.md\n```\n[README.md](<README.md>)\n"
        );
    }

    #[test]
    fn a_reply_streamed_so_far_links_what_is_complete() {
        // An unclosed code span or fence links nothing after it.
        assert_eq!(linkify("See `README.md", exists), "See `README.md");
        assert_eq!(linkify("```\nREADME.md", exists), "```\nREADME.md");
        // A double-backtick span works too.
        assert_eq!(linkify("``README.md``", exists), "[``README.md``](<README.md>)");
    }

    #[test]
    fn line_numbers_split_off() {
        assert_eq!(split_line("TODO.md:12"), ("TODO.md", ":12"));
        assert_eq!(split_line("TODO.md:12-20"), ("TODO.md", ":12-20"));
        assert_eq!(split_line("TODO.md:x"), ("TODO.md:x", ""));
        assert_eq!(split_line("TODO.md"), ("TODO.md", ""));
    }

    #[test]
    fn clicks_resolve_to_existing_markdown_files_only() {
        let dir = std::env::temp_dir().join(format!("tj-md-link-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("docs")).unwrap();
        std::fs::write(dir.join("docs/plan.md"), "# Plan").unwrap();
        std::fs::write(dir.join("main.rs"), "").unwrap();
        assert_eq!(target("docs/plan.md", &dir), Some(dir.join("docs/plan.md")));
        assert_eq!(target("docs/plan.md#goals", &dir), Some(dir.join("docs/plan.md")));
        assert_eq!(target("docs/plan.md:3", &dir), Some(dir.join("docs/plan.md")));
        let absolute = dir.join("docs/plan.md").display().to_string();
        assert_eq!(target(&absolute, Path::new("/")), Some(dir.join("docs/plan.md")));
        assert_eq!(target("docs/gone.md", &dir), None);
        assert_eq!(target("main.rs", &dir), None);
        assert_eq!(target("https://example.com/plan.md", &dir), None);

        let text = "[a](<docs/plan.md>) [b](docs/plan.md) [c](https://x.dev) [d](<docs/gone.md>)";
        assert_eq!(linked_files(text, &dir), vec![dir.join("docs/plan.md")]);
        std::fs::remove_dir_all(&dir).ok();
    }

    use std::path::Path;
}
