//! The window: a full-height sidebar of projects and threads; the project's panes, where
//! conversations, terminals and the review are tabs; and the overview on the right when
//! there's something in it. Laid out like a GNOME app: no title bar; the headers move it.

use std::{collections::HashMap, path::PathBuf, time::Duration};

use gpui_kit::base::{h_flex, v_flex};
use gpui_kit::component::{
    ActiveTheme as _, Sizable as _,
    button::{Button, ButtonVariants as _},
    popover::Popover,
    slider::{Slider, SliderEvent, SliderState, SliderValue},
    switch::Switch,
};
use gpui_kit::{TestSupportExt as _, prelude::FluentBuilder as _, *};

use crate::agent::{AgentEvent, claude::SessionOptions};
use crate::changes::{ChangeKind, ProjectChanges, ReviewView, Summarizer};
use crate::panes::{PaneEvent, Panes, Start};
use crate::settings::Settings;
use crate::store::SplitDirection;
use crate::store::{self, Accounts, Paths, State};
use crate::thread::{self, Backend, MenuContext, Status, ThreadAction, ThreadChanged, ThreadSetup, ThreadView};
use gpui_kit::assets::IconName;

const SIDEBAR_WIDTH: f32 = 264.0;
const HEADER_HEIGHT: f32 = 46.0;
const OVERVIEW_WIDTH: f32 = 340.0;

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
    /// The shell terminals run; the login shell when `None` (tests use `/bin/sh`).
    pub shell: Option<alacritty_terminal::tty::Shell>,
    /// Don't call Claude for change summaries (tests).
    pub no_summaries: bool,
    /// Open the review of the project's changes at launch (test hook).
    pub review: bool,
}

pub struct Workspace {
    paths: Paths,
    home: PathBuf,
    accounts: Accounts,
    state: State,
    threads: HashMap<String, Entity<ThreadView>>,
    /// Per project: its panes of conversations, terminals and the review.
    panes: HashMap<String, Entity<Panes>>,
    /// Per project: the review of its changes, shown in a tab.
    reviews: HashMap<String, Entity<ReviewView>>,
    /// The conversation the header and overview follow (the last one focused).
    active: Option<String>,
    shell: Option<alacritty_terminal::tty::Shell>,
    model: Option<String>,
    scripted: Option<Vec<(Duration, AgentEvent)>>,
    type_text: Option<String>,
    /// Shown above the thread when something about the setup is wrong.
    problem: Option<SharedString>,
    /// Per project: its uncommitted changes, kept fresh.
    changes: HashMap<String, Entity<ProjectChanges>>,
    no_summaries: bool,
    /// The sidebar-opacity slider in the settings popover.
    opacity_slider: Entity<SliderState>,
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

