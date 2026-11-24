//! The window: a full-height sidebar of projects and threads, the active thread, and the
//! project's terminal. Laid out like a GNOME app: no title bar; the headers move the window.

use std::{collections::HashMap, path::PathBuf, time::Duration};

use gpui_kit::base::{h_flex, v_flex};
use gpui_kit::component::{
    ActiveTheme as _, Selectable as _, Sizable as _,
    button::{Button, ButtonVariants as _},
    menu::{DropdownMenu as _, PopupMenuItem},
};
use gpui_kit::{TestSupportExt as _, prelude::FluentBuilder as _, *};

use crate::agent::{AgentEvent, PermissionMode, claude::SessionOptions};
use crate::store::{self, Accounts, Paths, State};
use crate::terminal::TerminalView;
use crate::thread::{self, Backend, Status, ThreadChanged, ThreadSetup, ThreadView};

const SIDEBAR_WIDTH: f32 = 264.0;
const HEADER_HEIGHT: f32 = 46.0;

/// How the workspace starts.
#[derive(Default)]
pub struct Launch {
    /// Open (adding if needed) this project folder.
    pub project: Option<PathBuf>,
    /// Send this prompt in a new thread.
    pub prompt: Option<String>,
    pub model: Option<String>,
    /// Canned events for the first thread opened (`--demo`, `--replay`); nothing is saved.
    pub scripted: Option<Vec<(Duration, AgentEvent)>>,
    /// Text to type into the first terminal.
    pub type_text: Option<String>,
    pub paths: Option<Paths>,
    /// Start with the terminal hidden.
    pub hide_terminal: bool,
}

pub struct Workspace {
    paths: Paths,
    home: PathBuf,
    accounts: Accounts,
    state: State,
    threads: HashMap<String, Entity<ThreadView>>,
    terminals: HashMap<String, Entity<TerminalView>>,
    active: Option<String>,
    show_terminal: bool,
    model: Option<String>,
    scripted: Option<Vec<(Duration, AgentEvent)>>,
    type_text: Option<String>,
    /// Shown above the thread when something about the setup is wrong.
    problem: Option<SharedString>,
    _subscriptions: Vec<Subscription>,
}

impl Workspace {
    pub fn new(launch: Launch, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let paths = launch.paths.unwrap_or_else(Paths::from_env);
        let home = store::home_dir();
        let mut problem = None;
        let accounts = Accounts::load_or_create(&paths, &home).unwrap_or_else(|err| {
            problem = Some(format!("Accounts file problem, using detected accounts instead: {err:#}").into());
            Accounts::detect(&home)
        });
        let state = State::load(&paths).unwrap_or_else(|err| {
            problem = Some(format!("Could not read saved projects: {err:#}").into());
            State::default()
        });

        let mut this = Self {
            paths,
            home,
            accounts,
            state,
            threads: HashMap::new(),
            terminals: HashMap::new(),
            active: None,
            show_terminal: !launch.hide_terminal,
            model: launch.model,
            scripted: launch.scripted,
            type_text: launch.type_text,
            problem,
            _subscriptions: Vec::new(),
        };

        // Open the requested folder, or the current one when there are no projects yet.
        let folder = launch.project.or_else(|| this.state.projects.is_empty().then(thread::default_cwd));
        let opened_project = folder.map(|folder| this.state.add_project(&folder).id.clone());

        // A prompt or a canned session gets a fresh thread; otherwise reopen where the user was.
        let wants_new = this.scripted.is_some() || launch.prompt.is_some();
        let remembered = this.state.selected_thread.clone().filter(|id| this.state.thread(id).is_some());
        let project = opened_project
            .or_else(|| remembered.as_ref().and_then(|id| this.state.thread(id)).map(|(p, _)| p.id.clone()))
            .or_else(|| this.state.projects.first().map(|p| p.id.clone()));
        let thread_id = match project {
            Some(project) if wants_new => this.state.new_thread(&project),
            Some(project) => remembered
                .filter(|id| this.state.thread(id).is_some_and(|(p, _)| p.id == project))
                .or_else(|| this.state.project(&project).and_then(|p| p.threads.first().map(|t| t.id.clone())))
                .or_else(|| this.state.new_thread(&project)),
            None => None,
        };
        if let Some(id) = thread_id {
            this.open_thread(&id, window, cx);
            if let (Some(prompt), Some(thread)) = (launch.prompt, this.active_thread()) {
                thread.update(cx, |t, cx| t.send(&prompt, cx));
            }
        }
        this.save();
        this
    }

