//! Claude's questions (AskUserQuestion), answered in a modal over the conversation.
//!
//! The modal lists every question. The open one shows its options and an "Other" field; the
//! others fold to one line, with the answer once there is one. Answering a question folds it
//! and opens the next unanswered one. With several questions, a last review step sends them
//! all; a single question is sent as soon as it's answered.
//!
//! Everything works from the keyboard (ROADMAP.md, "Keyboard rules"): Up/Down move, 1-9 pick,
//! Space ticks a multi-select option, Enter chooses (or finishes a multi-select), Left/Right
//! go to the previous or next question, and Escape leaves the "Other" field or closes the
//! modal. Closing leaves the question waiting; its card in the conversation opens it again.

use std::collections::{BTreeMap, BTreeSet};

use gpui_kit::base::{h_flex, v_flex};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Sizable as _,
    button::{Button, ButtonVariants as _},
    input::{Input, InputEvent, InputState},
};
use gpui_kit::{TestSupportExt as _, prelude::FluentBuilder as _, *};
use serde_json::Value;

use super::{ApprovalKind, ApprovalRow, Row, ThreadView};
use crate::agent::Decision;

/// Key context of the open question's option list. The "Other" field sits outside it, so
/// typing there (Space, arrows) never reaches these bindings.
const LIST_CONTEXT: &str = "QuestionList";
/// Key context of the whole modal, the "Other" field included.
const MODAL_CONTEXT: &str = "QuestionModal";
const MODAL_WIDTH: f32 = 640.;

gpui_kit::actions!(questions, [Up, Down, Toggle, Confirm, Previous, Next, Close]);

pub fn bindings() -> Vec<KeyBinding> {
    let list = Some(LIST_CONTEXT);
    vec![
        KeyBinding::new("up", Up, list),
        KeyBinding::new("down", Down, list),
        KeyBinding::new("space", Toggle, list),
        KeyBinding::new("enter", Confirm, list),
        KeyBinding::new("left", Previous, list),
        KeyBinding::new("right", Next, list),
        // The "Other" field lets Escape through, so this leaves the field there, and closes
        // the modal from the list.
        KeyBinding::new("escape", Close, Some(MODAL_CONTEXT)),
    ]
}

pub(super) struct Question {
    pub question: String,
    pub header: String,
    /// (label, description)
    pub options: Vec<(String, String)>,
    pub multi: bool,
}

pub(super) struct QuestionCard {
    pub questions: Vec<Question>,
    /// The options picked for each question.
    pub picked: Vec<BTreeSet<usize>>,
    /// An "Other" answer for each question, created when the modal is first drawn.
    pub other: Vec<Option<Entity<InputState>>>,
    /// The open question; `questions.len()` is the review step before sending.
    pub current: usize,
    /// The highlighted row of each question; `options.len()` is its "Other" field.
    cursor: Vec<usize>,
    /// Closed with Escape: the question still waits, and its card opens the modal again.
    pub dismissed: bool,
    /// Take the keyboard on the next frame. `Some(false)` only when the prompt box has it
    /// (or nothing does), so a question arriving while you type in a terminal doesn't
    /// steal the keys; `Some(true)` always, for a modal you opened yourself.
    claim_focus: Option<bool>,
    focus: FocusHandle,
}

/// What a key or click in the modal asks for.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Nav {
    Up,
    Down,
    Toggle,
    Confirm,
    Previous,
    Next,
    /// Pick option `n` of the open question (1-9 on the keyboard, or a click).
    Pick(usize),
    /// Open question `n` (a click on its folded row).
    Open(usize),
}

/// What the thread should do after the card moved.
#[derive(Debug, PartialEq)]
pub(super) enum After {
    Stay,
    /// Put the keyboard in the open question's "Other" field.
    FocusOther,
    Send,
}

