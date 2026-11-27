//! The 80/20 summary: a short closing section Claude writes when it reports finished work or
//! leaves the user a decision, which TJ draws as a card instead of plain text.
//!
//! The setting (`Settings::eighty_twenty`) adds [`INSTRUCTIONS`] to new sessions. The card is
//! drawn for any reply that ends with the section, so one written because of a CLAUDE.md rule
//! gets it too.

use gpui_kit::assets::IconName;
use gpui_kit::base::{h_flex, v_flex};
use gpui_kit::component::{
    ActiveTheme as _, Icon, Sizable as _,
    text::{TextView, TextViewState},
};
use gpui_kit::{prelude::FluentBuilder as _, *};

/// Added to Claude's system prompt while the setting is on.
pub const INSTRUCTIONS: &str = "When a reply reports finished work (files changed, commands run, something \
built or fixed) or leaves the user a decision, end it with an 80/20 summary, which TJ draws as a card: a `---` \
line, then a `## 80/20` heading, then the items below. Skip it for short answers, explanations, back-and-forth, \
and reports written for another agent. Bottom line only, nothing new. Write each item on its own line, starting \
with its emoji and label exactly as shown, and include an item only when it has real content:
✅ Result: always. One line. If something was not done, say so here.
👉 Next: only if the user must do something. A numbered list under the label, one step per line, action in bold. \
Never list work you skipped.
❓ Decide: only if the choice is the user's. One question per line; number them under the label if there are \
several.
⚠️ Watch out: only a real risk.
🔀 Git: only if a repo changed.
Bold the key actions, so the bold text alone is the answer. Write the section once, at the very end.";

/// What an item of the summary is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Result,
    Next,
    Decide,
    WatchOut,
    Git,
    /// Text before the first label.
    Note,
}

impl Kind {
    const LABELLED: [Kind; 5] = [Kind::Result, Kind::Next, Kind::Decide, Kind::WatchOut, Kind::Git];

    fn label(self) -> &'static str {
        match self {
            Kind::Result => "Result",
            Kind::Next => "Next",
            Kind::Decide => "Decide",
            Kind::WatchOut => "Watch out",
            Kind::Git => "Git",
            Kind::Note => "",
        }
    }

    fn icon(self) -> IconName {
        match self {
            Kind::Result => IconName::CircleCheck,
            Kind::Next => IconName::ArrowRight,
            Kind::Decide => IconName::CircleQuestionMark,
            Kind::WatchOut => IconName::TriangleAlert,
            Kind::Git => IconName::GitBranch,
            Kind::Note => IconName::Info,
        }
    }

    fn color(self, cx: &App) -> Hsla {
        let theme = cx.theme();
        match self {
            Kind::Result => theme.success,
            Kind::Next => theme.primary,
            Kind::Decide => theme.info,
            Kind::WatchOut => theme.warning,
            Kind::Git | Kind::Note => theme.muted_foreground,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Item {
    pub kind: Kind,
    /// Markdown, without the emoji and label.
    pub text: String,
}

/// Where the summary starts in a reply: the `---` line before its heading, or the heading.
/// `None` when the reply has none. Headings inside code blocks don't count.
pub fn split(text: &str) -> Option<usize> {
    let mut fenced = false;
    let mut found = None;
    let mut rule: Option<usize> = None;
    let mut at = 0;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            fenced = !fenced;
        } else if !fenced && is_heading(trimmed) {
            found = Some(rule.unwrap_or(at));
        }
        // A rule counts for the next heading only if nothing but blank lines come between.
        if !fenced && is_rule(trimmed) {
            rule = Some(at);
        } else if !trimmed.is_empty() {
            rule = None;
        }
        at += line.len();
    }
    found
}

fn is_heading(line: &str) -> bool {
    let Some(rest) = line.strip_prefix('#') else { return false };
    let rest = rest.trim_start_matches('#');
    rest.starts_with(' ') && rest.trim().trim_matches('*').trim() == "80/20"
}

fn is_rule(line: &str) -> bool {
    line.len() >= 3 && ['-', '*', '_'].into_iter().any(|c| line.chars().all(|ch| ch == c))
}

/// The items of a summary section (`text` starts at [`split`]).
pub fn parse(text: &str) -> Vec<Item> {
    let mut items: Vec<Item> = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if is_rule(trimmed) || is_heading(trimmed) {
            continue;
        }
        if let Some((kind, rest)) = labelled(trimmed) {
            items.push(Item { kind, text: rest.to_string() });
            continue;
        }
        match items.last_mut() {
            Some(item) => {
                if !item.text.is_empty() || !trimmed.is_empty() {
                    item.text.push('\n');
                }
                item.text.push_str(line);
            }
            None if trimmed.is_empty() => {}
            None => items.push(Item { kind: Kind::Note, text: line.to_string() }),
        }
    }
    for item in &mut items {
        item.text = item.text.trim().to_string();
    }
    items
}