    fn save(&mut self) {
        if let Err(err) = self.state.save(&self.paths) {
            self.problem = Some(format!("Could not save projects: {err:#}").into());
        }
    }

    fn active_thread(&self) -> Option<Entity<ThreadView>> {
        self.active.as_ref().and_then(|id| self.threads.get(id)).cloned()
    }

    fn active_project(&self) -> Option<&store::Project> {
        self.active.as_ref().and_then(|id| self.state.thread(id)).map(|(project, _)| project)
    }

    // ---------- threads ----------

    fn open_thread(&mut self, thread_id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some((project, meta)) = self.state.thread(thread_id) else { return };
        if !self.threads.contains_key(thread_id) {
            let account = self.accounts.resolve(meta.account.as_deref(), project.account.as_deref()).clone();
            // A Claude session lives in its account's config directory; resume it only there.
            let session_id = meta.session_id.clone().filter(|_| meta.session_account.as_deref() == Some(&account.id));
            let mode = meta.mode.or(project.mode).unwrap_or_default();
            let setup = ThreadSetup {
                title: meta.title.clone(),
                session_id,
                account_id: account.id.clone(),
                account_name: account.name.clone(),
                transcript: self.scripted.is_none().then(|| self.paths.transcript(&meta.id)),
            };
            let backend = match self.scripted.take() {
                Some(events) => Backend::Scripted(events),
                None => Backend::Claude(SessionOptions {
                    cwd: project.path.clone(),
                    config_dir: account.config_path(&self.home),
                    model: self.model.clone(),
                    permission_mode: mode,
                    ..Default::default()
                }),
            };
            let thread = cx.new(|cx| ThreadView::open(setup, backend, window, cx));
            let id = thread_id.to_string();
            self._subscriptions.push(cx.subscribe(&thread, move |this, _, _: &ThreadChanged, cx| {
                this.sync_thread(&id, cx);
                cx.notify();
            }));
            self.threads.insert(thread_id.to_string(), thread);
        }
        self.active = Some(thread_id.to_string());
        self.state.selected_thread = self.active.clone();
        if let Some(thread) = self.active_thread() {
            let focus = thread.read(cx).prompt_focus(cx);
            window.focus(&focus, cx);
        }
        self.save();
        cx.notify();
    }

    /// Copy what the thread knows (title, session, account, mode) into the saved state.
    fn sync_thread(&mut self, thread_id: &str, cx: &App) {
        let Some(thread) = self.threads.get(thread_id) else { return };
        let Some((project, _)) = self.state.thread(thread_id) else { return };
        let project_account = self.accounts.resolve(None, project.account.as_deref()).id.clone();
        let project_mode = project.mode.unwrap_or_default();
        let t = thread.read(cx);
        let (title, session_id, account_id, mode) =
            (t.title.clone(), t.session_id.clone(), t.account_id.clone(), t.mode);
        let Some(meta) = self.state.thread_mut(thread_id) else { return };
        let changed = meta.title != title || meta.session_id != session_id;
        meta.title = title;
        meta.session_account = session_id.as_ref().map(|_| account_id.clone());
        meta.session_id = session_id;
        meta.account = (account_id != project_account).then_some(account_id);
        meta.mode = (mode != project_mode).then_some(mode);
        if changed {
            meta.updated_at = store::now();
        }
        self.save();
    }

    fn new_thread(&mut self, project_id: &str, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(id) = self.state.new_thread(project_id) {
            self.open_thread(&id, window, cx);
        }
    }

