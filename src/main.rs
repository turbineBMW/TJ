//! TJ: a native GUI for coding agents.

mod agent;
mod agent_icon;
mod agent_slot;
mod attention;
mod box_drawing;
mod caret;
mod changes;
mod composer;
mod demo;
mod fonts;
mod github;
mod github_view;
mod grow;
mod handoff;
mod highlight;
mod history;
mod images;
mod keybindings;
mod keymap;
mod line_comment;
mod md_link;
mod notes;
mod notes_mcp;
mod notes_view;
mod omarchy;
mod overview;
mod panes;
mod project_image;
mod sample;
mod settings;
mod settings_view;
mod store;
mod summary;
mod switcher;
mod terminal;
mod thread;
mod tool_card;
mod usage;
mod working;
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
    /// Open the project's GitHub tab.
    github: bool,
    /// Open the project's notes.
    notes: bool,
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
            github: args.iter().any(|a| a == "--github"),
            notes: args.iter().any(|a| a == "--notes"),
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
            github: self.github,
            notes: self.notes,
        }
    }
}

fn main() {
    // `tj notes-mcp …`: the notes server TJ gives each Claude session, not the app.
    let raw: Vec<String> = std::env::args().skip(1).collect();
    if raw.first().map(String::as_str) == Some("notes-mcp") {
        let result = notes_mcp::Context::from_args(&raw).and_then(|context| notes_mcp::run(&context));
        if let Err(err) = result {
            eprintln!("tj notes-mcp: {err:#}");
            std::process::exit(1);
        }
        return;
    }
    let args = Args::parse();
    // The icon SVGs (the full Lucide set) must be registered, or icons render blank.
    gpui_kit::application().with_assets(gpui_kit::assets::AllAssets).run(move |cx| {
        gpui_kit::init(cx);
        // Notifications go out as TJ, which also finds TJ's launcher entry and its icon.
        cx.set_app_identity("dev.turbinebmw.TJ", "TJ");
        caret::init(cx);
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
        let follow = settings.follow_omarchy_theme;
        fonts::init(cx);
        cx.set_global(settings);
        omarchy::apply(cx, follow);
        omarchy::watch(cx);
        // After the settings: the app shortcuts take their keys from them.
        keymap::init(cx);

        // GNOME-style: no title bar; the sidebar and content headers move the window.
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::centered(None, size(px(1500.), px(940.)), cx))),
            // Matches the installed .desktop file, so the launcher and the window list show
            // TJ's icon for the running window.
            app_id: Some("dev.turbinebmw.TJ".into()),
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
