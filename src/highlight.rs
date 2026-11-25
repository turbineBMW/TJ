//! Colouring for shell commands and what they print, in the theme's terminal palette so it
//! matches the terminal. Commands are parsed as bash and JSON output as JSON (tree-sitter,
//! via gpui-kit); diffs and error or warning lines are coloured line by line.

use std::{cell::RefCell, ops::Range, rc::Rc};

use gpui_kit::component::{Rope, highlighter::SyntaxHighlighter};
use gpui_kit::{FontStyle, FontWeight, HighlightStyle, Hsla, SharedString, StyledText};

/// The colours text is drawn in: the 16 ANSI colours, plus the plain and faint text.
#[derive(Clone, Copy, PartialEq)]
pub struct Palette {
    pub ansi: [Hsla; 16],
    pub text: Hsla,
    pub dim: Hsla,
}

/// What a piece of text is, which decides its colour.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Token {
    Command,
    Keyword,
    String,
    Number,
    Constant,
    Variable,
    Operator,
    Comment,
    Added,
    Removed,
    Hunk,
    Heading,
    Error,
    Warning,
}

impl Token {
    /// The token for a tree-sitter capture name, by its first part (`function.builtin`).
    fn from_capture(name: &str) -> Option<Token> {
        Some(match name.split('.').next()? {
            "function" => Token::Command,
            "keyword" => Token::Keyword,
            "string" => Token::String,
            "number" => Token::Number,
            "constant" | "boolean" => Token::Constant,
            "property" | "variable" => Token::Variable,
            "operator" => Token::Operator,
            "comment" => Token::Comment,
            _ => return None,
        })
    }

    fn style(self, p: &Palette) -> HighlightStyle {
        let color = |c: Hsla| HighlightStyle { color: Some(c), ..Default::default() };
        match self {
            Token::Command => color(p.ansi[4]),
            Token::Keyword => color(p.ansi[5]),
            Token::String | Token::Added => color(p.ansi[2]),
            Token::Number | Token::Constant | Token::Warning => color(p.ansi[3]),
            Token::Variable | Token::Hunk => color(p.ansi[6]),
            Token::Operator | Token::Removed => color(p.ansi[1]),
            Token::Error => color(p.ansi[9]),
            Token::Comment => HighlightStyle {
                color: Some(p.dim.opacity(0.75)),
                font_style: Some(FontStyle::Italic),
                ..Default::default()
            },
            Token::Heading => {
                HighlightStyle { color: Some(p.text), font_weight: Some(FontWeight::BOLD), ..Default::default() }
            }
        }
    }
}

/// Maps tree-sitter capture names to palette colours.
struct Resolver(Palette);

impl gpui_kit::base::input::HighlightStyleResolver for Resolver {
    fn style(&self, name: &str) -> Option<HighlightStyle> {
        Token::from_capture(name).map(|t| t.style(&self.0))
    }
}

/// Coloured ranges of a text.
type Styles = Rc<Vec<(Range<usize>, HighlightStyle)>>;

/// Text and how to colour it. Cheap to clone; a grammar's colours are worked out once per
/// palette and kept, so a card isn't parsed again on every frame.
#[derive(Clone)]
pub struct Highlighted {
    pub text: SharedString,
    how: How,
    cache: Rc<RefCell<Option<(Palette, Styles)>>>,
}

#[derive(Clone)]
enum How {
    /// Parsed with a tree-sitter grammar when drawn.
    Grammar(&'static str),
    /// Already split into coloured ranges.
    Spans(Rc<Vec<(Range<usize>, Token)>>),
}

impl Highlighted {
    fn new(text: String, how: How) -> Self {
        Highlighted { text: text.into(), how, cache: Rc::default() }
    }

    /// A shell command, coloured as bash.
    pub fn command(text: String) -> Self {
        Self::new(text, How::Grammar("bash"))
    }

    /// A command's output: JSON as JSON, a diff as a diff, and otherwise its error and
    /// warning lines picked out.
    pub fn output(text: String) -> Self {
        let trimmed = text.trim_start();
        if (trimmed.starts_with('{') || trimmed.starts_with('['))
            && serde_json::from_str::<serde_json::Value>(&text).is_ok()
        {
            return Self::new(text, How::Grammar("json"));
        }
        let spans = line_spans(&text);
        Self::new(text, How::Spans(Rc::new(spans)))
    }

    /// The text as a styled element in `palette`.
    pub fn styled(&self, palette: &Palette) -> StyledText {
        StyledText::new(self.text.clone()).with_highlights(self.styles(palette).iter().cloned())
    }

