//! Line comments on a diff, drawn the same way wherever a diff takes them: the GitHub tab's
//! Files changed (comments that go out with a review) and the local review (comments sent to
//! the agent). A comment waiting to go out is a yellow "Pending" card under its last line; one
//! being written is a box with its buttons.

use gpui_kit::assets::IconName;
use gpui_kit::base::{h_flex, v_flex};
use gpui_kit::component::{
    ActiveTheme as _, Sizable as _,
    button::{Button, ButtonVariants as _},
    input::{Textarea, TextareaState},
    text::TextView,
};
use gpui_kit::{prelude::FluentBuilder as _, *};

/// The yellow a pending comment is drawn in: the terminal palette's, so it follows the theme.
pub fn pending_color(cx: &App) -> Hsla {
    crate::terminal::palette(cx).0[3]
}

/// "Line 12" or "Lines 12–14".
pub fn lines_label(start: u64, line: u64) -> String {
    if start < line { format!("Lines {start}–{line}") } else { format!("Line {line}") }
}

/// A card under a diff line, outlined in `accent`.
pub fn frame(accent: Hsla, cx: &App) -> Div {
    let theme = cx.theme();
    v_flex()
        .mx_3()
        .my_2()
        .rounded_lg()
        .border_1()
        .border_color(accent.opacity(0.4))
        .bg(theme.secondary)
        .overflow_hidden()
        .font_family(theme.font_family.clone())
}

pub fn pill(text: &str, color: Hsla) -> Div {
    div()
        .flex_none()
        .px_1p5()
        .rounded_full()
        .border_1()
        .border_color(color.opacity(0.45))
        .text_color(color)
        .text_xs()
        .child(text.to_string())
}

/// A comment waiting to go out: "Pending", the lines it's on, its text and a button that
/// removes it. `ix` tells cards (and their buttons) apart.
pub fn pending_card(
    ix: usize,
    lines: String,
    body: String,
    remove: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    cx: &App,
) -> AnyElement {
    let (yellow, muted) = (pending_color(cx), cx.theme().muted_foreground);
    frame(yellow, cx)
        .child(
            h_flex()
                .gap_2()
                .px_3()
                .py_1()
                .bg(yellow.opacity(0.10))
                .text_xs()
                .child(pill("Pending", yellow))
                .child(div().text_color(muted).child(lines))
                .child(div().flex_1())
                .child(
                    Button::new(SharedString::from(format!("drop-draft-{ix}")))
                        .xsmall()
                        .ghost()
                        .icon(IconName::Trash)
                        .tooltip("Remove this comment")
                        .on_click(remove),
                ),
        )
        .child(
            div()
                .px_3()
                .py_2()
                .text_sm()
                .child(TextView::markdown(SharedString::from(format!("draft-body-{ix}")), body)),
        )
        .into_any_element()
}

/// The box a comment is written in, with `buttons` under it (right-aligned, in order) and,
/// if given, a hint at the left saying which keys do what.
pub fn compose_card(
    input: &Entity<TextareaState>,
    hint: Option<&str>,
    buttons: impl IntoIterator<Item = Button>,
    cx: &App,
) -> AnyElement {
    let muted = cx.theme().muted_foreground;
    frame(cx.theme().primary, cx)
        .p_2()
        .gap_2()
        .child(Textarea::new(input))
        .child(
            h_flex()
                .gap_2()
                .when_some(hint, |el, hint| el.child(div().text_xs().text_color(muted).child(hint.to_string())))
                .child(div().flex_1())
                .children(buttons),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::lines_label;

    #[test]
    fn one_line_or_a_range() {
        assert_eq!(lines_label(12, 12), "Line 12");
        assert_eq!(lines_label(12, 14), "Lines 12–14");
    }
}