impl QuestionCard {
    pub fn parse(input: &Value, cx: &mut App) -> Option<Self> {
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
            cursor: vec![0; questions.len()],
            current: 0,
            dismissed: false,
            claim_focus: Some(false),
            focus: cx.focus_handle(),
            questions,
        })
    }

    /// Question `qi`'s answer so far: what's typed under "Other", else the picked labels.
    pub fn answer(&self, qi: usize, cx: &App) -> Option<String> {
        let q = self.questions.get(qi)?;
        let other = self.other[qi].as_ref().map(|o| o.read(cx).value().trim().to_string()).unwrap_or_default();
        let answer = if !other.is_empty() {
            other
        } else {
            self.picked[qi].iter().map(|oi| q.options[*oi].0.clone()).collect::<Vec<_>>().join(", ")
        };
        (!answer.is_empty()).then_some(answer)
    }

    /// Question text -> answer, when every question has one.
    pub fn answers(&self, cx: &App) -> Option<BTreeMap<String, String>> {
        (0..self.questions.len()).map(|qi| Some((self.questions[qi].question.clone(), self.answer(qi, cx)?))).collect()
    }

    /// The open question's highlighted row, if a question (not the review step) is open.
    pub fn cursor(&self) -> Option<usize> {
        self.cursor.get(self.current).copied()
    }

    fn on_other(&self) -> bool {
        self.questions.get(self.current).is_some_and(|q| self.cursor[self.current] == q.options.len())
    }

    pub fn open(&mut self, force: bool) {
        self.dismissed = false;
        self.claim_focus = Some(force);
    }

    /// Move through the card. `cx` reads the "Other" fields.
    pub fn nav(&mut self, nav: Nav, cx: &App) -> After {
        let count = self.questions.len();
        match nav {
            Nav::Up | Nav::Down => {
                if let Some(q) = self.questions.get(self.current) {
                    // The rows are the options and then "Other".
                    let rows = q.options.len() + 1;
                    let row = &mut self.cursor[self.current];
                    *row = if nav == Nav::Up { (*row + rows - 1) % rows } else { (*row + 1) % rows };
                    if self.on_other() {
                        return After::FocusOther;
                    }
                }
                After::Stay
            }
            Nav::Toggle => match self.cursor() {
                Some(_) if self.on_other() => After::FocusOther,
                Some(row) => {
                    self.pick(row);
                    After::Stay
                }
                None => After::Stay,
            },
            Nav::Pick(oi) => {
                let Some(q) = self.questions.get(self.current) else { return After::Stay };
                if oi >= q.options.len() {
                    return After::Stay;
                }
                let multi = q.multi;
                self.cursor[self.current] = oi;
                self.pick(oi);
                if multi { After::Stay } else { self.advance(cx) }
            }
            Nav::Confirm => {
                if self.current == count {
                    return if self.answers(cx).is_some() { After::Send } else { After::Stay };
                }
                if self.on_other() {
                    // Enter in the "Other" field: done when something's typed.
                    return if self.answer(self.current, cx).is_some() { self.advance(cx) } else { After::FocusOther };
                }
                let (row, multi) = (self.cursor[self.current], self.questions[self.current].multi);
                // Enter chooses the highlighted option; in a multi-select it finishes the
                // question, picking the highlighted one only if nothing is ticked yet.
                if !multi || self.picked[self.current].is_empty() {
                    self.pick(row);
                }
                self.advance(cx)
            }
            Nav::Previous => {
                self.current = self.current.saturating_sub(1);
                After::Stay
            }
            Nav::Next => {
                // The review step comes after the last question once there's something to send.
                let last = if count > 1 && self.answers(cx).is_some() { count } else { count - 1 };
                self.current = (self.current + 1).min(last);
                After::Stay
            }
            Nav::Open(qi) => {
                self.current = qi.min(count);
                After::Stay
            }
        }
    }

    /// Pick (or, for a multi-select, toggle) option `oi` of the open question.
    fn pick(&mut self, oi: usize) {
        let (Some(q), Some(picked)) = (self.questions.get(self.current), self.picked.get_mut(self.current)) else {
            return;
        };
        if q.multi {
            if !picked.remove(&oi) {
                picked.insert(oi);
            }
        } else {
            picked.clear();
            picked.insert(oi);
        }
    }

    /// The open question is answered: open the next unanswered one, or the review step.
    /// A lone question is sent straight away.
    fn advance(&mut self, cx: &App) -> After {
        let count = self.questions.len();
        if self.answer(self.current, cx).is_none() {
            return After::Stay;
        }
        let unanswered = |qi: &usize| self.answer(*qi, cx).is_none();
        if let Some(next) = (self.current + 1..count).chain(0..self.current).find(unanswered) {
            self.current = next;
            return After::Stay;
        }
        if count == 1 {
            return After::Send;
        }
        self.current = count;
        After::Stay
    }
}

