//! TJ: a native GUI for coding agents.

mod agent;
mod changes;
mod composer;
mod demo;
mod handoff;
mod keymap;
mod omarchy;
mod overview;
mod panes;
mod sample;
mod settings;
mod store;
mod terminal;
mod thread;
mod workspace;

use std::path::PathBuf;

use gpui_kit::*;

use workspace::{Launch, Workspace};

/// Command-line options. The test hooks let a window be exercised without clicks.
#[derive(Default)]
struct Args {
    /// `stream` or `firehose`: play the canned sample reply.
    demo: Option<String>,
    /// Play back a recorded session file through the real parser.
    replay: Option<PathBuf>,
    /// Send this prompt to the agent at launch.
    prompt: Option<String>,
    /// Model alias or id for Claude, e.g. `haiku`.
    model: Option<String>,
    /// Text to type into the terminal at launch (`\r` for Enter).
    type_text: Option<String>,
    /// Open this project folder (added if new).
    project: Option<PathBuf>,
    /// Open the review of the project's changes.
    review: bool,
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
            project: value("--project").map(PathBuf::from),
            review: args.iter().any(|a| a == "--review"),
        }
    }

    fn launch(self) -> Launch {
        let scripted = if let Some(path) = &self.replay {
            Some(demo::replay(path).unwrap_or_default())
        } else {
            self.demo.as_ref().map(|kind| demo::sample_stream(kind != "stream"))
        };
        // Demos and replays run in a throwaway folder so they never touch saved projects.
        let paths = scripted
            .as_ref()
            .map(|_| store::Paths::in_dir(&std::env::temp_dir().join(format!("tj-demo-{}", std::process::id()))));
        let prompt = self.prompt.or_else(|| scripted.as_ref().map(|_| sample::USER_PROMPT.to_string()));
        Launch {
            project: self.project,
            prompt,
            model: self.model,
            scripted,
            type_text: self.type_text,
            paths,
            shell: None,
            no_summaries: false,
            review: self.review,
        }
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
    // The icon SVGs (the full Lucide set) must be registered, or icons render blank.
    gpui_kit::application().with_assets(gpui_kit::assets::AllAssets).run(move |cx| {
        gpui_kit::init(cx);
        keymap::init(cx);
        // gpui keeps running after its last window closes; TJ has only one, so quit with it.
        // Dropping the workspace also stops its claude processes.
        cx.on_window_closed(|cx, _| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        let launch = args.launch();
        let paths = launch.paths.clone().unwrap_or_else(store::Paths::from_env);
        let settings = settings::Settings::load(&paths);
        let font = desktop_ui_font();
        omarchy::apply(cx, settings.follow_omarchy_theme, font.as_deref());
        omarchy::watch(cx, font);
        cx.set_global(settings);

        // GNOME-style: no title bar; the sidebar and content headers move the window.
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::centered(None, size(px(1500.), px(940.)), cx))),
            app_id: Some("tj".into()),
            titlebar: None,
            window_decorations: Some(WindowDecorations::Client),
            // Transparent so the sidebar can be see-through; everything else paints opaque.
            window_background: WindowBackgroundAppearance::Transparent,
            ..Default::default()
        };
        let (window, _) =
            gpui_kit::open_window(options, cx, |window, cx| cx.new(|cx| Workspace::new(launch, window, cx)))
                .expect("failed to open window");
        // gpui-kit's root paints the theme background over the whole window. The root's own
        // style is applied after that, so clear it there: TJ's content area paints its own
        // background, and the sidebar can then be see-through.
        if let Some(root) = window.downcast::<gpui_kit::base::Root>() {
            let _ = root.update(cx, |root, _, _| {
                root.style().background = Some(gpui_kit::transparent_black().into());
            });
        }
        cx.activate(true);
    });
}