    fn add_project_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Add project".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = picked.await else { return };
            let Some(path) = paths.into_iter().next() else { return };
            let _ = this.update_in(cx, |this: &mut Workspace, window, cx| {
                let project = this.state.add_project(&path).id.clone();
                this.new_thread(&project, window, cx);
            });
        })
        .detach();
    }

    // ---------- accounts and modes ----------

    fn switch_account(&mut self, account_id: &str, cx: &mut Context<Self>) {
        let (Some(thread), Some(account)) = (self.active_thread(), self.accounts.get(account_id).cloned()) else {
            return;
        };
        let config_dir = account.config_path(&self.home);
        thread.update(cx, |t, cx| t.set_account(&account.id, &account.name, config_dir, cx));
    }

    fn make_project_default_account(&mut self, cx: &mut Context<Self>) {
        let Some(thread) = self.active_thread() else { return };
        let account = thread.read(cx).account_id.clone();
        let Some(project_id) = self.active_project().map(|p| p.id.clone()) else { return };
        if let Some(project) = self.state.project_mut(&project_id) {
            project.account = Some(account);
        }
        if let Some(id) = self.active.clone() {
            self.sync_thread(&id, cx);
        }
        cx.notify();
    }

    fn make_project_default_mode(&mut self, cx: &mut Context<Self>) {
        let Some(thread) = self.active_thread() else { return };
        let mode = thread.read(cx).mode;
        let Some(project_id) = self.active_project().map(|p| p.id.clone()) else { return };
        if let Some(project) = self.state.project_mut(&project_id) {
            project.mode = Some(mode);
        }
        if let Some(id) = self.active.clone() {
            self.sync_thread(&id, cx);
        }
        cx.notify();
    }

    /// Open the project's terminal and start `claude auth login` for the thread's account.
    fn log_in(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(thread) = self.active_thread() else { return };
        let account_id = thread.read(cx).account_id.clone();
        let Some(account) = self.accounts.get(&account_id).cloned() else { return };
        let command = match account.config_path(&self.home) {
            Some(dir) => format!("CLAUDE_CONFIG_DIR={} claude auth login\r", shell_quote(&dir.display().to_string())),
            None => "claude auth login\r".to_string(),
        };
        self.show_terminal = true;
        if let Some(term) = self.project_terminal(window, cx) {
            term.update(cx, |t, _| t.type_text(&command));
            let focus = term.read(cx).focus_handle(cx);
            window.focus(&focus, cx);
        }
        cx.notify();
    }

    fn project_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Option<Entity<TerminalView>> {
        let project = self.active_project()?.clone();
        if let Some(term) = self.terminals.get(&project.id) {
            return Some(term.clone());
        }
        match TerminalView::spawn(Some(project.path.clone()), None, window, cx) {
            Ok(term) => {
                self._subscriptions.push(cx.observe(&term, |_, _, cx| cx.notify()));
                if let Some(text) = self.type_text.take() {
                    term.update(cx, |t, _| t.type_text(&text.replace("\\r", "\r")));
                }
                self.terminals.insert(project.id.clone(), term.clone());
                Some(term)
            }
            Err(err) => {
                self.problem = Some(format!("Terminal failed: {err:#}").into());
                None
            }
        }
    }

    // ---------- rendering ----------

    fn sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (muted, accent_bg, fg) =
            (theme.sidebar_foreground.opacity(0.6), theme.sidebar_accent, theme.sidebar_foreground);
        let (running, waiting) = (theme.warning, theme.primary);
        let active = self.active.clone();

        let projects = self.state.projects.iter().map(|project| {
            let project_id = project.id.clone();
            let threads = project.threads.iter().map(|meta| {
                let id = meta.id.clone();
                let status = self.threads.get(&meta.id).map(|t| t.read(cx).status).unwrap_or(Status::Idle);
                let dot = match status {
                    Status::Idle => None,
                    Status::Running => Some(running),
                    Status::AwaitingApproval => Some(waiting),
                };
                let selected = active.as_deref() == Some(meta.id.as_str());
                h_flex()
                    .id(SharedString::from(format!("thread-{}", meta.id)))
                    .test_support()
                    .gap_2()
                    .mx_2()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .cursor_pointer()
                    .when(selected, |el| el.bg(accent_bg))
                    .hover(|el| el.bg(accent_bg.opacity(0.6)))
                    .on_click(cx.listener(move |this, _, window, cx| this.open_thread(&id, window, cx)))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .text_sm()
                            .text_color(fg)
                            .child(meta.title.clone().unwrap_or_else(|| "New thread".into())),
                    )
                    .when_some(dot, |el, color| el.child(div().size_2().rounded_full().bg(color)))
            });
            v_flex()
                .pb_2()
                .child(
                    h_flex()
                        .px_4()
                        .pt_2()
                        .pb_1()
                        .gap_1()
                        .child(
                            div()
                                .flex_1()
                                .text_xs()
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(muted)
                                .child(project.name.to_uppercase()),
                        )
                        .child(
                            Button::new(SharedString::from(format!("new-thread-{}", project.id)))
                                .ghost()
                                .xsmall()
                                .label("+")
                                .tooltip("New thread")
                                .on_click(
                                    cx.listener(move |this, _, window, cx| this.new_thread(&project_id, window, cx)),
                                ),
                        ),
                )
                .children(threads)
        });

        v_flex()
            .w(px(SIDEBAR_WIDTH))
            .h_full()
            .flex_none()
            .bg(theme.sidebar)
            .border_r_1()
            .border_color(theme.sidebar_border)
            .child(
                h_flex()
                    .h(px(HEADER_HEIGHT))
                    .flex_none()
                    .px_4()
                    .gap_2()
                    .child(div().font_weight(FontWeight::BOLD).text_color(fg).child("TJ"))
                    .child(drag_area())
                    .child(
                        Button::new("add-project")
                            .ghost()
                            .small()
                            .label("+")
                            .tooltip("Add project")
                            .on_click(cx.listener(|this, _, window, cx| this.add_project_dialog(window, cx))),
                    ),
            )
            .child(div().id("projects").flex_1().min_h_0().overflow_y_scroll().children(projects))
    }

    fn content_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (muted, border, warning) = (theme.muted_foreground, theme.border, theme.warning);
        let Some(thread) = self.active_thread() else {
            return h_flex().h(px(HEADER_HEIGHT)).flex_none().border_b_1().border_color(border).child(drag_area());
        };
        let t = thread.read(cx);
        let (mode, account_id, account_name) = (t.mode, t.account_id.clone(), t.account_name.clone());
        let title = t.title.clone().unwrap_or_else(|| "New thread".into());
        let mut stats = Vec::new();
        if let Some(model) = &t.model {
            stats.push(thread::short_model(model));
        }
        for window in &t.usage {
            let label = match window.id.as_str() {
                "five_hour" => "5h",
                "seven_day" => "7d",
                other => other,
            };
            stats.push(format!("{label} {:.0}%", window.utilization * 100.0));
        }
        if t.cost_usd > 0.0 {
            stats.push(if t.cost_usd < 1.0 { format!("${:.3}", t.cost_usd) } else { format!("${:.2}", t.cost_usd) });
        }
        let status = match t.status {
            Status::Idle => "",
            Status::Running => "Working…",
            Status::AwaitingApproval => "Waiting for you",
        };
        let project = self.active_project();
        let project_name = project.map(|p| p.name.clone()).unwrap_or_default();
        let project_account = self.accounts.resolve(None, project.and_then(|p| p.account.as_deref())).id.clone();
        let project_mode = project.and_then(|p| p.mode).unwrap_or_default();
        let accounts = self.accounts.accounts.clone();
        let this = cx.entity();

        let account_menu = Button::new("account").small().outline().label(format!("{account_name} ▾")).dropdown_menu(
            move |menu, _window, _cx| {
                let mut menu = menu;
                for account in &accounts {
                    let (this, id) = (this.clone(), account.id.clone());
                    menu = menu.item(
                        PopupMenuItem::new(account.name.clone())
                            .checked(account.id == account_id)
                            .on_click(move |_, _, cx| this.update(cx, |w, cx| w.switch_account(&id, cx))),
                    );
                }
                let (this2, this3) = (this.clone(), this.clone());
                menu.separator()
                    .item(
                        PopupMenuItem::new(format!("Use {account_name} for new threads in {project_name}"))
                            .checked(account_id == project_account)
                            .on_click(move |_, _, cx| this2.update(cx, |w, cx| w.make_project_default_account(cx))),
                    )
                    .item(
                        PopupMenuItem::new(format!("Log in to {account_name}…"))
                            .on_click(move |_, window, cx| this3.update(cx, |w, cx| w.log_in(window, cx))),
                    )
            },
        );

        h_flex()
            .h(px(HEADER_HEIGHT))
            .flex_none()
            .gap_2()
            .px_3()
            .border_b_1()
            .border_color(border)
            .child(
                div()
                    .max_w(px(320.))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(title),
            )
            .child(account_menu)
            .child(h_flex().gap_0p5().children(PermissionMode::ALL.map(|m| {
                Button::new(SharedString::from(format!("mode-{}", m.claude_name())))
                    .xsmall()
                    .ghost()
                    .selected(m == mode)
                    .label(m.label())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(thread) = this.active_thread() {
                            thread.update(cx, |t, cx| t.set_mode(m, cx));
                        }
                        if let Some(id) = this.active.clone() {
                            this.sync_thread(&id, cx);
                        }
                    }))
            })))
            .when(mode != project_mode, |el| {
                el.child(
                    Button::new("mode-default")
                        .xsmall()
                        .ghost()
                        .label("Make default")
                        .tooltip("Use this mode for new threads in this project")
                        .on_click(cx.listener(|this, _, _, cx| this.make_project_default_mode(cx))),
                )
            })
            .child(drag_area())
            .child(div().text_sm().text_color(warning).child(status))
            .child(div().text_sm().text_color(muted).child(stats.join(" · ")))
            .child(
                Button::new("toggle-terminal")
                    .xsmall()
                    .ghost()
                    .selected(self.show_terminal)
                    .label("Terminal")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.show_terminal = !this.show_terminal;
                        cx.notify();
                    })),
            )
            .child(
                Button::new("close-window")
                    .xsmall()
                    .ghost()
                    .label("✕")
                    .tooltip("Close")
                    .on_click(|_, window, _| window.remove_window()),
            )
    }
}