        let opacity = cx.try_global::<Settings>().map(|s| s.sidebar_opacity).unwrap_or(0.8);
        let opacity_slider =
            cx.new(|_| SliderState::new().min(Settings::MIN_OPACITY).max(1.0).step(0.05).default_value(opacity));
        let slider_events = cx.subscribe(&opacity_slider, |this, _, event: &SliderEvent, cx| {
            let (SliderEvent::Change(value) | SliderEvent::Release(value)) = event;
            if let SliderValue::Single(opacity) = value {
                let opacity = *opacity;
                this.update_settings(cx, |s| s.sidebar_opacity = opacity);
            }
        });
        let mut this = Self {
            paths,
            home,
            accounts,
            state,
            threads: HashMap::new(),
            panes: HashMap::new(),
            reviews: HashMap::new(),
            active: None,
            shell: launch.shell,
            model: launch.model,
            scripted: launch.scripted,
            type_text: launch.type_text,
            problem,
            changes: HashMap::new(),
            no_summaries: launch.no_summaries,
            opacity_slider,
            _subscriptions: vec![slider_events],
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
        if launch.review {
            this.open_review(None, window, cx);
        }
        // Save each terminal's current directory when the window closes.
        let workspace = cx.entity().downgrade();
        window.on_window_should_close(cx, move |_, cx| {
            let _ = workspace.update(cx, |w, cx| w.save_layouts(cx));
            true
        });
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

    /// The thread's view, created (and its transcript replayed) on first use.
    fn ensure_thread(
        &mut self,
        thread_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Entity<ThreadView>> {
        let (project, meta) = self.state.thread(thread_id)?;
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
                if let Some(changes) = this.state.thread(&id).and_then(|(p, _)| this.changes.get(&p.id)).cloned() {
                    changes.update(cx, |c, cx| c.refresh(cx));
                }
                cx.notify();
            }));
            let id = thread_id.to_string();
            self._subscriptions.push(cx.subscribe_in(
                &thread,
                window,
                move |this, _, action: &ThreadAction, window, cx| {
                    // The thread whose menu was used becomes the one the actions apply to.
                    this.active = Some(id.clone());
                    match action {
                        ThreadAction::SwitchAccount(account) => this.switch_account(account, cx),
                        ThreadAction::MakeAccountDefault => this.make_project_default_account(cx),
                        ThreadAction::MakeModeDefault => this.make_project_default_mode(cx),
                        ThreadAction::LogIn => this.log_in(window, cx),
                    }
                },
            ));
            self.threads.insert(thread_id.to_string(), thread);
            self.refresh_menus(cx);
        }
        self.threads.get(thread_id).cloned()
    }

    /// Tell every open thread what its account and mode menus offer (accounts, project defaults).
    fn refresh_menus(&mut self, cx: &mut Context<Self>) {
        let accounts: Vec<(String, String)> =
            self.accounts.accounts.iter().map(|a| (a.id.clone(), a.name.clone())).collect();
        for (id, thread) in &self.threads {
            let Some((project, _)) = self.state.thread(id) else { continue };
            let menus = MenuContext {
                accounts: accounts.clone(),
                project_name: project.name.clone(),
                project_account: self.accounts.resolve(None, project.account.as_deref()).id.clone(),
                project_mode: project.mode.unwrap_or_default(),
            };
            thread.update(cx, |t, cx| {
                t.menus = menus;
                cx.notify();
            });
        }
    }

    /// Show a conversation in its project's panes (focusing its tab, or opening one).
    fn open_thread(&mut self, thread_id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = self.ensure_thread(thread_id, window, cx) else { return };
        let Some(project_id) = self.state.thread(thread_id).map(|(p, _)| p.id.clone()) else { return };
        self.active = Some(thread_id.to_string());
        self.state.selected_thread = self.active.clone();
        let start = Some((thread_id.to_string(), view.clone()));
        if let Some(panes) = self.project_panes(&project_id, start, window, cx) {
            let id = thread_id.to_string();
            panes.update(cx, |p, cx| p.show_thread(&id, view, window, cx));
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
        self.refresh_menus(cx);
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
        self.refresh_menus(cx);
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
        let Some(project_id) = self.active_project().map(|p| p.id.clone()) else { return };
        let Some(panes) = self.project_panes(&project_id, None, window, cx) else { return };
        if panes.read(cx).any_terminal().is_none() {
            panes.update(cx, |p, cx| p.new_terminal(window, cx));
        }
        if let Some(term) = panes.read(cx).any_terminal() {
            term.update(cx, |t, _| t.type_text(&command));
            let focus = term.read(cx).focus_handle(cx);
            window.focus(&focus, cx);
        }
        cx.notify();
    }

    /// Change a setting, save it, and re-apply the theme if following it was switched.
    fn update_settings(&mut self, cx: &mut Context<Self>, change: impl FnOnce(&mut Settings)) {
        if !cx.has_global::<Settings>() {
            cx.set_global(Settings::default());
        }
        let before = cx.global::<Settings>().clone();
        cx.update_global::<Settings, _>(|settings, _| change(settings));
        let after = cx.global::<Settings>().clone();
        if after == before {
            return;
        }
        if let Err(err) = after.save(&self.paths) {
            self.problem = Some(format!("Could not save settings: {err:#}").into());
        }
        if after.follow_omarchy_theme != before.follow_omarchy_theme {
            let font = cx.theme().font_family.to_string();
            crate::omarchy::apply(cx, after.follow_omarchy_theme, Some(&font));
        }
        cx.notify();
    }

    fn settings_popover(&self, cx: &mut Context<Self>) -> AnyElement {
        let this = cx.entity();
        let slider = self.opacity_slider.clone();
        let theme_name = crate::omarchy::theme_name(&self.home);
        Popover::new("settings")
            .anchor(Anchor::TopLeft)
            .trigger(Button::new("settings-button").ghost().small().icon(IconName::Settings).tooltip("Settings"))
            .content(move |_, _window, cx| {
                let settings = cx.try_global::<Settings>().cloned().unwrap_or_default();
                let muted = cx.theme().muted_foreground;
                let (this_follow, this_transparent) = (this.clone(), this.clone());
                let row = |label: &'static str, hint: Option<String>, control: AnyElement| {
                    h_flex()
                        .gap_3()
                        .justify_between()
                        .child(
                            v_flex()
                                .child(div().text_sm().child(label))
                                .children(hint.map(|h| div().text_xs().text_color(muted).child(h))),
                        )
                        .child(control)
                };
                v_flex()
                    .w(px(300.))
                    .gap_3()
                    .p_1()
                    .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).child("Settings"))
                    .child(row(
                        "Follow Omarchy theme",
                        Some(theme_name.clone().unwrap_or_else(|| "No Omarchy theme found".into())),
                        Switch::new("follow-theme")
                            .checked(settings.follow_omarchy_theme)
                            .on_change(move |checked, _, cx| {
                                let checked = *checked;
                                this_follow
                                    .update(cx, |w, cx| w.update_settings(cx, |s| s.follow_omarchy_theme = checked))
                            })
                            .into_any_element(),
                    ))
                    .child(row(
                        "Transparent sidebar",
                        None,
                        Switch::new("transparent-sidebar")
                            .checked(settings.transparent_sidebar)
                            .on_change(move |checked, _, cx| {
                                let checked = *checked;
                                this_transparent
                                    .update(cx, |w, cx| w.update_settings(cx, |s| s.transparent_sidebar = checked))
                            })
                            .into_any_element(),
                    ))
                    .child(
                        v_flex()
                            .gap_1()
                            .child(h_flex().justify_between().text_sm().child("Opacity").child(
                                div().text_color(muted).child(format!("{:.0}%", settings.sidebar_opacity * 100.0)),
                            ))
                            .child(Slider::new(&slider).disabled(!settings.transparent_sidebar)),
                    )
            })
            .into_any_element()
    }

    /// The active project's changes, created (and first read) on demand.
    fn project_changes(&mut self, cx: &mut Context<Self>) -> Option<Entity<ProjectChanges>> {
        let project = self.active_project()?.clone();
        if let Some(changes) = self.changes.get(&project.id) {
            return Some(changes.clone());
        }
        let changes = cx.new(|cx| ProjectChanges::new(project.path.clone(), cx));
        self._subscriptions.push(cx.observe(&changes, |_, _, cx| cx.notify()));
        self.changes.insert(project.id.clone(), changes.clone());
        Some(changes)
    }

    /// The active project's review, created on demand.
    fn project_review(&mut self, cx: &mut Context<Self>) -> Option<Entity<ReviewView>> {
        let changes = self.project_changes(cx)?;
        let project_id = self.active_project()?.id.clone();
        let no_summaries = self.no_summaries;
        Some(
            self.reviews
                .entry(project_id)
                .or_insert_with(|| cx.new(|cx| ReviewView::new(changes, no_summaries, cx)))
                .clone(),
        )
    }

    /// Show the review tab (scrolled to `path`, if given), with summaries being written.
    fn open_review(&mut self, path: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(changes) = self.project_changes(cx) else { return };
        let Some(review) = self.project_review(cx) else { return };
        let summarizer = if self.no_summaries {
            Summarizer::Off
        } else {
            let config_dir = self
                .active_thread()
                .and_then(|t| self.accounts.get(&t.read(cx).account_id).and_then(|a| a.config_path(&self.home)));
            Summarizer::Claude { config_dir }
        };
        let requests = self.active_thread().map(|t| t.read(cx).recent_requests(3)).unwrap_or_default();
        changes.update(cx, |c, cx| {
            c.set_auto_summarize(Some((summarizer, requests)), cx);
            c.refresh(cx);
        });
        if let Some(path) = &path {
            let diff = review.read(cx).changes.read(cx).diff.clone();
            diff.update(cx, |d, cx| d.scroll_to_file(path, cx));
        }
        let Some(project_id) = self.active_project().map(|p| p.id.clone()) else { return };
        if let Some(panes) = self.project_panes(&project_id, None, window, cx) {
            panes.update(cx, |p, cx| p.show_review(window, cx));
        }
        cx.notify();
    }

    /// Goal, Now, Next and tasks for the active thread, with its usage underneath (like pi's sidebar).
    fn overview_panel(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let thread = self.active_thread()?;
        let t = thread.read(cx);
        let theme = cx.theme();
        let (muted, accent, fg) = (theme.muted_foreground, theme.primary, theme.foreground);
        let ov = &t.overview;
        if ov.is_empty() {
            return None;
        }
        let label =
            |text: &'static str| div().text_xs().font_weight(FontWeight::SEMIBOLD).text_color(muted).child(text);
        let field = |name: &'static str, value: &Option<String>, color: Hsla| {
            value.clone().map(|value| {
                v_flex().gap_0p5().child(label(name)).child(div().text_sm().text_color(color).child(value))
            })
        };

        let body = if ov.is_empty() {
            div()
                .text_sm()
                .text_color(muted)
                .child(if t.overview_busy {
                    "Writing the overview…"
                } else {
                    "The overview appears after the first reply."
                })
                .into_any_element()
        } else {
            let tasks = ov.tasks.iter().map(|task| {
                h_flex()
                    .gap_2()
                    .items_start()
                    .text_sm()
                    .child(div().text_color(if task.done { muted } else { accent }).child(if task.done {
                        "✓"
                    } else {
                        "○"
                    }))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .when(task.done, |el| el.text_color(muted).line_through())
                            .child(task.text.clone()),
                    )
            });
            v_flex()
                .gap_2()
                .children(field("GOAL", &ov.goal, fg))
                .children(field("NOW", &ov.now, accent))
                .children(field("NEXT", &ov.next, fg))
                .when(!ov.tasks.is_empty(), |el| {
                    el.child(
                        v_flex()
                            .gap_0p5()
                            .child(h_flex().gap_1().child(label("TASKS")).child(
                                div().text_xs().text_color(muted).child(format!(
                                    "{}/{}",
                                    ov.done_count(),
                                    ov.tasks.len()
                                )),
                            ))
                            .children(tasks),
                    )
                })
                .into_any_element()
        };

        Some(
            v_flex()
                .id("overview")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .gap_2()
                .px_3()
                .py_2()
                .child(
                    h_flex()
                        .gap_2()
                        .child(
                            div()
                                .flex_1()
                                .text_xs()
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(muted)
                                .child("OVERVIEW"),
                        )
                        .when(t.overview_busy && !ov.is_empty(), |el| {
                            el.child(div().text_xs().text_color(muted).child("updating…"))
                        })
                        .child(
                            Button::new("refresh-overview")
                                .xsmall()
                                .ghost()
                                .label("↻")
                                .tooltip("Update the overview now")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    if let Some(thread) = this.active_thread() {
                                        thread.update(cx, |t, cx| t.update_overview(cx));
                                    }
                                })),
                        ),
                )
                .child(body)
                .into_any_element(),
        )
    }

    /// The agent's numbers, pinned to the bottom of the overview pane: context, usage windows,
    /// cost and model.
    fn metrics(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let thread = self.active_thread()?;
        let t = thread.read(cx);
        let theme = cx.theme();
        let (muted, border) = (theme.muted_foreground, theme.border);
        let mut stats = Vec::new();
        if let (Some(tokens), Some(window)) = (t.context_tokens, t.context_window) {
            let percent = tokens as f64 / window.max(1) as f64 * 100.0;
            stats.push(format!("ctx {} / {} · {percent:.0}%", compact(tokens), compact(window)));
        }
        for window in &t.usage {
            let name = match window.id.as_str() {
                "five_hour" => "5h",
                "seven_day" => "7d",
                other => other,
            };
            stats.push(format!("{name} {:.0}%", window.utilization * 100.0));
        }
        if t.cost_usd > 0.0 {
            // On a subscription this is Claude's estimate, not a charge.
            let approx = if t.usage.is_empty() { "" } else { "≈" };
            stats.push(if t.cost_usd < 1.0 {
                format!("{approx}${:.3}", t.cost_usd)
            } else {
                format!("{approx}${:.2}", t.cost_usd)
            });
        }
        if let Some(model) = &t.model {
            stats.push(thread::short_model(model));
        }

        if stats.is_empty() {
            return None;
        }
        Some(
            div()
                .flex_none()
                .px_3()
                .py_2()
                .border_t_1()
                .border_color(border)
                .text_xs()
                .text_color(muted)
                .child(stats.join("  ·  "))
                .into_any_element(),
        )
    }

    fn changes_list(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let changes = self.project_changes(cx)?;
        let touched = self
            .active_thread()
            .zip(self.active_project().map(|p| p.path.clone()))
            .map(|(t, root)| crate::changes::touched_relative(&root, &t.read(cx).touched))
            .unwrap_or_default();
        let model = changes.read(cx);
        if !model.changes.is_repo || model.changes.files.is_empty() {
            return None;
        }
        let theme = cx.theme();
        let (muted, border, accent) = (theme.muted_foreground, theme.border, theme.primary);
        let (added, deleted, modified) = (theme.success, theme.danger, theme.warning);
        let mono = theme.mono_font_family.clone();
        let rows = model.changes.files.iter().enumerate().map(|(ix, file)| {
            let path = file.path.clone();
            let (dir, name) = match path.rsplit_once('/') {
                Some((dir, name)) => (format!("{dir}/"), name.to_string()),
                None => (String::new(), path.clone()),
            };
            let color = match file.kind {
                ChangeKind::Added => added,
                ChangeKind::Deleted => deleted,
                ChangeKind::Modified => modified,
                ChangeKind::Renamed => accent,
            };
            h_flex()
                .id(SharedString::from(format!("change-{ix}")))
                .test_support()
                .gap_2()
                .px_3()
                .py_0p5()
                .cursor_pointer()
                .hover(|el| el.bg(border.opacity(0.5)))
                .on_click(cx.listener(move |this, _, window, cx| this.open_review(Some(path.clone()), window, cx)))
                .child(div().w_3().text_xs().font_family(mono.clone()).text_color(color).child(file.kind.letter()))
                .child(
                    h_flex()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_sm()
                        .child(div().text_color(muted).child(dir))
                        .child(div().child(name)),
                )
                .when(touched.contains(&file.path), |el| el.child(div().size_1p5().rounded_full().bg(accent)))
                .child(
                    div()
                        .text_xs()
                        .font_family(mono.clone())
                        .text_color(muted)
                        .child(format!("+{} −{}", file.additions, file.deletions)),
                )
        });
        Some(
            v_flex()
                .flex_none()
                .max_h(relative(0.4))
                .border_t_1()
                .border_color(border)
                .child(
                    h_flex()
                        .px_3()
                        .py_1()
                        .gap_2()
                        .child(div().text_xs().text_color(muted).child(format!(
                            "Changes · {} files · +{} −{}",
                            model.changes.files.len(),
                            model.changes.additions(),
                            model.changes.deletions()
                        )))
                        .child(div().flex_1())
                        .child(
                            Button::new("review-changes")
                                .xsmall()
                                .ghost()
                                .label("Review")
                                .on_click(cx.listener(|this, _, window, cx| this.open_review(None, window, cx))),
                        ),
                )
                .child(div().id("changes-list").pb_1().overflow_y_scroll().children(rows))
                .into_any_element(),
        )
    }

    /// A project's panes, restored from its saved layout on first use. `start` is the
    /// conversation to show when there's no saved layout.
    fn project_panes(
        &mut self,
        project_id: &str,
        start: Option<(String, Entity<ThreadView>)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Entity<Panes>> {
        if let Some(panes) = self.panes.get(project_id) {
            return Some(panes.clone());
        }
        let project = self.state.project(project_id)?.clone();
        // Saved conversation tabs need their threads first.
        let ids = project.layout.as_ref().map(|l| l.thread_ids()).unwrap_or_default();
        let threads: HashMap<String, Entity<ThreadView>> =
            ids.into_iter().filter_map(|id| self.ensure_thread(&id, window, cx).map(|view| (id, view))).collect();
        let changes = self
            .changes
            .entry(project.id.clone())
            .or_insert_with(|| cx.new(|cx| ProjectChanges::new(project.path.clone(), cx)))
            .clone();
        let no_summaries = self.no_summaries;
        let review = self
            .reviews
            .entry(project.id.clone())
            .or_insert_with(|| cx.new(|cx| ReviewView::new(changes.clone(), no_summaries, cx)))
            .clone();
        let shell = self.shell.clone();
        let panes = cx.new(|cx| {
            Panes::new(
                project.path.clone(),
                project.layout.as_ref(),
                &threads,
                review,
                Start { thread: start, terminal: false },
                shell,
                window,
                cx,
            )
        });
        let id = project.id.clone();
        self._subscriptions.push(cx.subscribe(&panes, move |this, panes, event: &PaneEvent, cx| match event {
            PaneEvent::LayoutChanged => {
                let layout = panes.read(cx).snapshot(cx);
                if let Some(project) = this.state.project_mut(&id) {
                    project.layout = layout;
                }
                this.save();
            }
            PaneEvent::ThreadFocused(thread) => {
                this.active = Some(thread.clone());
                this.state.selected_thread = this.active.clone();
                this.save();
                cx.notify();
            }
            PaneEvent::ReviewClosed => {
                if let Some(changes) = this.changes.get(&id).cloned() {
                    changes.update(cx, |c, cx| c.set_auto_summarize(None, cx));
                }
            }
        }));
        self._subscriptions.push(cx.observe(&changes, |_, _, cx| cx.notify()));
        self._subscriptions.push(cx.observe(&panes, |_, _, cx| cx.notify()));
        if let (Some(text), Some(term)) = (self.type_text.take(), panes.read(cx).any_terminal()) {
            term.update(cx, |t, _| t.type_text(&text.replace("\\r", "\r")));
        }
        self.panes.insert(project.id.clone(), panes.clone());
        Some(panes)
    }

    /// Record every open layout, with each terminal's current directory.
    pub fn save_layouts(&mut self, cx: &mut Context<Self>) {
        let layouts: Vec<_> = self.panes.iter().map(|(id, panes)| (id.clone(), panes.read(cx).snapshot(cx))).collect();
        for (id, layout) in layouts {
            if let Some(project) = self.state.project_mut(&id) {
                project.layout = layout;
            }
        }
        self.save();
    }

    // ---------- rendering ----------

    fn sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let settings = self.settings_popover(cx);
        let theme = cx.theme();
        let (muted, accent_bg, fg) =
            (theme.sidebar_foreground.opacity(0.6), theme.sidebar_accent, theme.sidebar_foreground);
        let (running, waiting) = (theme.warning, theme.primary);
        let alpha = cx.try_global::<Settings>().map(Settings::sidebar_alpha).unwrap_or(1.0);
        let sidebar_bg = theme.sidebar.opacity(alpha);
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
            .bg(sidebar_bg)
            .border_r_1()
            .border_color(theme.sidebar_border)
            .child(
                // "TJ" sits in the middle of the header; the controls and drag areas lie over it.
                div()
                    .relative()
                    .h(px(HEADER_HEIGHT))
                    .flex_none()
                    .child(
                        h_flex()
                            .absolute()
                            .inset_0()
                            .justify_center()
                            .child(div().font_weight(FontWeight::BOLD).text_color(fg).child("TJ")),
                    )
                    .child(
                        h_flex()
                            .absolute()
                            .inset_0()
                            .px_2()
                            .gap_1()
                            .child(
                                Button::new("add-project")
                                    .ghost()
                                    .small()
                                    .icon(IconName::FolderPlus)
                                    .tooltip("Add project")
                                    .on_click(cx.listener(|this, _, window, cx| this.add_project_dialog(window, cx))),
                            )
                            .child(drag_area())
                            .child(settings),
                    ),
            )
            .child(div().id("projects").flex_1().min_h_0().overflow_y_scroll().children(projects))
    }

    /// The top bar: `[badge] Project / Thread title`, like T3 Code, then the pane actions.
    fn content_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (border, warning, muted, accent, fg) =
            (theme.border, theme.warning, theme.muted_foreground, theme.primary, theme.foreground);
        let accent_fg = theme.primary_foreground;
        let thread = self.active_thread().map(|t| t.read(cx));
        let title = thread.and_then(|t| t.title.clone()).unwrap_or_else(|| "New thread".into());
        let status = match thread.map(|t| t.status) {
            Some(Status::Running) => "Working…",
            Some(Status::AwaitingApproval) => "Waiting for you",
            _ => "",
        };
        let project_name = self.active_project().map(|p| p.name.clone());
        let has_panes = self.active_project().is_some_and(|p| self.panes.contains_key(&p.id));

        let pane_button = |id: &'static str, icon: IconName, tip: &'static str| {
            Button::new(id).ghost().small().icon(icon).tooltip(tip)
        };
        let with_panes = |f: fn(&mut Panes, &mut Window, &mut Context<Panes>)| {
            cx.listener(move |this: &mut Self, _: &ClickEvent, window, cx| {
                if let Some(panes) = this.active_project().and_then(|p| this.panes.get(&p.id)).cloned() {
                    panes.update(cx, |p, cx| f(p, window, cx));
                }
            })
        };

        h_flex()
            .h(px(HEADER_HEIGHT))
            .flex_none()
            .gap_2()
            .px_3()
            .border_b_1()
            .border_color(border)
            .when_some(project_name, |el, name| {
                el.child(
                    div()
                        .flex_none()
                        .size_5()
                        .rounded_sm()
                        .bg(accent)
                        .text_color(accent_fg)
                        .text_xs()
                        .font_weight(FontWeight::BOLD)
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(initials(&name)),
                )
                .child(div().flex_none().text_sm().text_color(muted).child(name))
                .child(div().flex_none().text_sm().text_color(muted).child("/"))
            })
            .child(
                div()
                    .min_w_0()
                    .max_w(px(420.))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .text_sm()
                    .text_color(fg)
                    .child(title),
            )
            .child(drag_area())
            .child(div().text_sm().text_color(warning).child(status))
            .when(has_panes, |el| {
                el.child(
                    pane_button("header-new-terminal", IconName::SquareTerminal, "New terminal tab (Ctrl+Shift+T)")
                        .on_click(with_panes(|p, window, cx| p.new_terminal(window, cx))),
                )
                .child(
                    pane_button("header-split-right", IconName::Columns2, "Split right (Ctrl+Shift+E)")
                        .on_click(with_panes(|p, window, cx| p.split(SplitDirection::Right, window, cx))),
                )
                .child(
                    pane_button("header-split-down", IconName::Rows2, "Split down (Ctrl+Shift+O)")
                        .on_click(with_panes(|p, window, cx| p.split(SplitDirection::Down, window, cx))),
                )
            })
            .child(Button::new("close-window").ghost().small().icon(IconName::X).tooltip("Close").on_click(
                cx.listener(|this, _, window, cx| {
                    this.save_layouts(cx);
                    window.remove_window();
                }),
            ))
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