/// A line that starts an item: an optional emoji, the label (maybe bold), and a colon.
fn labelled(line: &str) -> Option<(Kind, &str)> {
    let rest = line.trim_start_matches(|c: char| !c.is_alphanumeric() && c != '*' && c != '_');
    let rest = rest.trim_start_matches(['*', '_']);
    Kind::LABELLED.into_iter().find_map(|kind| {
        let label = kind.label();
        let head = rest.get(..label.len())?;
        if !head.eq_ignore_ascii_case(label) {
            return None;
        }
        let after = rest[label.len()..].trim_start_matches(['*', '_']);
        let after = after.strip_prefix(':')?;
        Some((kind, after.trim_start_matches(['*', '_']).trim()))
    })
}

/// A reply's summary, drawn as a card under the rest of the reply.
pub struct SummaryCard {
    items: Vec<(Kind, Entity<TextViewState>)>,
}

impl SummaryCard {
    pub fn new() -> Self {
        Self { items: Vec::new() }
    }

    /// Show these items, reusing the text views already made (the card is filled while the
    /// reply streams in).
    pub fn sync(&mut self, items: Vec<Item>, cx: &mut App) {
        self.items.truncate(items.len());
        for (ix, item) in items.into_iter().enumerate() {
            match self.items.get_mut(ix) {
                Some((kind, view)) => {
                    *kind = item.kind;
                    view.update(cx, |v, cx| v.set_text(&item.text, cx));
                }
                None => {
                    let view = cx.new(|cx| TextViewState::markdown(&item.text, cx).selectable(true));
                    self.items.push((item.kind, view));
                }
            }
        }
    }

    pub fn render(&self, cx: &App) -> impl IntoElement {
        let theme = cx.theme();
        let (border, muted) = (theme.border, theme.muted_foreground);
        let bg = theme.secondary.opacity(0.45);
        let header = h_flex()
            .gap_1p5()
            .px_3()
            .py_1p5()
            .text_xs()
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(muted)
            .child(Icon::new(IconName::Target).xsmall())
            .child("80/20");
        v_flex()
            .w_full()
            .mt_1()
            .rounded_lg()
            .border_1()
            .border_color(border)
            .bg(bg)
            .overflow_hidden()
            .child(header)
            .children(self.items.iter().map(|(kind, view)| {
                let color = kind.color(cx);
                let chip = h_flex()
                    .flex_none()
                    .w(px(96.))
                    .gap_1p5()
                    .pt(px(3.))
                    .text_xs()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(color)
                    .child(Icon::new(kind.icon()).xsmall())
                    .when(*kind != Kind::Note, |el| el.child(kind.label()));
                h_flex()
                    .items_start()
                    .gap_3()
                    .px_3()
                    .py_2()
                    .border_t_1()
                    .border_color(border)
                    .child(chip)
                    .child(div().flex_1().min_w_0().child(TextView::new(view).selectable(true)))
            }))
    }
}

#[cfg(test)]
mod tests {
    use super::{Item, Kind, labelled, parse, split};

    const REPLY: &str = "Plan below.\n\nMore detail.\n\n---\n\n## 80/20\n\n✅ Result: **Plan ready**; no code \
changed yet.\n❓ Decide:\n1. **Go ahead with the plan?**\n2. **Copy entries always?**\n⚠️ **Watch out:** the \
migration is slow.\n🔀 Git: nothing changed.\n";

    #[test]
    fn finds_the_section_with_its_rule() {
        let at = split(REPLY).expect("a summary");
        assert_eq!(&REPLY[..at], "Plan below.\n\nMore detail.\n\n", "the rule goes with the card");
        assert_eq!(split("No summary here.\n---\nJust a rule."), None);
        assert_eq!(split("## 80/20\n✅ Result: done"), Some(0), "a heading alone, without a rule");
        let fenced = "Write it like this:\n```md\n## 80/20\n✅ Result: x\n```\nThat's all.";
        assert_eq!(split(fenced), None, "a heading inside a code block is not a summary");
        let streaming = "Done.\n\n---\n\n## 80/20\n\n✅ Res";
        assert_eq!(split(streaming), Some(7), "found while the items are still arriving");
    }

    #[test]
    fn items_lose_their_emoji_and_label_and_keep_their_lines() {
        let at = split(REPLY).unwrap();
        let items = parse(&REPLY[at..]);
        let kinds: Vec<Kind> = items.iter().map(|i| i.kind).collect();
        assert_eq!(kinds, [Kind::Result, Kind::Decide, Kind::WatchOut, Kind::Git]);
        assert_eq!(items[0].text, "**Plan ready**; no code changed yet.");
        assert_eq!(items[1].text, "1. **Go ahead with the plan?**\n2. **Copy entries always?**");
        assert_eq!(items[2].text, "the migration is slow.", "a bold label is a label");
        assert_eq!(items[3].text, "nothing changed.");
    }

    #[test]
    fn labels_without_emoji_and_stray_text() {
        let items = parse("## 80/20\nIn short:\nResult: built\nnext: **run it**\n");
        assert_eq!(items[0], Item { kind: Kind::Note, text: "In short:".into() });
        assert_eq!(items[1], Item { kind: Kind::Result, text: "built".into() });
        assert_eq!(items[2], Item { kind: Kind::Next, text: "**run it**".into() });
        assert!(labelled("Results are in: fine").is_none(), "a word that only starts with a label");
    }
}
