//! An MCP server asking the user for input through Claude (an MCP "elicitation"). Claude
//! waits until it's answered.
//!
//! A request is either a form, whose fields come from the server's schema (text, numbers,
//! yes/no, one choice or several), or a link to open, such as a sign-in page. The card in the
//! conversation shows the server's message and the form or the link, with **Submit** (or
//! **Open link**), **Decline** and **Cancel**. Enter in a text field submits.

use std::collections::BTreeSet;

use gpui_kit::base::{h_flex, v_flex};
use gpui_kit::component::{
    ActiveTheme as _, Sizable as _,
    button::{Button, ButtonVariants as _},
    input::{Input, InputEvent, InputState},
    switch::Switch,
};
use gpui_kit::{prelude::FluentBuilder as _, *};
use serde_json::{Map, Value, json};

use super::{ApprovalKind, ApprovalRow, Row, ThreadView};
use crate::agent::{Decision, ElicitAction};

pub(super) struct ElicitationCard {
    /// Who's asking: the server's display name, else its name.
    pub server: String,
    pub title: Option<String>,
    pub message: String,
    /// A link to open rather than a form to fill in.
    pub url: Option<String>,
    pub fields: Vec<Field>,
    /// Why the form can't be sent yet.
    pub error: Option<String>,
}

pub(super) struct Field {
    pub key: String,
    pub label: String,
    pub description: Option<String>,
    pub required: bool,
    pub kind: FieldKind,
    /// The text field of a text or number field, created when the card is first drawn.
    pub input: Option<Entity<InputState>>,
}

pub(super) enum FieldKind {
    Text {
        default: Option<String>,
        min: Option<usize>,
        max: Option<usize>,
    },
    Number {
        integer: bool,
        default: Option<f64>,
        min: Option<f64>,
        max: Option<f64>,
    },
    Bool(bool),
    /// (value, label) pairs; one may be picked.
    Choice {
        options: Vec<(String, String)>,
        picked: Option<usize>,
    },
    /// (value, label) pairs; any number may be picked.
    Multi {
        options: Vec<(String, String)>,
        picked: BTreeSet<usize>,
    },
}