impl ThreadView {
    /// The first question card still waiting for an answer.
    fn waiting_question(&self) -> Option<usize> {
        self.rows.iter().position(|r| {
            matches!(r, Row::Approval(ApprovalRow { kind: ApprovalKind::Question(_), outcome: None, .. }))
        })
    }

    fn question_card(&mut self, ix: usize) -> Option<&mut QuestionCard> {
        match self.rows.get_mut(ix) {
            Some(Row::Approval(ApprovalRow { kind: ApprovalKind::Question(card), outcome: None, .. })) => Some(card),
            _ => None,
        }
    }

    /// Open the modal for card `ix` again (its card's "Answer…" button).
    pub(super) fn open_questions(&mut self, ix: usize, cx: &mut Context<Self>) {
        if let Some(card) = self.question_card(ix) {
            card.open(true);
        }
        cx.notify();
    }

    /// A key or click in the modal for card `ix`.
    pub(super) fn question_nav(&mut self, ix: usize, nav: Nav, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Row::Approval(ApprovalRow { kind: ApprovalKind::Question(card), outcome: None, .. })) =
            self.rows.get_mut(ix)
        else {
            return;
        };
        let after = card.nav(nav, cx);
        match after {
            After::FocusOther => {
                if let Some(Some(other)) = card.other.get(card.current) {
                    other.update(cx, |o, cx| o.focus(window, cx));
                }
            }
            // Clicks land here too, so the list keeps the keyboard after one.
            After::Stay => window.focus(&card.focus, cx),
            After::Send => {}
        }
        if after == After::Send {
            self.submit_answers(ix, cx);
            self.leave_questions(window, cx);
        }
        self.remeasure(ix, cx);
    }

    /// Escape: out of the "Other" field, or else close the modal (the question still waits).
    fn close_questions(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(card) = self.question_card(ix) else { return };
        let in_other = card.other.iter().flatten().any(|o| o.focus_handle(cx).is_focused(window));
        if in_other {
            window.focus(&card.focus, cx);
        } else {
            card.dismissed = true;
            self.leave_questions(window, cx);
        }
        cx.notify();
    }

    fn skip_questions(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.decide(ix, Decision::Decline { feedback: None }, cx);
        self.leave_questions(window, cx);
    }

    /// Give the keyboard back to the prompt box.
    fn leave_questions(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let prompt = self.prompt.focus_handle(cx);
        window.focus(&prompt, cx);
    }

    /// The modal over the conversation, while a question waits and hasn't been closed.
    pub(super) fn render_questions(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        let ix = self.waiting_question()?;
        let prompt = self.prompt.focus_handle(cx);
        let Some(Row::Approval(ApprovalRow { kind: ApprovalKind::Question(card), .. })) = self.rows.get_mut(ix) else {
            return None;
        };
        if card.dismissed {
            return None;
        }
        for qi in 0..card.other.len() {
            if card.other[qi].is_none() {
                let input = cx.new(|cx| InputState::new(window, cx).placeholder("Type your own answer"));
                cx.subscribe_in(&input, window, move |this, _, event: &InputEvent, window, cx| match event {
                    InputEvent::PressEnter { .. } => this.question_nav(ix, Nav::Confirm, window, cx),
                    InputEvent::Change => cx.notify(),
                    // Clicking into a folded question's field isn't possible, so focus here
                    // means this question's "Other" row.
                    InputEvent::Focus => {
                        if let Some(card) = this.question_card(ix)
                            && let Some(q) = card.questions.get(qi)
                        {
                            card.current = qi;
                            card.cursor[qi] = q.options.len();
                        }
                        cx.notify();
                    }
                    _ => {}
                })
                .detach();
                card.other[qi] = Some(input);
            }
        }
        if let Some(force) = card.claim_focus.take() {
            let free = window.focused(cx).is_none_or(|f| f == prompt);
            if force || free {
                window.focus(&card.focus, cx);
            }
        }

        let theme = cx.theme();
        let (fg, muted, secondary, accent, border) =
            (theme.foreground, theme.muted_foreground, theme.secondary, theme.primary, theme.border);
        let (popover, overlay) = (theme.popover, theme.overlay);
        let card = match &self.rows[ix] {
            Row::Approval(ApprovalRow { kind: ApprovalKind::Question(card), .. }) => card,
            _ => return None,
        };
        let count = card.questions.len();
        let reviewing = card.current == count;
        let complete = card.answers(cx).is_some();

        let chip = |text: &str| {
            div().flex_none().px_1p5().rounded_sm().bg(secondary).text_xs().text_color(muted).child(text.to_string())
        };

        let rows = card.questions.iter().enumerate().map(|(qi, q)| {
            if qi != card.current {
                // Folded: the question on one line, and its answer once it has one.
                let answer = card.answer(qi, cx);
                return h_flex()
                    .id(SharedString::from(format!("question-row-{qi}")))
                    .test_support()
                    .w_full()
                    .gap_2()
                    .px_2()
                    .py_1p5()
                    .rounded_md()
                    .cursor_pointer()
                    .hover(|el| el.bg(secondary))
                    .on_click(cx.listener(move |this, _, window, cx| this.question_nav(ix, Nav::Open(qi), window, cx)))
                    .child(
                        div()
                            .flex_none()
                            .text_color(if answer.is_some() { accent } else { muted })
                            .child(if answer.is_some() { "✓" } else { "○" }),
                    )
                    .when(!q.header.is_empty(), |el| el.child(chip(&q.header)))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .text_sm()
                            .text_color(muted)
                            .child(q.question.clone()),
                    )
                    .when_some(answer, |el, answer| {
                        el.child(
                            div()
                                .flex_none()
                                .max_w(relative(0.45))
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_ellipsis()
                                .text_sm()
                                .text_color(fg)
                                .child(answer),
                        )
                    })
                    .into_any_element();
            }

            let cursor = card.cursor[qi];
            let options = q.options.iter().enumerate().map(|(oi, (label, description))| {
                let picked = card.picked[qi].contains(&oi);
                let mark = match (q.multi, picked) {
                    (true, true) => "☑",
                    (true, false) => "☐",
                    (false, true) => "●",
                    (false, false) => "○",
                };
                h_flex()
                    .id(SharedString::from(format!("question-option-{qi}-{oi}")))
                    .test_support()
                    .gap_2()
                    .items_start()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .border_1()
                    .border_color(if cursor == oi { accent } else { transparent_black() })
                    .cursor_pointer()
                    .hover(|el| el.bg(secondary))
                    .when(picked, |el| el.bg(secondary))
                    .on_click(cx.listener(move |this, _, window, cx| this.question_nav(ix, Nav::Pick(oi), window, cx)))
                    .child(div().w(px(14.)).flex_none().text_xs().text_color(muted).pt_0p5().child(if oi < 9 {
                        format!("{}", oi + 1)
                    } else {
                        String::new()
                    }))
                    .child(div().flex_none().text_color(if picked { accent } else { muted }).child(mark))
                    .child(
                        v_flex()
                            .min_w_0()
                            .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).child(label.clone()))
                            .when(!description.is_empty(), |el| {
                                el.child(div().text_xs().text_color(muted).child(description.clone()))
                            }),
                    )
            });
            let on_other = cursor == q.options.len();
            v_flex()
                .gap_1()
                .child(
                    h_flex()
                        .gap_2()
                        .when(!q.header.is_empty(), |el| el.child(chip(&q.header)))
                        .child(div().text_sm().text_color(fg).child(q.question.clone())),
                )
                .child(
                    v_flex()
                        .id(SharedString::from(format!("question-list-{qi}")))
                        .key_context(LIST_CONTEXT)
                        .track_focus(&card.focus)
                        .gap_0p5()
                        .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                            let keystroke = &event.keystroke;
                            if keystroke.modifiers.modified() {
                                return;
                            }
                            if let Some(n) = keystroke.key.parse::<usize>().ok().filter(|n| (1..=9).contains(n)) {
                                this.question_nav(ix, Nav::Pick(n - 1), window, cx);
                                cx.stop_propagation();
                            }
                        }))
                        .children(options),
                )
                .when_some(card.other[qi].as_ref(), |el, o| {
                    el.child(
                        div()
                            .rounded_md()
                            .border_1()
                            .border_color(if on_other { accent } else { transparent_black() })
                            .child(Input::new(o).small())
                            .child(crate::caret::overlay(o)),
                    )
                })
                .into_any_element()
        });

        let hint = if reviewing {
            "Enter sends · ← back · Esc closes"
        } else if card.questions[card.current].multi {
            "↑↓ move · Space ticks · Enter done · ←→ questions · Esc closes"
        } else {
            "↑↓ move · 1–9 or Enter picks · ←→ questions · Esc closes"
        };
        let step = if reviewing {
            "Ready to send".to_string()
        } else if count > 1 {
            format!("Question {} of {count}", card.current + 1)
        } else {
            String::new()
        };

        let panel = v_flex()
            .id("question-modal")
            .test_support()
            .key_context(MODAL_CONTEXT)
            .on_action(cx.listener(move |this, _: &Up, window, cx| this.question_nav(ix, Nav::Up, window, cx)))
            .on_action(cx.listener(move |this, _: &Down, window, cx| this.question_nav(ix, Nav::Down, window, cx)))
            .on_action(cx.listener(move |this, _: &Toggle, window, cx| this.question_nav(ix, Nav::Toggle, window, cx)))
            .on_action(
                cx.listener(move |this, _: &Confirm, window, cx| this.question_nav(ix, Nav::Confirm, window, cx)),
            )
            .on_action(
                cx.listener(move |this, _: &Previous, window, cx| this.question_nav(ix, Nav::Previous, window, cx)),
            )
            .on_action(cx.listener(move |this, _: &Next, window, cx| this.question_nav(ix, Nav::Next, window, cx)))
            .on_action(cx.listener(move |this, _: &Close, window, cx| this.close_questions(ix, window, cx)))
            // A click on the panel doesn't reach the backdrop, which would close it.
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .w_full()
            .max_w(px(MODAL_WIDTH))
            .max_h(relative(0.85))
            .overflow_y_scroll()
            .gap_2()
            .p_4()
            .rounded_lg()
            .border_1()
            .border_color(accent)
            .bg(popover)
            .shadow_lg()
            .child(
                h_flex()
                    .justify_between()
                    .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).child("Claude is asking"))
                    .child(div().text_xs().text_color(muted).child(step)),
            )
            .children(rows)
            .child(
                // Only the list's focus handle sits on the open question, so the review step
                // keeps it on this row instead.
                h_flex()
                    .pt_1()
                    .gap_2()
                    .border_t_1()
                    .border_color(border)
                    .when(reviewing, |el| el.key_context(LIST_CONTEXT).track_focus(&card.focus))
                    .child(div().flex_1().pt_1().text_xs().text_color(muted).child(hint))
                    .child(
                        Button::new("question-skip")
                            .ghost()
                            .small()
                            .label("Skip")
                            .on_click(cx.listener(move |this, _, window, cx| this.skip_questions(ix, window, cx))),
                    )
                    .child(
                        Button::new("question-send")
                            .primary()
                            .small()
                            .label("Send answers")
                            .disabled(!complete)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.submit_answers(ix, cx);
                                this.leave_questions(window, cx);
                            })),
                    ),
            );

        // A shaded backdrop over the conversation only; a click on it closes the modal.
        Some(
            div()
                .id("question-backdrop")
                .absolute()
                .inset_0()
                .occlude()
                .bg(overlay)
                .flex()
                .items_center()
                .justify_center()
                .p_6()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _, window, cx| {
                        if let Some(card) = this.question_card(ix) {
                            card.dismissed = true;
                        }
                        this.leave_questions(window, cx);
                        cx.notify();
                    }),
                )
                .child(panel)
                .into_any_element(),
        )
    }
}