/// "SeaBreeze" -> "SB", "my project" -> "MP", "tj" -> "TJ".
fn initials(name: &str) -> String {
    let words: Vec<&str> = name.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).collect();
    let letters: String = match words.as_slice() {
        [one] => {
            // Within one word, take its first letter and its next capital ("SeaBreeze" -> "SB").
            let mut chars = one.chars();
            let first = chars.next().into_iter();
            let capital = chars.find(|c| c.is_uppercase());
            first.chain(capital.or_else(|| one.chars().nth(1))).collect()
        }
        many => many.iter().take(2).filter_map(|w| w.chars().next()).collect(),
    };
    letters.to_uppercase()
}

/// 12_345 -> "12k", 1_000_000 -> "1M".
fn compact(n: u64) -> String {
    match n {
        0..1_000 => n.to_string(),
        1_000..1_000_000 => format!("{}k", n / 1_000),
        _ => format!("{}M", (n as f64 / 1_000_000.0 * 10.0).round() / 10.0),
    }
}

fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

impl Render for Workspace {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let panes = self.active_project().and_then(|p| self.panes.get(&p.id)).cloned();
        let overview_panel = self.overview_panel(cx);
        let changes_list = self.changes_list(cx);
        let metrics = self.metrics(cx);
        let theme = cx.theme();
        let (bg, fg, border, danger) = (theme.background, theme.foreground, theme.border, theme.danger);
        // The overview pane only appears when it has something to show.
        let right_pane = overview_panel.is_some() || changes_list.is_some();