impl ElicitationCard {
    /// The card for a request as Claude sent it.
    pub fn parse(request: &Value) -> Self {
        let text = |key: &str| request.get(key).and_then(Value::as_str).filter(|s| !s.trim().is_empty());
        let server = text("display_name").or(text("mcp_server_name")).unwrap_or("An MCP server").to_string();
        let url = (text("mode") == Some("url")).then(|| text("url").unwrap_or_default().to_string());
        let schema = &request["requested_schema"];
        let properties = schema["properties"].as_object();
        let required: BTreeSet<&str> =
            schema["required"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
        // The order the server listed its fields in, else the schema's own.
        let mut keys: Vec<String> = request["property_order"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect();
        for key in properties.into_iter().flat_map(|p| p.keys()) {
            if !keys.contains(key) {
                keys.push(key.clone());
            }
        }
        let fields = if url.is_some() {
            Vec::new()
        } else {
            keys.iter()
                .filter_map(|key| {
                    let property = properties?.get(key)?;
                    Some(Field {
                        key: key.clone(),
                        label: property["title"].as_str().unwrap_or(key).to_string(),
                        description: property["description"].as_str().map(str::to_owned),
                        required: required.contains(key.as_str()),
                        kind: field_kind(property),
                        input: None,
                    })
                })
                .collect()
        };
        Self {
            server,
            title: text("title").map(str::to_owned),
            message: text("message").unwrap_or_default().to_string(),
            url,
            fields,
            error: None,
        }
    }

    /// One line for a notification: who's asking, and for what.
    pub fn summary(&self) -> String {
        match &self.url {
            Some(url) => format!("{} wants you to open {}", self.server, host(url)),
            None => format!("{}: {}", self.server, self.message),
        }
    }

    /// The form's values as the server asked for them, or why they can't be sent.
    pub fn content(&self, cx: &App) -> Result<Value, String> {
        let mut content = Map::new();
        for field in &self.fields {
            let typed = field.input.as_ref().map(|i| i.read(cx).value().trim().to_string()).unwrap_or_default();
            let value = match &field.kind {
                FieldKind::Text { min, max, .. } => {
                    let chars = typed.chars().count();
                    if !typed.is_empty() && min.is_some_and(|m| chars < m) {
                        return Err(format!("{} needs at least {} characters.", field.label, min.unwrap_or(0)));
                    }
                    if max.is_some_and(|m| chars > m) {
                        return Err(format!("{} takes at most {} characters.", field.label, max.unwrap_or(0)));
                    }
                    (!typed.is_empty()).then(|| json!(typed))
                }
                FieldKind::Number { integer, min, max, .. } if !typed.is_empty() => {
                    let number: f64 = typed.parse().map_err(|_| format!("{} must be a number.", field.label))?;
                    if *integer && number.fract() != 0.0 {
                        return Err(format!("{} must be a whole number.", field.label));
                    }
                    if min.is_some_and(|m| number < m) || max.is_some_and(|m| number > m) {
                        let range = match (min, max) {
                            (Some(a), Some(b)) => format!("between {a} and {b}"),
                            (Some(a), None) => format!("at least {a}"),
                            (None, Some(b)) => format!("at most {b}"),
                            (None, None) => String::new(),
                        };
                        return Err(format!("{} must be {range}.", field.label));
                    }
                    Some(if *integer { json!(number as i64) } else { json!(number) })
                }
                FieldKind::Number { .. } => None,
                FieldKind::Bool(on) => Some(json!(on)),
                FieldKind::Choice { options, picked } => picked.map(|i| json!(options[i].0)),
                FieldKind::Multi { options, picked } => (!picked.is_empty() || !field.required)
                    .then(|| json!(picked.iter().map(|&i| options[i].0.clone()).collect::<Vec<_>>())),
            };
            match value {
                Some(value) => {
                    content.insert(field.key.clone(), value);
                }
                None if field.required => return Err(format!("{} is needed.", field.label)),
                None => {}
            }
        }
        Ok(Value::Object(content))
    }
}

/// How a property of the server's schema is asked for.
fn field_kind(property: &Value) -> FieldKind {
    let number = |key: &str| property.get(key).and_then(Value::as_f64);
    let options = |property: &Value| -> Option<Vec<(String, String)>> {
        // `enum`, with `enumNames` as its labels (the older form), or `oneOf` / `anyOf` of
        // titled constants.
        if let Some(values) = property["enum"].as_array() {
            let names = property["enumNames"].as_array();
            return Some(
                values
                    .iter()
                    .enumerate()
                    .filter_map(|(i, v)| {
                        let value = v.as_str()?.to_string();
                        let label = names.and_then(|n| n.get(i)).and_then(Value::as_str).unwrap_or(&value).to_string();
                        Some((value, label))
                    })
                    .collect(),
            );
        }
        let choices = property["oneOf"].as_array().or(property["anyOf"].as_array())?;
        Some(
            choices
                .iter()
                .filter_map(|c| {
                    let value = c["const"].as_str()?.to_string();
                    Some((value.clone(), c["title"].as_str().unwrap_or(&value).to_string()))
                })
                .collect(),
        )
    };
    match property["type"].as_str() {
        Some("boolean") => FieldKind::Bool(property["default"].as_bool().unwrap_or(false)),
        Some(kind @ ("number" | "integer")) => FieldKind::Number {
            integer: kind == "integer",
            default: number("default"),
            min: number("minimum"),
            max: number("maximum"),
        },
        Some("array") => {
            let options = options(&property["items"]).unwrap_or_default();
            let defaults: Vec<&str> =
                property["default"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
            let picked =
                options.iter().enumerate().filter(|(_, (v, _))| defaults.contains(&v.as_str())).map(|(i, _)| i);
            FieldKind::Multi { picked: picked.collect(), options }
        }
        _ => match options(property) {
            Some(options) => {
                let default = property["default"].as_str();
                let picked = options.iter().position(|(v, _)| Some(v.as_str()) == default);
                FieldKind::Choice { options, picked }
            }
            None => FieldKind::Text {
                default: property["default"].as_str().map(str::to_owned),
                min: number("minLength").map(|n| n as usize),
                max: number("maxLength").map(|n| n as usize),
            },
        },
    }
}

/// The site a link goes to, which is what the user should check before opening it.
fn host(url: &str) -> &str {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    rest.split(['/', '?', '#']).next().unwrap_or(rest)
}

impl ThreadView {
    fn elicitation_card(&mut self, ix: usize) -> Option<&mut ElicitationCard> {
        match self.rows.get_mut(ix) {
            Some(Row::Approval(ApprovalRow { kind: ApprovalKind::Elicitation(card), outcome: None, .. })) => Some(card),
            _ => None,
        }
    }

    #[cfg(test)]
    pub fn elicitation_input(&self, ix: usize, field: usize) -> Option<Entity<InputState>> {
        match self.rows.get(ix) {
            Some(Row::Approval(ApprovalRow { kind: ApprovalKind::Elicitation(card), .. })) => {
                card.fields.get(field)?.input.clone()
            }
            _ => None,
        }
    }

    #[cfg(test)]
    pub fn elicitation_error(&self, ix: usize) -> Option<String> {
        match self.rows.get(ix) {
            Some(Row::Approval(ApprovalRow { kind: ApprovalKind::Elicitation(card), .. })) => card.error.clone(),
            _ => None,
        }
    }

    /// Create the text fields of a waiting form, which need a window.
    pub(super) fn ensure_elicitation_inputs(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(card) = self.elicitation_card(ix) else { return };
        let mut created = Vec::new();
        for (fi, field) in card.fields.iter_mut().enumerate() {
            let default = match &field.kind {
                FieldKind::Text { default, .. } => default.clone(),
                FieldKind::Number { default, .. } => default.map(|n| n.to_string()),
                _ => continue,
            };
            if field.input.is_some() {
                continue;
            }
            let placeholder = field.description.clone().unwrap_or_default();
            let input = cx.new(|cx| {
                let mut state = InputState::new(window, cx).placeholder(placeholder);
                if let Some(default) = default {
                    state.set_value(default, window, cx);
                }
                state
            });
            field.input = Some(input.clone());
            created.push((fi, input));
        }
        for (_, input) in created {
            cx.subscribe_in(&input, window, move |this, _, event: &InputEvent, _, cx| {
                if let InputEvent::PressEnter { .. } = event {
                    this.submit_elicitation(ix, cx);
                }
            })
            .detach();
        }
    }

    /// Send the form, or say what's missing.
    fn submit_elicitation(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(card) = self.elicitation_card(ix) else { return };
        match card.content(cx) {
            Ok(content) => {
                let decision = Decision::Elicit { action: ElicitAction::Accept, content: Some(content) };
                self.decide(ix, decision, cx);
            }
            Err(error) => {
                card.error = Some(error);
                self.remeasure(ix, cx);
            }
        }
    }

    /// Open the link the server asked for, and tell it the user went.
    fn open_elicitation_link(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(url) = self.elicitation_card(ix).and_then(|card| card.url.clone()) else { return };
        cx.open_url(&url);
        self.decide(ix, Decision::Elicit { action: ElicitAction::Accept, content: None }, cx);
    }

    /// Change a yes/no field, or pick (or for several, tick) a choice.
    fn pick_elicitation(&mut self, ix: usize, fi: usize, option: usize, cx: &mut Context<Self>) {
        let Some(card) = self.elicitation_card(ix) else { return };
        let Some(field) = card.fields.get_mut(fi) else { return };
        match &mut field.kind {
            FieldKind::Bool(on) => *on = option == 1,
            FieldKind::Choice { picked, .. } => *picked = Some(option),
            FieldKind::Multi { picked, .. } => {
                if !picked.remove(&option) {
                    picked.insert(option);
                }
            }
            _ => {}
        }
        card.error = None;
        self.remeasure(ix, cx);
    }

    pub(super) fn render_elicitation(
        &self,
        ix: usize,
        card: &ElicitationCard,
        outcome: Option<&String>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        let (border, muted, secondary, accent, danger) =
            (theme.border, theme.muted_foreground, theme.secondary, theme.primary, theme.danger);
        let mono = theme.mono_font_family.clone();
        let heading = card.title.clone().unwrap_or_else(|| format!("{} needs your input", card.server));
        let id = |name: &str| SharedString::from(format!("elicit-{name}-{ix}"));

        let field = |fi: usize, field: &Field, cx: &mut Context<Self>| {
            let label = h_flex()
                .gap_1()
                .text_sm()
                .child(field.label.clone())
                .when(field.required, |el| el.child(div().text_color(danger).child("*")));
            let control = match &field.kind {
                FieldKind::Text { .. } | FieldKind::Number { .. } => field.input.as_ref().map(|input| {
                    div().child(Input::new(input).small()).child(crate::caret::overlay(input)).into_any_element()
                }),
                FieldKind::Bool(on) => Some(
                    Switch::new(SharedString::from(format!("elicit-field-{ix}-{fi}")))
                        .checked(*on)
                        .small()
                        .on_click(cx.listener(move |this, checked: &bool, _, cx| {
                            this.pick_elicitation(ix, fi, usize::from(*checked), cx)
                        }))
                        .into_any_element(),
                ),
                FieldKind::Choice { options, .. } | FieldKind::Multi { options, .. } => {
                    let is_picked = |i: usize| match &field.kind {
                        FieldKind::Choice { picked, .. } => *picked == Some(i),
                        FieldKind::Multi { picked, .. } => picked.contains(&i),
                        _ => false,
                    };
                    Some(
                        h_flex()
                            .flex_wrap()
                            .gap_1()
                            .children(options.iter().enumerate().map(|(oi, (_, label))| {
                                Button::new(SharedString::from(format!("elicit-option-{ix}-{fi}-{oi}")))
                                    .small()
                                    .label(label.clone())
                                    .when(is_picked(oi), |b| b.primary())
                                    .on_click(cx.listener(move |this, _, _, cx| this.pick_elicitation(ix, fi, oi, cx)))
                            }))
                            .into_any_element(),
                    )
                }
            };
            // A text field shows its description as its placeholder; the others under the label.
            let description = field.description.clone().filter(|_| field.input.is_none());
            v_flex()
                .gap_1()
                .child(label)
                .when_some(description, |el, d| el.child(div().text_xs().text_color(muted).child(d)))
                .children(control)
        };
        let fields: Vec<_> = card.fields.iter().enumerate().map(|(fi, f)| field(fi, f, cx)).collect();

        let buttons = h_flex()
            .gap_2()
            .map(|el| match &card.url {
                Some(_) => el.child(
                    Button::new(id("open"))
                        .primary()
                        .small()
                        .label("Open link")
                        .on_click(cx.listener(move |this, _, _, cx| this.open_elicitation_link(ix, cx))),
                ),
                None => el.child(
                    Button::new(id("submit"))
                        .primary()
                        .small()
                        .label("Submit")
                        .on_click(cx.listener(move |this, _, _, cx| this.submit_elicitation(ix, cx))),
                ),
            })
            .child(Button::new(id("decline")).small().label("Decline").on_click(cx.listener(move |this, _, _, cx| {
                this.decide(ix, Decision::Elicit { action: ElicitAction::Decline, content: None }, cx)
            })))
            .child(Button::new(id("cancel")).ghost().small().label("Cancel").on_click(cx.listener(
                move |this, _, _, cx| {
                    this.decide(ix, Decision::Elicit { action: ElicitAction::Cancel, content: None }, cx)
                },
            )));

        v_flex()
            .gap_2()
            .p_3()
            .rounded_md()
            .border_1()
            .border_color(if outcome.is_none() { accent } else { border })
            .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).child(heading))
            .when(!card.message.is_empty(), |el| el.child(div().text_sm().child(card.message.clone())))
            .when_some(card.url.clone(), |el, url| {
                el.child(
                    v_flex()
                        .gap_0p5()
                        .px_2()
                        .py_1()
                        .rounded_sm()
                        .bg(secondary)
                        .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).child(host(&url).to_string()))
                        .child(div().text_xs().font_family(mono).text_color(muted).child(url)),
                )
            })
            .map(|el| match outcome {
                Some(outcome) => el.child(div().text_sm().text_color(muted).child(outcome.clone())),
                None => el
                    .children(fields)
                    .when_some(card.error.clone(), |el, error| {
                        el.child(div().text_sm().text_color(danger).child(error))
                    })
                    .child(buttons),
            })
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::{ElicitationCard, FieldKind};
    use gpui_kit::AppContext as _;
    use gpui_kit::component::input::InputState;
    use serde_json::{Value, json};