    fn styles(&self, palette: &Palette) -> Styles {
        if let Some((cached, styles)) = self.cache.borrow().as_ref()
            && cached == palette
        {
            return styles.clone();
        }
        let styles = Rc::new(match &self.how {
            How::Spans(spans) => spans.iter().map(|(range, token)| (range.clone(), token.style(palette))).collect(),
            How::Grammar(lang) => {
                let mut highlighter = SyntaxHighlighter::new(lang);
                highlighter.update(None, &Rope::from_str(&self.text), None);
                highlighter.styles(&(0..self.text.len()), &Resolver(*palette))
            }
        });
        *self.cache.borrow_mut() = Some((*palette, styles.clone()));
        styles
    }

    #[cfg(test)]
    fn tokens(&self) -> Vec<(&str, Token)> {
        match &self.how {
            How::Spans(spans) => spans.iter().map(|(r, t)| (&self.text[r.clone()], *t)).collect(),
            How::Grammar(_) => Vec::new(),
        }
    }
}

/// Line-by-line colours for output: a unified diff's additions, removals, hunks and file
/// headers, or else error and warning lines.
fn line_spans(text: &str) -> Vec<(Range<usize>, Token)> {
    let is_diff = text.lines().any(|l| l.starts_with("diff --git") || l.starts_with("@@ "))
        && text.lines().any(|l| l.starts_with('+') || l.starts_with('-'));
    let mut spans = Vec::new();
    let mut start = 0;
    for line in text.split_inclusive('\n') {
        let content = line.trim_end_matches('\n');
        let range = start..start + content.len();
        start += line.len();
        let token = if is_diff {
            match content {
                l if l.starts_with("+++") || l.starts_with("---") => Some(Token::Heading),
                l if l.starts_with("diff ") || l.starts_with("index ") => Some(Token::Heading),
                l if l.starts_with("@@") => Some(Token::Hunk),
                l if l.starts_with('+') => Some(Token::Added),
                l if l.starts_with('-') => Some(Token::Removed),
                _ => None,
            }
        } else {
            let lower = content.trim_start().to_ascii_lowercase();
            if ["error", "fatal", "panicked", "failed"].iter().any(|w| lower.starts_with(w))
                || content.contains("FAILED")
            {
                Some(Token::Error)
            } else if lower.starts_with("warning") {
                Some(Token::Warning)
            } else {
                None
            }
        };
        if let Some(token) = token.filter(|_| !range.is_empty()) {
            spans.push((range, token));
        }
    }
    spans
}

#[cfg(test)]
mod tests {
    // Named imports: gpui's glob would replace the standard `test` attribute.
    use super::{Highlighted, Palette, Token};
    use gpui_kit::hsla;

    fn palette() -> Palette {
        let ansi = std::array::from_fn(|i| hsla(i as f32 / 16.0, 0.5, 0.5, 1.0));
        Palette { ansi, text: hsla(0., 0., 0.8, 1.), dim: hsla(0., 0., 0.6, 1.) }
    }

    #[test]
    fn a_diff_is_coloured_by_line() {
        let out = Highlighted::output("diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -1 +1 @@\n-old\n+new\n same".into());
        assert_eq!(
            out.tokens(),
            vec![
                ("diff --git a/x b/x", Token::Heading),
                ("--- a/x", Token::Heading),
                ("+++ b/x", Token::Heading),
                ("@@ -1 +1 @@", Token::Hunk),
                ("-old", Token::Removed),
                ("+new", Token::Added),
            ]
        );
    }

    #[test]
    fn error_and_warning_lines_stand_out() {
        let out = Highlighted::output("Compiling tj\nwarning: unused\nerror[E0425]: oops\n  --> src/a.rs".into());
        assert_eq!(out.tokens(), vec![("warning: unused", Token::Warning), ("error[E0425]: oops", Token::Error)]);
    }

    #[test]
    fn a_command_is_parsed_as_bash() {
        let cmd = Highlighted::command("cargo test --lib 2>&1 | tail -5 # quick".into());
        let styles = cmd.styles(&palette());
        let blue = palette().ansi[4];
        let commands: Vec<&str> =
            styles.iter().filter(|(_, s)| s.color == Some(blue)).map(|(r, _)| &cmd.text[r.clone()]).collect();
        assert_eq!(commands, vec!["cargo", "tail"]);
        assert!(styles.iter().any(|(r, s)| &cmd.text[r.clone()] == "# quick" && s.font_style.is_some()));
    }

    #[test]
    fn json_output_is_parsed_as_json() {
        let out = Highlighted::output(r#"{"ok": true, "n": 3}"#.into());
        let styles = out.styles(&palette());
        assert!(styles.iter().any(|(r, _)| &out.text[r.clone()] == "3"));
    }
}