        // The window itself is transparent: only the content area paints an opaque background,
        // so the sidebar can be see-through.
        h_flex().size_full().text_color(fg).child(self.sidebar(cx)).child(
            v_flex()
                .flex_1()
                .min_w_0()
                .h_full()
                .bg(bg)
                .child(self.content_header(cx))
                .when_some(self.problem.clone(), |el, problem| {
                    el.child(div().px_4().py_2().text_sm().text_color(danger).child(problem))
                })
                .child(h_flex().flex_1().min_h_0().child(div().flex_1().min_w_0().h_full().children(panes)).when(
                    right_pane,
                    |el| {
                        el.child(
                            // Summary and tasks take the space; changed files sit above the agent's
                            // numbers, which are pinned to the bottom.
                            v_flex()
                                .w(px(OVERVIEW_WIDTH))
                                .flex_none()
                                .h_full()
                                .border_l_1()
                                .border_color(border)
                                .child(v_flex().flex_1().min_h_0().children(overview_panel))
                                .children(changes_list)
                                .children(metrics),
                        )
                    },
                )),
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
            shell: Some(alacritty_terminal::tty::Shell::new("/bin/sh".into(), vec![])),
            no_summaries: true,
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
    fn clicking_a_changed_file_opens_the_review_tab_and_closing_it_returns(cx: &mut TestAppContext) {
        let (window, workspace, _, project) = open(cx);
        let cx = &mut VisualTestContext::from_window(window, cx);
        let run = |args: &[&str]| {
            assert!(
                std::process::Command::new("git").arg("-C").arg(&project).args(args).output().unwrap().status.success()
            )
        };
        run(&["init", "-q"]);
        std::fs::write(project.join("notes.md"), "a new file\n").unwrap();

        // Changes are read in the background; wait for them to arrive.
        let changes = workspace.update(cx, |w, cx| w.project_changes(cx).unwrap());
        cx.run_until_parked();
        for _ in 0..200 {
            if changes.read_with(cx, |c, _| !c.changes.files.is_empty()) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
            cx.run_until_parked();
        }
        assert_eq!(changes.read_with(cx, |c, _| c.changes.files[0].path.clone()), "notes.md");

        cx.update(|window, cx| {
            window.render_frame(cx);
            window.click(SharedString::from("change-0"), cx);
        });
        let panes = workspace.read_with(cx, |w, _| w.panes.values().next().cloned().unwrap());
        assert!(panes.read_with(cx, |p, _| p.review_is_showing()), "the review opens as a tab");

        // Ctrl+Shift+W closes it like any tab, and the conversation shows again.
        cx.update(|window, cx| {
            window.render_frame(cx);
            window.press("ctrl-shift-w", cx);
        });
        assert!(!panes.read_with(cx, |p, _| p.review_is_showing()));
        assert!(panes.read_with(cx, |p, _| p.active_thread_id().is_some()));
    }

    #[gpui_kit::test]
    fn the_settings_popover_turns_on_the_transparent_sidebar_and_saves_it(cx: &mut TestAppContext) {
        let (window, _workspace, paths, _) = open(cx);
        let cx = &mut VisualTestContext::from_window(window, cx);
        cx.update(|_, cx| cx.set_global(crate::settings::Settings::default()));
        cx.update(|window, cx| {
            window.render_frame(cx);
            window.click(SharedString::from("settings-button"), cx);
        });
        cx.run_until_parked();
        cx.update(|window, cx| {
            window.render_frame(cx);
            window.click(SharedString::from("transparent-sidebar"), cx);
        });
        let settings = cx.update(|_, cx| cx.global::<crate::settings::Settings>().clone());
        assert!(settings.transparent_sidebar);
        assert_eq!(settings.sidebar_alpha(), 0.8);
        assert_eq!(crate::settings::Settings::load(&paths), settings, "the change was saved");
    }

    #[gpui_kit::test]
    fn composer_menu_actions_switch_the_account_and_set_project_defaults(cx: &mut TestAppContext) {
        let (window, workspace, paths, _) = open(cx);
        let cx = &mut VisualTestContext::from_window(window, cx);
        // The test machine may have one account; add a second so there's something to switch to.
        workspace.update(cx, |w, cx| {
            w.accounts.accounts.push(crate::store::Account {
                id: "work".into(),
                name: "Work".into(),
                config_dir: Some("/tmp/tj-work".into()),
            });
            w.refresh_menus(cx);
        });
        let thread = workspace.read_with(cx, |w, _| w.active_thread().unwrap());
        assert!(thread.read_with(cx, |t, _| t.menus.accounts.iter().any(|(id, _)| id == "work")));

        thread.update(cx, |_, cx| cx.emit(crate::thread::ThreadAction::SwitchAccount("work".into())));
        cx.run_until_parked();
        assert_eq!(thread.read_with(cx, |t, _| t.account_id.clone()), "work");

        thread.update(cx, |t, cx| t.set_mode(crate::agent::PermissionMode::AcceptEdits, cx));
        thread.update(cx, |_, cx| cx.emit(crate::thread::ThreadAction::MakeModeDefault));
        thread.update(cx, |_, cx| cx.emit(crate::thread::ThreadAction::MakeAccountDefault));
        cx.run_until_parked();
        let saved = State::load(&paths).unwrap();
        assert_eq!(saved.projects[0].mode, Some(crate::agent::PermissionMode::AcceptEdits));
        assert_eq!(saved.projects[0].account.as_deref(), Some("work"));
        let menus = thread.read_with(cx, |t, _| t.menus.clone());
        assert_eq!(menus.project_account, "work", "the menus learn the new defaults");
        assert_eq!(menus.project_mode, crate::agent::PermissionMode::AcceptEdits);
    }

    #[gpui_kit::test]
    fn the_top_bar_splits_the_focused_pane_and_tabs_appear_only_when_needed(cx: &mut TestAppContext) {
        let (window, workspace, _, _) = open(cx);
        let cx = &mut VisualTestContext::from_window(window, cx);
        let panes = workspace.read_with(cx, |w, _| w.panes.values().next().cloned().unwrap());
        assert_eq!(panes.read_with(cx, |p, _| p.shape()), vec![1], "a new project starts with just its conversation");
        cx.update(|window, cx| {
            window.render_frame(cx);
            assert!(window.try_find(SharedString::from("tab-1-0")).is_none(), "no tab strip for a single tab");
            window.click(SharedString::from("header-new-terminal"), cx);
            window.render_frame(cx);
            assert!(window.try_find(SharedString::from("tab-1-1")).is_some(), "a second tab shows the strip");
            window.click(SharedString::from("header-split-right"), cx);
        });
        assert_eq!(panes.read_with(cx, |p, _| p.shape()), vec![2, 1]);
    }

    #[test]
    fn project_initials() {
        assert_eq!(super::initials("SeaBreeze"), "SB");
        assert_eq!(super::initials("my project"), "MP");
        assert_eq!(super::initials("tj"), "TJ");
        assert_eq!(super::initials("nautilus-omarchy"), "NO");
        assert_eq!(super::initials("Vmux"), "VM");
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