    #[test]
    fn a_form_is_built_from_the_servers_schema_in_its_order() {
        let request: Value = serde_json::from_str(
            r#"{
            "subtype": "elicitation",
            "mcp_server_name": "fieldhook",
            "display_name": "FieldHook",
            "message": "Confirm the timesheet",
            "mode": "form",
            "property_order": ["hours", "site", "approve", "crew"],
            "requested_schema": {
                "type": "object",
                "properties": {
                    "approve": { "type": "boolean", "title": "Approve", "default": true },
                    "crew": { "type": "array", "items": { "anyOf": [
                        { "const": "a", "title": "Alpha" }, { "const": "b", "title": "Bravo" } ] } },
                    "hours": { "type": "integer", "minimum": 0, "maximum": 24, "description": "Hours worked" },
                    "site": { "type": "string", "enum": ["n", "s"], "enumNames": ["North", "South"] },
                    "note": { "type": "string", "maxLength": 5 }
                },
                "required": ["hours", "site"]
            }
        }"#,
        )
        .unwrap();
        let card = ElicitationCard::parse(&request);
        assert_eq!(card.server, "FieldHook");
        assert_eq!(card.summary(), "FieldHook: Confirm the timesheet");
        let keys: Vec<&str> = card.fields.iter().map(|f| f.key.as_str()).collect();
        assert_eq!(keys, ["hours", "site", "approve", "crew", "note"], "listed order, then the rest");
        assert!(card.fields[0].required && !card.fields[2].required);
        assert!(matches!(
            card.fields[0].kind,
            FieldKind::Number { integer: true, min: Some(0.0), max: Some(24.0), .. }
        ));
        assert!(
            matches!(&card.fields[1].kind, FieldKind::Choice { options, picked: None } if options[1] == ("s".into(), "South".into()))
        );
        assert!(matches!(card.fields[2].kind, FieldKind::Bool(true)));
        assert!(matches!(&card.fields[3].kind, FieldKind::Multi { options, .. } if options[0].1 == "Alpha"));
        assert!(matches!(card.fields[4].kind, FieldKind::Text { max: Some(5), .. }));
    }

    #[test]
    fn a_link_names_its_site() {
        let card = ElicitationCard::parse(&json!({
            "mcp_server_name": "github",
            "mode": "url",
            "url": "https://github.com/login/oauth?x=1",
            "message": "Sign in"
        }));
        assert_eq!(card.url.as_deref(), Some("https://github.com/login/oauth?x=1"));
        assert!(card.fields.is_empty());
        assert_eq!(card.summary(), "github wants you to open github.com");
    }

    #[gpui_kit::test]
    fn the_forms_values_are_checked_and_sent_as_the_server_asked(cx: &mut gpui_kit::TestAppContext) {
        let mut card = ElicitationCard::parse(&json!({
            "mcp_server_name": "s",
            "message": "m",
            "requested_schema": { "properties": {
                "hours": { "type": "integer", "maximum": 24 },
                "site": { "type": "string", "enum": ["n", "s"] },
                "approve": { "type": "boolean" },
                "crew": { "type": "array", "items": { "enum": ["a", "b"] } }
            }, "required": ["hours", "site"] }
        }));
        let window = cx.add_empty_window();
        let input = window.update(|window, cx| cx.new(|cx| InputState::new(window, cx)));
        card.fields[0].input = Some(input.clone());
        let set = |text: &str, window: &mut gpui_kit::VisualTestContext| {
            let text = text.to_string();
            window.update(|window, cx| input.update(cx, |i, cx| i.set_value(text, window, cx)));
        };

        assert_eq!(window.update(|_, cx| card.content(cx)), Err("hours is needed.".into()));
        set("30", window);
        assert_eq!(window.update(|_, cx| card.content(cx)), Err("hours must be at most 24.".into()));
        set("8.5", window);
        assert_eq!(window.update(|_, cx| card.content(cx)), Err("hours must be a whole number.".into()));
        set("8", window);
        assert_eq!(window.update(|_, cx| card.content(cx)), Err("site is needed.".into()));
        if let FieldKind::Choice { picked, .. } = &mut card.fields[1].kind {
            *picked = Some(1);
        }
        if let FieldKind::Multi { picked, .. } = &mut card.fields[3].kind {
            picked.insert(0);
        }
        assert_eq!(
            window.update(|_, cx| card.content(cx)),
            Ok(json!({ "hours": 8, "site": "s", "approve": false, "crew": ["a"] }))
        );
    }
}
