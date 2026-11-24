//! TJ spike: streaming markdown transcript next to an embedded terminal.

mod chat;
mod sample;
mod terminal;

use gpui_kit::base::{h_flex, v_flex};
use gpui_kit::component::{
    ActiveTheme as _, Theme, ThemeMode,
    button::{Button, ButtonVariants as _},
};
use gpui_kit::*;

use chat::{ChatView, Speed};
use terminal::TerminalView;

struct Workspace {
    chat: Entity<ChatView>,
    terminal: Option<Entity<TerminalView>>,
    terminal_error: Option<String>,
}

impl Workspace {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let chat = cx.new(ChatView::new);
        cx.observe(&chat, |_, _, cx| cx.notify()).detach();

        let cwd = std::env::current_dir().ok();
        let (terminal, terminal_error) = match TerminalView::spawn(cwd, None, window, cx) {
            Ok(term) => {
                cx.observe(&term, |_, _, cx| cx.notify()).detach();
                (Some(term), None)
            }
            Err(err) => (None, Some(format!("{err:#}"))),
        };

        // Spike-only test hooks, so a hidden window can be exercised without clicks.
        let args: Vec<String> = std::env::args().collect();
        let arg = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
        match arg("--demo").as_deref() {
            Some("stream") => chat.update(cx, |c, cx| c.stream_reply(Speed::Realistic, cx)),
            Some("firehose") => chat.update(cx, |c, cx| c.stream_reply(Speed::Firehose, cx)),
            Some("history") => chat.update(cx, |c, cx| {
                c.add_history(50, cx);
                c.stream_reply(Speed::Firehose, cx);
            }),
            _ => {}
        }
        if let (Some(text), Some(term)) = (arg("--type"), &terminal) {
            let text = text.replace("\\r", "\r");
            term.update(cx, |t, _| t.type_text(&text));
        }

        Self { chat, terminal, terminal_error }
    }

    fn toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let chat = self.chat.read(cx);
        let stats = chat.stats.clone();
        let count = chat.message_count();
        let rate = if stats.elapsed.as_secs_f32() > 0.0 {
            stats.chars as f32 / stats.elapsed.as_secs_f32()
        } else {
            0.0
        };
        let status = if stats.chunks == 0 {
            format!("{count} messages")
        } else {
            format!(
                "{count} messages · {} {} chars in {:.1}s ({:.0}/s, {} chunks)",
                if stats.streaming { "streaming" } else { "streamed" },
                stats.chars,
                stats.elapsed.as_secs_f32(),
                rate,
                stats.chunks,
            )
        };
        let theme = cx.theme();

        h_flex()
            .gap_2()
            .px_3()
            .py_2()
            .border_b_1()
            .border_color(theme.border)
            .child(div().font_weight(FontWeight::BOLD).mr_2().child("TJ"))
            .child(Button::new("stream").primary().label("Stream reply").on_click(cx.listener(
                |this, _, _, cx| this.chat.update(cx, |c, cx| c.stream_reply(Speed::Realistic, cx)),
            )))
            .child(Button::new("firehose").label("Firehose").on_click(cx.listener(|this, _, _, cx| {
                this.chat.update(cx, |c, cx| c.stream_reply(Speed::Firehose, cx))
            })))
            .child(Button::new("history").label("+50 replies").on_click(cx.listener(|this, _, _, cx| {
                this.chat.update(cx, |c, cx| c.add_history(50, cx))
            })))
            .child(Button::new("clear").ghost().label("Clear").on_click(cx.listener(|this, _, _, cx| {
                this.chat.update(cx, |c, cx| c.clear(cx))
            })))
            .child(div().flex_1())
            .child(div().text_sm().text_color(theme.muted_foreground).child(status))
    }
}

impl Render for Workspace {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let border = theme.border;
        let muted = theme.muted_foreground;
        let title = self
            .terminal
            .as_ref()
            .and_then(|t| t.read(cx).title().map(str::to_owned))
            .unwrap_or_else(|| "Terminal".into());

        let terminal_pane = match (&self.terminal, &self.terminal_error) {
            (Some(term), _) => div().size_full().child(term.clone()).into_any_element(),
            (None, Some(err)) => div().p_4().child(format!("Terminal failed: {err}")).into_any_element(),
            (None, None) => div().into_any_element(),
        };

        v_flex()
            .size_full()
            .bg(theme.background)
            .text_color(theme.foreground)
            .child(self.toolbar(cx))
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .child(div().flex_1().min_w_0().h_full().child(self.chat.clone()))
                    .child(
                        v_flex()
                            .w(relative(0.42))
                            .h_full()
                            .border_l_1()
                            .border_color(border)
                            .child(div().px_3().py_1().text_xs().text_color(muted).child(title))
                            .child(div().flex_1().min_h_0().child(terminal_pane)),
                    ),
            )
    }
}

/// The UI font: `TJ_UI_FONT`, else the desktop font from GNOME settings if it
/// has a real bold face, else Noto Sans.
///
/// gpui on Linux ignores the weight axis of variable fonts, so a variable-only
/// family such as Adwaita Sans renders **bold** as regular. gpui-kit's default
/// `.SystemUIFont` has the same problem here.
fn desktop_ui_font() -> Option<String> {
    if let Ok(family) = std::env::var("TJ_UI_FONT") {
        return Some(family);
    }
    let run = |cmd: &str, args: &[&str]| {
        let out = std::process::Command::new(cmd).args(args).output().ok()?;
        String::from_utf8(out.stdout).ok()
    };
    let desktop = run("gsettings", &["get", "org.gnome.desktop.interface", "font-name"]).and_then(|value| {
        let value = value.trim().trim_matches('\'').to_string();
        // "Adwaita Sans 11" -> "Adwaita Sans"
        let family = match value.rsplit_once(' ') {
            Some((family, size)) if size.parse::<f32>().is_ok() => family.to_string(),
            _ => value,
        };
        (!family.is_empty()).then_some(family)
    });
    // A static bold face lives in its own file; a variable font's "Bold" instance shares the
    // regular file. fontconfig reports both as style Bold, so compare files instead.
    let has_static_bold = |family: &str| {
        let matched = |pattern: String| run("fc-match", &["-f", "%{family}|%{file}", &pattern]);
        match (matched(family.to_string()), matched(format!("{family}:bold"))) {
            (Some(regular), Some(bold)) => bold.starts_with(family) && regular != bold,
            _ => false,
        }
    };
    desktop
        .filter(|family| has_static_bold(family))
        .or_else(|| has_static_bold("Noto Sans").then(|| "Noto Sans".to_string()))
}

fn main() {
    gpui_kit::application().run(|cx| {
        gpui_kit::init(cx);
        terminal::init(cx);
        Theme::change(ThemeMode::Dark, None, cx);
        if let Some(family) = desktop_ui_font() {
            Theme::global_mut(cx).font_family = family.into();
        }

        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::centered(None, size(px(1400.), px(900.)), cx))),
            app_id: Some("tj".into()),
            titlebar: Some(TitlebarOptions { title: Some("TJ".into()), ..Default::default() }),
            ..Default::default()
        };
        gpui_kit::open_window(options, cx, |window, cx| cx.new(|cx| Workspace::new(window, cx)))
            .expect("failed to open window");
        cx.activate(true);
    });
}
