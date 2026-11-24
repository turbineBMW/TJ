//! TJ: a native GUI for coding agents.

mod agent;
mod demo;
mod keymap;
mod sample;
mod terminal;
mod thread;

use std::path::PathBuf;

use gpui_kit::base::{h_flex, v_flex};
use gpui_kit::component::{
    ActiveTheme as _, Selectable as _, Sizable as _, Theme, ThemeMode,
    button::{Button, ButtonVariants as _},
};
use gpui_kit::*;

use agent::{PermissionMode, claude::SessionOptions};
use terminal::TerminalView;
use thread::{Backend, Status, ThreadView};

/// Command-line options. The test hooks let a window be exercised without clicks.
#[derive(Default)]
struct Args {
    /// `stream`, `firehose` or `history`: play the canned sample reply.
    demo: Option<String>,
    /// Play back a recorded session file through the real parser.
    replay: Option<PathBuf>,
    /// Send this prompt to the agent at launch.
    prompt: Option<String>,
    /// Model alias or id for Claude, e.g. `haiku`.
    model: Option<String>,
    /// Text to type into the terminal at launch (`\r` for Enter).
    type_text: Option<String>,
}

impl Args {
    fn parse() -> Self {
        let args: Vec<String> = std::env::args().collect();
        let value = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
        Self {
            demo: value("--demo"),
            replay: value("--replay").map(PathBuf::from),
            prompt: value("--prompt"),
            model: value("--model"),
            type_text: value("--type"),
        }
    }
}

struct Workspace {
    thread: Entity<ThreadView>,
    terminal: Option<Entity<TerminalView>>,
    terminal_error: Option<String>,
    cwd: PathBuf,
}

impl Workspace {
    fn new(args: Args, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let cwd = thread::default_cwd();
        let backend = if let Some(path) = &args.replay {
            Backend::Scripted(demo::replay(path).unwrap_or_default())
        } else if let Some(kind) = &args.demo {
            Backend::Scripted(demo::sample_stream(kind != "stream"))
        } else {
            Backend::Claude(SessionOptions { cwd: cwd.clone(), model: args.model.clone(), ..Default::default() })
        };
        let thread = cx.new(|cx| ThreadView::new(backend, window, cx));
        cx.observe(&thread, |_, _, cx| cx.notify()).detach();

        if args.demo.as_deref() == Some("history") {
            thread.update(cx, |t, cx| t.add_history(50, sample::USER_PROMPT, sample::REPLY, cx));
        }
        let first_prompt = args
            .prompt
            .clone()
            .or_else(|| (args.demo.is_some() || args.replay.is_some()).then(|| sample::USER_PROMPT.to_string()));
        if let Some(prompt) = first_prompt {
            thread.update(cx, |t, cx| t.send(&prompt, cx));
        }

        let (terminal, terminal_error) = match TerminalView::spawn(Some(cwd.clone()), None, window, cx) {
            Ok(term) => {
                cx.observe(&term, |_, _, cx| cx.notify()).detach();
                (Some(term), None)
            }
            Err(err) => (None, Some(format!("{err:#}"))),
        };
        if let (Some(text), Some(term)) = (&args.type_text, &terminal) {
            let text = text.replace("\\r", "\r");
            term.update(cx, |t, _| t.type_text(&text));
        }

        let focus = thread.read(cx).prompt_focus(cx);
        window.focus(&focus, cx);

        Self { thread, terminal, terminal_error, cwd }
    }

    fn header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let thread = self.thread.read(cx);
        let mode = thread.mode;
        let mut stats = Vec::new();
        if let Some(model) = &thread.model {
            stats.push(thread::short_model(model));
        }
        for window in &thread.usage {
            let label = match window.id.as_str() {
                "five_hour" => "5h",
                "seven_day" => "7d",
                other => other,
            };
            stats.push(format!("{label} {:.0}%", window.utilization * 100.0));
        }
        if thread.cost_usd > 0.0 {
            stats.push(if thread.cost_usd < 1.0 {
                format!("${:.3}", thread.cost_usd)
            } else {
                format!("${:.2}", thread.cost_usd)
            });
        }
        let status = match thread.status {
            Status::Idle => "",
            Status::Running => "Working…",
            Status::AwaitingApproval => "Waiting for you",
        };
        let project = self.cwd.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        let theme = cx.theme();

        h_flex()
            .gap_2()
            .px_3()
            .py_2()
            .border_b_1()
            .border_color(theme.border)
            .child(div().font_weight(FontWeight::BOLD).child("TJ"))
            .child(div().text_sm().text_color(theme.muted_foreground).mr_2().child(project))
            .children(PermissionMode::ALL.map(|m| {
                Button::new(SharedString::from(format!("mode-{}", m.claude_name())))
                    .small()
                    .ghost()
                    .selected(m == mode)
                    .label(m.label())
                    .on_click(cx.listener(move |this, _, _, cx| this.thread.update(cx, |t, cx| t.set_mode(m, cx))))
            }))
            .child(div().flex_1())
            .child(div().text_sm().text_color(theme.warning).child(status))
            .child(div().text_sm().text_color(theme.muted_foreground).child(stats.join(" · ")))
    }
}

impl Render for Workspace {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (border, muted) = (theme.border, theme.muted_foreground);
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

        v_flex().size_full().bg(theme.background).text_color(theme.foreground).child(self.header(cx)).child(
            h_flex().flex_1().min_h_0().child(div().flex_1().min_w_0().h_full().child(self.thread.clone())).child(
                v_flex()
                    .w(relative(0.4))
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
    let args = Args::parse();
    gpui_kit::application().run(move |cx| {
        gpui_kit::init(cx);
        keymap::init(cx);
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
        gpui_kit::open_window(options, cx, |window, cx| cx.new(|cx| Workspace::new(args, window, cx)))
            .expect("failed to open window");
        cx.activate(true);
    });
}