/// An empty stretch of header that moves the window when dragged and maximizes it on double-click.
fn drag_area() -> impl IntoElement {
    div().id("drag").flex_1().h_full().on_mouse_down(MouseButton::Left, |event, window, _| {
        if event.click_count == 2 {
            window.zoom_window();
        } else {
            window.start_window_move();
        }
    })
}

fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let terminal = if self.show_terminal && self.active_project().is_some() {
            self.project_terminal(window, cx)
        } else {
            None
        };
        let theme = cx.theme();
        let (bg, fg, border, muted, danger) =
            (theme.background, theme.foreground, theme.border, theme.muted_foreground, theme.danger);
        let terminal_title =
            terminal.as_ref().and_then(|t| t.read(cx).title().map(str::to_owned)).unwrap_or_else(|| "Terminal".into());

        h_flex().size_full().bg(bg).text_color(fg).child(self.sidebar(cx)).child(
            v_flex()
                .flex_1()
                .min_w_0()
                .h_full()
                .child(self.content_header(cx))
                .when_some(self.problem.clone(), |el, problem| {
                    el.child(div().px_4().py_2().text_sm().text_color(danger).child(problem))
                })
                .child(
                    h_flex()
                        .flex_1()
                        .min_h_0()
                        .child(div().flex_1().min_w_0().h_full().children(self.active_thread()))
                        .when_some(terminal, |el, term| {
                            el.child(
                                v_flex()
                                    .w(relative(0.42))
                                    .h_full()
                                    .border_l_1()
                                    .border_color(border)
                                    .child(div().px_3().py_1().text_xs().text_color(muted).child(terminal_title))
                                    .child(div().flex_1().min_h_0().child(term)),
                            )
                        }),
                ),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{Launch, Workspace};
    use crate::store::{Paths, State, new_id};
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{
        AppContext as _, Entity, SharedString, TestAppContext, VisualTestContext, WindowBounds, WindowOptions, bounds,
        point, px, size,
    };

    fn open(cx: &mut TestAppContext) -> (gpui_kit::AnyWindowHandle, Entity<Workspace>, Paths, std::path::PathBuf) {
        let root = std::env::temp_dir().join(format!("tj-ws-{}", new_id()));
        let project = root.join("demo-project");
        std::fs::create_dir_all(&project).unwrap();
        let paths = Paths::in_dir(&root.join("home"));
        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::keymap::init(cx);
        });
        let launch = Launch {
            project: Some(project.clone()),
            paths: Some(paths.clone()),
            hide_terminal: true,
            ..Default::default()
        };
        let (window, workspace) = cx.update(|cx| {
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds(point(px(0.), px(0.)), size(px(1400.), px(900.))))),
                ..Default::default()
            };
            gpui_kit::open_window(options, cx, |window, cx| cx.new(|cx| Workspace::new(launch, window, cx))).unwrap()
        });
        (window, workspace, paths, project)
    }

    #[gpui_kit::test]
    fn sidebar_creates_and_switches_threads_and_saves_them(cx: &mut TestAppContext) {
        let (window, workspace, paths, _) = open(cx);
        let cx = &mut VisualTestContext::from_window(window, cx);
        let (project_id, first) = workspace.read_with(cx, |w, _| {
            let project = &w.state.projects[0];
            (project.id.clone(), w.active.clone().unwrap())
        });
        assert_eq!(workspace.read_with(cx, |w, _| w.state.projects[0].name.clone()), "demo-project");

        cx.update(|window, cx| {
            window.render_frame(cx);
            window.click(SharedString::from(format!("new-thread-{project_id}")), cx);
        });
        let second = workspace.read_with(cx, |w, _| w.active.clone().unwrap());
        assert_ne!(second, first);
        assert_eq!(workspace.read_with(cx, |w, _| w.state.projects[0].threads.len()), 2);

        cx.update(|window, cx| {
            window.render_frame(cx);
            window.click(SharedString::from(format!("thread-{first}")), cx);
        });
        assert_eq!(workspace.read_with(cx, |w, _| w.active.clone()), Some(first.clone()));

        let saved = State::load(&paths).unwrap();
        assert_eq!(saved.projects[0].threads.len(), 2);
        assert_eq!(saved.selected_thread, Some(first));
    }

    #[gpui_kit::test]
    fn project_defaults_apply_to_new_threads(cx: &mut TestAppContext) {
        let (window, workspace, paths, _) = open(cx);
        let cx = &mut VisualTestContext::from_window(window, cx);
        // Make the active thread's account and mode the project's defaults.
        workspace.update(cx, |w, cx| {
            let thread = w.active_thread().unwrap();
            thread.update(cx, |t, cx| t.set_mode(crate::agent::PermissionMode::Plan, cx));
            w.make_project_default_mode(cx);
            w.make_project_default_account(cx);
        });
        let saved = State::load(&paths).unwrap();
        assert_eq!(saved.projects[0].mode, Some(crate::agent::PermissionMode::Plan));
        assert!(saved.projects[0].account.is_some());

        let project_id = saved.projects[0].id.clone();
        workspace.update_in(cx, |w, window, cx| w.new_thread(&project_id, window, cx));
        let mode = workspace.read_with(cx, |w, cx| w.active_thread().unwrap().read(cx).mode);
        assert_eq!(mode, crate::agent::PermissionMode::Plan);
    }
}
