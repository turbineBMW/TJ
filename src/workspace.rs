//! The window: a full-height sidebar of projects and threads, and the project's panes, where
//! the agent (the picked thread with its status column), terminals and the review are tabs.
//! Laid out like a GNOME app: no title bar; the headers move it.

use std::{collections::HashMap, path::PathBuf, time::Duration};

use gpui_kit::base::{h_flex, v_flex};
use gpui_kit::component::{
    ActiveTheme as _, Icon, Sizable as _, WindowExt as _,
    button::{Button, ButtonVariant, ButtonVariants as _},
    input::{Input, InputEvent, InputState},
    menu::{ContextMenuExt as _, PopupMenu, PopupMenuItem},
    popover::Popover,
    slider::{Slider, SliderEvent, SliderState, SliderValue},
    switch::Switch,
};
use gpui_kit::{TestSupportExt as _, prelude::FluentBuilder as _, *};

use crate::agent::{AgentEvent, claude::SessionOptions};
use crate::agent_slot::{AgentSlot, AgentSlotEvent};
use crate::changes::{ProjectChanges, ReviewView, Summarizer};
use crate::panes::{FLOAT_HEIGHT, FLOAT_INSET, PaneEvent, Panes, Start};
use crate::settings::Settings;
use crate::store::SplitDirection;
use crate::store::{self, Accounts, Paths, State};
use crate::thread::{
    self, Backend, MenuContext, Status, ThreadAction, ThreadChanged, ThreadSent, ThreadSetup, ThreadView,
};
use gpui_kit::assets::IconName;

const SIDEBAR_WIDTH: f32 = 264.0;
const HEADER_HEIGHT: f32 = 46.0;

// The top bar's floating things sit in its middle.
const _: () = assert!(HEADER_HEIGHT == FLOAT_HEIGHT + 2.0 * FLOAT_INSET);

pub const CONTEXT: &str = "Workspace";

gpui_kit::actions!(workspace, [ToggleSidebar]);

/// Shortcuts for the whole window. `Ctrl+Alt`, so the terminal never needs the key.
pub fn bindings() -> Vec<KeyBinding> {
    vec![KeyBinding::new("ctrl-alt-s", ToggleSidebar, Some(CONTEXT))]
}
/// The agent mark at the end of a thread's row in the sidebar.
const MARK_SIZE: f32 = 14.0;
/// An idle thread's Claude process (about 300 MB) is stopped after this long; the next
/// message resumes the session in a new one.
const IDLE_STOP: Duration = Duration::from_secs(10 * 60);
/// At most this many idle Claude processes are kept, the most recently used.
const IDLE_KEEP: usize = 3;

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
    /// Don't call Claude for change summaries or the model list (tests).
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
    /// Per project: its panes of the agent, terminals and the review.
    panes: HashMap<String, Entity<Panes>>,
    /// Per project: its agent, shown in the agent tab.
    agents: HashMap<String, Entity<AgentSlot>>,
    /// Per project: the review of its changes, shown in a tab.
    reviews: HashMap<String, Entity<ReviewView>>,
    /// The thread the header follows (the one last shown or focused).
    active: Option<String>,
    shell: Option<alacritty_terminal::tty::Shell>,
    model: Option<String>,
    scripted: Option<Vec<(Duration, AgentEvent)>>,
    type_text: Option<String>,
    /// Shown above the thread when something about the setup is wrong.
    problem: Option<SharedString>,
    /// Per project: its uncommitted changes, kept fresh.
    changes: HashMap<String, Entity<ProjectChanges>>,
    /// Per account: the models `claude` offers it (loaded once, in the background).
    models: HashMap<String, Vec<crate::agent::ModelChoice>>,
    models_loading: std::collections::HashSet<String>,
    no_summaries: bool,
    /// The sidebar-opacity slider in the settings popover.
    opacity_slider: Entity<SliderState>,
    /// The cursor-animation slider (milliseconds; 0 is off).
    glide_slider: Entity<SliderState>,
    /// The thread whose title is being edited in the sidebar, and the editor.
    renaming: Option<(String, Entity<InputState>)>,
    /// Projects whose archived threads are listed.
    show_archived: std::collections::HashSet<String>,
    /// Per project: the icon found in its folder, once looked for (`None` inside: there isn't one).
    found_icons: HashMap<String, Option<PathBuf>>,
    _subscriptions: Vec<Subscription>,
    /// Checks every minute for Claude processes idle long enough to stop.
    _idle_check: Option<Task<()>>,
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
        let glide =
            cx.try_global::<Settings>().map(|s| s.cursor_glide_ms).unwrap_or(Settings::default().cursor_glide_ms);
        let glide_slider = cx.new(|_| {
            SliderState::new().min(0.).max(Settings::MAX_GLIDE_MS as f32).step(10.).default_value(glide as f32)
        });
        let glide_events = cx.subscribe(&glide_slider, |this, _, event: &SliderEvent, cx| {
            let (SliderEvent::Change(value) | SliderEvent::Release(value)) = event;
            if let SliderValue::Single(ms) = value {
                let ms = ms.round().max(0.) as u32;
                this.update_settings(cx, |s| s.cursor_glide_ms = ms);
            }
        });
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
            agents: HashMap::new(),
            reviews: HashMap::new(),
            active: None,
            shell: launch.shell,
            model: launch.model,
            scripted: launch.scripted,
            type_text: launch.type_text,
            problem,
            changes: HashMap::new(),
            models: HashMap::new(),
            models_loading: Default::default(),
            renaming: None,
            show_archived: Default::default(),
            found_icons: HashMap::new(),
            no_summaries: launch.no_summaries,
            opacity_slider,
            glide_slider,
            _subscriptions: vec![slider_events, glide_events],
            _idle_check: None,
        };
        this._idle_check = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(60)).await;
                if this.update(cx, |w: &mut Workspace, cx| w.stop_idle_processes(cx)).is_err() {
                    break;
                }
            }
        }));

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
        this.find_icons(cx);
        // Save each terminal's current directory when the window closes.
        let workspace = cx.entity().downgrade();
        window.on_window_should_close(cx, move |_, cx| {
            let _ = workspace.update(cx, |w, cx| w.save_layouts(cx));
            true
        });
        this.save();
        this
    }

    /// Fold the sidebar away, or bring it back. Remembered across restarts.
    fn toggle_sidebar(&mut self, cx: &mut Context<Self>) {
        self.state.sidebar_collapsed = !self.state.sidebar_collapsed;
        self.save();
        cx.notify();
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
            // The thread's own model and effort, else the project's, else `--model` / defaults.
            let model_choice = meta.model.clone().or(project.model.clone()).or(self.model.clone());
            let effort = meta.effort.clone().or(project.effort.clone());
            let setup = ThreadSetup {
                model_choice: model_choice.clone(),
                effort: effort.clone(),
                title: meta.title.clone(),
                named: meta.named,
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
                    model: model_choice,
                    effort,
                    permission_mode: mode,
                    ..Default::default()
                }),
            };
            let thread = cx.new(|cx| ThreadView::open(setup, backend, window, cx));
            let id = thread_id.to_string();
            self._subscriptions.push(cx.subscribe(&thread, move |this, _, _: &ThreadChanged, cx| {
                this.sync_thread(&id, cx);
                // A turn that just ended may put one idle process over the limit.
                this.stop_idle_processes(cx);
                if let Some(changes) = this.state.thread(&id).and_then(|(p, _)| this.changes.get(&p.id)).cloned() {
                    changes.update(cx, |c, cx| c.refresh(cx));
                }
                cx.notify();
            }));
            let id = thread_id.to_string();
            self._subscriptions.push(cx.subscribe(&thread, move |this, _, _: &ThreadSent, cx| {
                this.state.touch_thread(&id);
                this.save();
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
                        ThreadAction::MakeModelDefault => this.make_project_default_model(cx),
                        ThreadAction::LogIn => this.log_in(window, cx),
                    }
                },
            ));
            self.load_models(&account.id, cx);
            self.threads.insert(thread_id.to_string(), thread);
            self.refresh_menus(cx);
        }
        self.threads.get(thread_id).cloned()
    }

    /// Ask `claude` once, in the background, which models an account offers.
    fn load_models(&mut self, account_id: &str, cx: &mut Context<Self>) {
        if self.no_summaries
            || self.models.contains_key(account_id)
            || !self.models_loading.insert(account_id.to_string())
        {
            return;
        }
        let config_dir = self.accounts.get(account_id).and_then(|a| a.config_path(&self.home));
        let id = account_id.to_string();
        cx.spawn(async move |this, cx| {
            let models = cx
                .background_executor()
                .spawn(async move { crate::agent::claude::list_models(config_dir.as_deref()) })
                .await;
            let _ = this.update(cx, |this: &mut Workspace, cx| {
                this.models_loading.remove(&id);
                match models {
                    Ok(models) => {
                        this.models.insert(id, models);
                        this.refresh_menus(cx);
                    }
                    // The fallback list stays; the session itself still works.
                    Err(err) => eprintln!("tj: could not list models: {err:#}"),
                }
            });
        })
        .detach();
    }

    fn make_project_default_model(&mut self, cx: &mut Context<Self>) {
        let Some(thread) = self.active_thread() else { return };
        let (model, effort) = {
            let t = thread.read(cx);
            (t.model_choice.clone(), t.effort.clone())
        };
        let Some(project_id) = self.active_project().map(|p| p.id.clone()) else { return };
        if let Some(project) = self.state.project_mut(&project_id) {
            project.model = model;
            project.effort = effort;
        }
        if let Some(id) = self.active.clone() {
            self.sync_thread(&id, cx);
        }
        self.refresh_menus(cx);
        cx.notify();
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
                project_dir: display_path(&project.path, &self.home),
                branch: git_branch(&project.path),
                models: self
                    .models
                    .get(&thread.read(cx).account_id)
                    .cloned()
                    .unwrap_or_else(crate::agent::ModelChoice::fallback),
                project_model: project.model.clone(),
                project_effort: project.effort.clone(),
            };
            thread.update(cx, |t, cx| {
                t.menus = menus;
                cx.notify();
            });
        }
    }

    /// Show a thread in its project's agent tab, in place of the one there.
    pub(crate) fn open_thread(&mut self, thread_id: &str, window: &mut Window, cx: &mut Context<Self>) {
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
        let (project_model, project_effort) = (project.model.clone(), project.effort.clone());
        let t = thread.read(cx);
        let (title, session_id, account_id, mode) =
            (t.title.clone(), t.session_id.clone(), t.account_id.clone(), t.mode);
        let (model, effort, named) = (t.model_choice.clone(), t.effort.clone(), t.named);
        let Some(meta) = self.state.thread_mut(thread_id) else { return };
        meta.named = named;
        let changed = meta.title != title || meta.session_id != session_id;
        meta.title = title;
        meta.session_account = session_id.as_ref().map(|_| account_id.clone());
        meta.session_id = session_id;
        meta.account = (account_id != project_account).then_some(account_id);
        meta.mode = (mode != project_mode).then_some(mode);
        meta.model = model.filter(|m| Some(m) != project_model.as_ref());
        meta.effort = effort.filter(|e| Some(e) != project_effort.as_ref());
        if changed {
            meta.updated_at = store::now();
        }
        self.save();
    }

    // ---------- renaming, archiving, deleting ----------

    /// Edit a thread's title in place in the sidebar.
    fn start_rename(&mut self, thread_id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some((_, meta)) = self.state.thread(thread_id) else { return };
        let title = meta.title.clone().unwrap_or_default();
        let input = cx.new(|cx| InputState::new(window, cx).default_value(title).placeholder("Thread name"));
        input.update(cx, |i, cx| {
            i.focus(window, cx);
            i.select_all(window, cx);
        });
        let id = thread_id.to_string();
        self._subscriptions.push(cx.subscribe_in(&input, window, move |this, _, event: &InputEvent, _, cx| {
            if matches!(event, InputEvent::PressEnter { .. } | InputEvent::Blur)
                && this.renaming.as_ref().is_some_and(|(r, _)| *r == id)
            {
                this.finish_rename(true, cx);
            }
        }));
        self.renaming = Some((thread_id.to_string(), input));
        cx.notify();
    }

    /// Keep (or drop) the title being typed. An empty title lets TJ name the thread again.
    fn finish_rename(&mut self, keep: bool, cx: &mut Context<Self>) {
        let Some((id, input)) = self.renaming.take() else { return };
        if keep {
            let title = input.read(cx).value().trim().to_string();
            self.rename_thread(&id, &title, cx);
        }
        cx.notify();
    }

    fn rename_thread(&mut self, thread_id: &str, title: &str, cx: &mut Context<Self>) {
        if let Some(thread) = self.threads.get(thread_id).cloned() {
            // The thread works out a fallback title for an empty one; sync_thread saves it.
            thread.update(cx, |t, cx| t.rename(title, cx));
        } else if let Some(meta) = self.state.thread_mut(thread_id) {
            meta.named = !title.is_empty();
            meta.title = meta.named.then(|| title.to_string()).or(meta.title.take());
            self.save();
        }
        cx.notify();
    }

    fn set_archived(&mut self, thread_id: &str, archived: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(meta) = self.state.thread_mut(thread_id) else { return };
        meta.archived = archived;
        if archived {
            self.close_thread_tab(thread_id, cx);
            if self.active.as_deref() == Some(thread_id) {
                self.active = None;
                self.pick_active(thread_id, window, cx);
            }
        }
        self.save();
        cx.notify();
    }

    fn confirm_delete_thread(&mut self, thread_id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some((_, meta)) = self.state.thread(thread_id) else { return };
        let title = meta.title.clone().unwrap_or_else(|| "New thread".into());
        let (workspace, id) = (cx.entity().downgrade(), thread_id.to_string());
        window.open_alert_dialog(cx, move |alert, _, _| {
            let (workspace, id) = (workspace.clone(), id.clone());
            alert
                .confirm()
                .title(format!("Delete “{title}”?"))
                .description("Its conversation, attached images and overview are deleted from TJ. Files the agent changed in the project are not touched.")
                .ok_text("Delete")
                .ok_variant(ButtonVariant::Danger)
                .on_ok(move |_, window, cx| {
                    let _ = workspace.update(cx, |w, cx| w.delete_thread(&id, window, cx));
                    true
                })
        });
    }

    fn delete_thread(&mut self, thread_id: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.close_thread_tab(thread_id, cx);
        let was_active = self.active.as_deref() == Some(thread_id);
        if was_active {
            self.active = None;
            // Picked while the thread is still known, so its project's threads come first.
            self.pick_active(thread_id, window, cx);
        }
        // Dropping the view ends its Claude process.
        self.threads.remove(thread_id);
        self.state.remove_thread(thread_id);
        self.paths.delete_thread_files(thread_id);
        self.save();
        cx.notify();
    }

    fn confirm_remove_project(&mut self, project_id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(project) = self.state.project(project_id) else { return };
        let (name, count) = (project.name.clone(), project.threads.len());
        let threads = match count {
            0 => "It has no threads.".to_string(),
            1 => "Its thread is deleted from TJ.".to_string(),
            n => format!("Its {n} threads are deleted from TJ."),
        };
        let (workspace, id) = (cx.entity().downgrade(), project_id.to_string());
        window.open_alert_dialog(cx, move |alert, _, _| {
            let (workspace, id) = (workspace.clone(), id.clone());
            alert
                .confirm()
                .title(format!("Remove {name} from TJ?"))
                .description(format!(
                    "{threads} The project's folder and files are not touched; add it again any time."
                ))
                .ok_text("Remove")
                .ok_variant(ButtonVariant::Danger)
                .on_ok(move |_, window, cx| {
                    let _ = workspace.update(cx, |w, cx| w.remove_project(&id, window, cx));
                    true
                })
        });
    }

    fn remove_project(&mut self, project_id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(project) = self.state.remove_project(project_id) else { return };
        self.delete_image_copy(project.image.clone());
        self.found_icons.remove(project_id);
        for meta in &project.threads {
            self.threads.remove(&meta.id);
            self.paths.delete_thread_files(&meta.id);
        }
        // Its panes go too: their terminals' shells end with them.
        self.panes.remove(project_id);
        self.agents.remove(project_id);
        self.reviews.remove(project_id);
        self.changes.remove(project_id);
        self.show_archived.remove(project_id);
        if let Some(gone) = self.active.take_if(|id| project.threads.iter().any(|t| &t.id == id)) {
            self.pick_active(&gone, window, cx);
        }
        self.save();
        cx.notify();
    }

    fn close_thread_tab(&mut self, thread_id: &str, cx: &mut Context<Self>) {
        let Some(project_id) = self.state.thread(thread_id).map(|(p, _)| p.id.clone()) else { return };
        if let Some(panes) = self.panes.get(&project_id).cloned() {
            panes.update(cx, |p, cx| p.close_thread(thread_id, cx));
        }
    }

    /// After the active thread (`gone`) went away: the next thread in its project, else the
    /// first in any other project (archived ones aside), else nothing.
    fn pick_active(&mut self, gone: &str, window: &mut Window, cx: &mut Context<Self>) {
        let project = self.state.thread(gone).map(|(p, _)| p.id.clone());
        let candidates = |same: bool| {
            self.state
                .projects
                .iter()
                .filter(|p| (Some(&p.id) == project.as_ref()) == same)
                .flat_map(|p| p.threads.iter())
                .find(|t| !t.archived && t.id != gone)
                .map(|t| t.id.clone())
        };
        let next = candidates(true).or_else(|| candidates(false));
        match next {
            Some(id) => self.open_thread(&id, window, cx),
            None => self.state.selected_thread = None,
        }
    }

    // ---------- idle processes ----------

    /// Stop the Claude processes [`processes_to_stop`] picks, to free their memory.
    fn stop_idle_processes(&mut self, cx: &mut Context<Self>) {
        let idle: Vec<(String, Duration, bool)> = self
            .threads
            .iter()
            .filter_map(|(id, t)| t.read(cx).idle_for().map(|d| (id.clone(), d, self.active.as_ref() == Some(id))))
            .collect();
        for id in processes_to_stop(idle) {
            if let Some(thread) = self.threads.get(&id) {
                thread.update(cx, |t, _| t.stop_idle_process());
            }
        }
    }

    // ---------- history ----------

    /// A thread the sidebar lists even past its first few: the open one, one being renamed,
    /// and one working or waiting for the user, so none of those is lost in the history.
    fn pinned(&self, meta: &store::ThreadMeta, cx: &App) -> bool {
        self.active.as_deref() == Some(meta.id.as_str())
            || self.renaming.as_ref().is_some_and(|(id, _)| *id == meta.id)
            || self.threads.get(&meta.id).is_some_and(|t| t.read(cx).status != Status::Idle)
    }

    /// The project's threads the sidebar doesn't list, last touched first. Only their saved
    /// details: a thread's conversation is read when it's opened.
    pub(crate) fn history(&self, project_id: &str, cx: &App) -> Vec<store::ThreadMeta> {
        let Some(project) = self.state.project(project_id) else { return Vec::new() };
        store::split_threads(&project.threads, |t| self.pinned(t, cx)).1.into_iter().cloned().collect()
    }

    fn open_history(&mut self, project_id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(name) = self.state.project(project_id).map(|p| p.name.clone()) else { return };
        let workspace = cx.entity().downgrade();
        let id = project_id.to_string();
        let view = cx.new(|cx| crate::history::HistoryView::new(workspace, id, window, cx));
        let title = SharedString::from(format!("{name} history"));
        let shown = view.clone();
        window.open_dialog(cx, move |dialog, _, _| dialog.w(px(560.)).title(title.clone()).child(shown.clone()));
        view.update(cx, |v, cx| v.focus(window, cx));
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
                this.find_icons(cx);
                this.new_thread(&project, window, cx);
            });
        })
        .detach();
    }

    // ---------- project pictures ----------

    /// Look, in the background, for an icon in each project not searched yet.
    fn find_icons(&mut self, cx: &mut Context<Self>) {
        for project in &self.state.projects {
            if self.found_icons.contains_key(&project.id) {
                continue;
            }
            // Mark it searched now, so it's searched once.
            self.found_icons.insert(project.id.clone(), None);
            let (id, path, name) = (project.id.clone(), project.path.clone(), project.name.clone());
            cx.spawn(async move |this, cx| {
                let icon =
                    cx.background_executor().spawn(async move { crate::project_image::find_icon(&path, &name) }).await;
                let _ = this.update(cx, |this: &mut Workspace, cx| {
                    if icon.is_some() && this.found_icons.contains_key(&id) {
                        this.found_icons.insert(id, icon);
                        cx.notify();
                    }
                });
            })
            .detach();
        }
    }

    /// The picture to show for a project: the chosen one, else the icon found in it; `None` for
    /// its initials.
    fn project_picture(&self, project: &store::Project) -> Option<PathBuf> {
        match &project.image {
            Some(store::ProjectImage::Initials) => None,
            Some(store::ProjectImage::File { path }) if path.is_file() => Some(path.clone()),
            // A chosen picture that has gone missing falls back to the project's own icon.
            _ => self.found_icons.get(&project.id).cloned().flatten(),
        }
    }

    fn choose_project_image(&mut self, project_id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Use as project image".into()),
        });
        let project_id = project_id.to_string();
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = picked.await else { return };
            let Some(path) = paths.into_iter().next() else { return };
            let _ = this.update(cx, |this: &mut Workspace, cx| {
                if let Err(err) = this.use_image_file(&project_id, &path) {
                    this.problem =
                        Some(format!("Could not use {} as the project image: {err:#}", path.display()).into());
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Copy `source` into TJ's data folder (so the picture stays if the original moves) and use it.
    fn use_image_file(&mut self, project_id: &str, source: &std::path::Path) -> anyhow::Result<()> {
        anyhow::ensure!(crate::project_image::is_image(source), "it isn't an image TJ can show");
        let dir = self.paths.project_images_dir();
        std::fs::create_dir_all(&dir)?;
        let ext = source.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_else(|| "png".into());
        // A fresh name each time, so the image cache never shows the previous picture.
        let copy = dir.join(format!("{project_id}-{}.{ext}", &store::new_id()[..8]));
        std::fs::copy(source, &copy)?;
        self.set_project_image(project_id, Some(store::ProjectImage::File { path: copy }));
        Ok(())
    }

    /// Set (or, with `None`, reset to the found icon) a project's picture, deleting the copy of
    /// the one it replaces.
    fn set_project_image(&mut self, project_id: &str, image: Option<store::ProjectImage>) {
        let Some(project) = self.state.project_mut(project_id) else { return };
        let old = std::mem::replace(&mut project.image, image);
        self.delete_image_copy(old);
        self.problem = None;
        self.save();
    }

    fn delete_image_copy(&self, image: Option<store::ProjectImage>) {
        if let Some(store::ProjectImage::File { path }) = image
            && path.starts_with(self.paths.project_images_dir())
        {
            let _ = std::fs::remove_file(path);
        }
    }

    /// The project's picture, or its initials on the accent colour, `size` pixels square.
    fn project_avatar(&self, project: &store::Project, size: f32, cx: &App) -> AnyElement {
        let theme = cx.theme();
        let (accent, accent_fg) = (theme.primary, theme.primary_foreground);
        let name = project.name.clone();
        let badge = move || {
            div()
                .flex_none()
                .size(px(size))
                .rounded_sm()
                .bg(accent)
                .text_color(accent_fg)
                .text_size(px((size * 0.5).round()))
                .font_weight(FontWeight::BOLD)
                .flex()
                .items_center()
                .justify_center()
                .child(initials(&name))
                .into_any_element()
        };
        match self.project_picture(project) {
            Some(path) => img(path)
                .flex_none()
                .size(px(size))
                .rounded_sm()
                .object_fit(ObjectFit::Contain)
                .with_fallback(badge)
                .into_any_element(),
            None => badge(),
        }
    }

    /// What a project's right-click menu can offer about its picture.
    fn image_choices(&self, project_id: &str, cx: &Context<Self>) -> ImageChoices {
        let project = self.state.project(project_id);
        ImageChoices {
            workspace: cx.entity(),
            project_id: project_id.to_string(),
            showing: project.is_some_and(|p| self.project_picture(p).is_some()),
            chosen: project.is_some_and(|p| p.image.is_some()),
            found: self.found_icons.get(project_id).is_some_and(Option::is_some),
        }
    }

    // ---------- accounts and modes ----------

    fn switch_account(&mut self, account_id: &str, cx: &mut Context<Self>) {
        let (Some(thread), Some(account)) = (self.active_thread(), self.accounts.get(account_id).cloned()) else {
            return;
        };
        let config_dir = account.config_path(&self.home);
        thread.update(cx, |t, cx| t.set_account(&account.id, &account.name, config_dir, cx));
        self.load_models(&account.id, cx);
        self.refresh_menus(cx);
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
        let glide_slider = self.glide_slider.clone();
        let theme_name = crate::omarchy::theme_name(&self.home);
        Popover::new("settings")
            .anchor(Anchor::TopLeft)
            .trigger(Button::new("settings-button").ghost().small().icon(IconName::Settings).tooltip("Settings"))
            .content(move |_, _window, cx| {
                let settings = cx.try_global::<Settings>().cloned().unwrap_or_default();
                let muted = cx.theme().muted_foreground;
                let (this_follow, this_transparent) = (this.clone(), this.clone());
                let glide = match settings.cursor_glide_ms {
                    0 => "Off".to_string(),
                    ms => format!("{ms} ms"),
                };
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
                    .child(
                        v_flex()
                            .gap_1()
                            .child(
                                h_flex()
                                    .justify_between()
                                    .text_sm()
                                    .child("Cursor animation")
                                    .child(div().text_color(muted).child(glide)),
                            )
                            .child(Slider::new(&glide_slider)),
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

    /// A project's panes, restored from its saved layout on first use. `start` is the thread
    /// to show in the agent tab.
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
        let agent = cx.new(|cx| AgentSlot::new(project.path.clone(), changes.clone(), cx));
        self._subscriptions.push(cx.subscribe_in(
            &agent,
            window,
            |this, _, AgentSlotEvent::OpenReview(path): &AgentSlotEvent, window, cx| {
                this.open_review(path.clone(), window, cx)
            },
        ));
        self._subscriptions.push(cx.observe(&agent, |_, _, cx| cx.notify()));
        self.agents.insert(project.id.clone(), agent.clone());
        let shell = self.shell.clone();
        let panes = cx.new(|cx| {
            Panes::new(
                project.path.clone(),
                project.layout.as_ref(),
                agent,
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

    fn sidebar(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let settings = self.settings_popover(cx);
        // A working thread's agent mark breathes instead of showing a dot.
        let working = self.threads.values().any(|t| t.read(cx).status == Status::Running);
        let breath = if working { crate::working::breath(crate::working::tick(window, cx)) } else { 1.0 };
        let theme = cx.theme();
        let (muted, accent_bg, fg) =
            (theme.sidebar_foreground.opacity(0.6), theme.sidebar_accent, theme.sidebar_foreground);
        let waiting = theme.primary;
        let alpha = cx.try_global::<Settings>().map(Settings::sidebar_alpha).unwrap_or(1.0);
        let sidebar_bg = theme.sidebar.opacity(alpha);
        let active = self.active.clone();

        let workspace = cx.entity();
        let renaming = self.renaming.clone();
        let thread_row = |meta: &store::ThreadMeta, cx: &mut Context<Self>| {
            let id = meta.id.clone();
            let status = self.threads.get(&meta.id).map(|t| t.read(cx).status).unwrap_or(Status::Idle);
            let dot = (status == Status::AwaitingApproval).then_some(waiting);
            let mark = MARK_SIZE * if status == Status::Running { breath } else { 1.0 };
            let selected = active.as_deref() == Some(meta.id.as_str());
            let editing = renaming.as_ref().filter(|(r, _)| *r == meta.id).map(|(_, input)| input.clone());
            let label = div()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .whitespace_nowrap()
                .text_ellipsis()
                .text_sm()
                .text_color(if meta.archived { muted } else { fg })
                .child(meta.title.clone().unwrap_or_else(|| "New thread".into()));
            let menu =
                {
                    let (workspace, id, archived) = (workspace.clone(), meta.id.clone(), meta.archived);
                    move |menu: gpui_kit::component::menu::PopupMenu, _: &mut Window, _: &mut Context<_>| {
                        let (w1, w2, w3) = (workspace.clone(), workspace.clone(), workspace.clone());
                        let (i1, i2, i3) = (id.clone(), id.clone(), id.clone());
                        menu.item(
                            PopupMenuItem::new("Rename")
                                .on_click(move |_, window, cx| w1.update(cx, |w, cx| w.start_rename(&i1, window, cx))),
                        )
                        .item(PopupMenuItem::new(if archived { "Unarchive" } else { "Archive" }).on_click(
                            move |_, window, cx| w2.update(cx, |w, cx| w.set_archived(&i2, !archived, window, cx)),
                        ))
                        .separator()
                        .item(PopupMenuItem::new("Delete…").on_click(move |_, window, cx| {
                            w3.update(cx, |w, cx| w.confirm_delete_thread(&i3, window, cx))
                        }))
                    }
                };
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
                .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                    // A double click renames, as in a file manager.
                    if event.click_count() >= 2 {
                        this.start_rename(&id, window, cx);
                    } else {
                        this.open_thread(&id, window, cx);
                    }
                }))
                .map(|el| match editing {
                    Some(input) => el.child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .capture_action(cx.listener(|this, _: &gpui_kit::component::input::Escape, _, cx| {
                                this.finish_rename(false, cx);
                                cx.stop_propagation();
                            }))
                            .child(Input::new(&input).xsmall())
                            .child(crate::caret::overlay(&input)),
                    ),
                    None => el.child(label),
                })
                .when_some(dot, |el, color| el.child(div().size_2().rounded_full().bg(color)))
                .child(
                    div()
                        .id(SharedString::from(format!("thread-agent-{}", meta.id)))
                        .flex_none()
                        .size(px(MARK_SIZE))
                        .flex()
                        .items_center()
                        .justify_center()
                        .when(meta.archived, |el| el.opacity(0.6))
                        .tooltip({
                            let label = meta.agent.label();
                            move |window, cx| gpui_kit::component::tooltip::Tooltip::new(label).build(window, cx)
                        })
                        .child(
                            img(crate::agent_icon::icon(meta.agent, if meta.archived { muted } else { fg }))
                                .size(px(mark))
                                .flex_none()
                                .object_fit(ObjectFit::Contain),
                        ),
                )
                .context_menu(menu)
        };

        let mut projects = Vec::new();
        for project in &self.state.projects {
            let project_id = project.id.clone();
            let (shown, history) = store::split_threads(&project.threads, |t| self.pinned(t, cx));
            let history = history.len();
            let open: Vec<_> = shown.into_iter().map(|t| thread_row(t, cx)).collect();
            let archived: Vec<&store::ThreadMeta> = project.threads.iter().filter(|t| t.archived).collect();
            let showing_archived = self.show_archived.contains(&project.id);
            let archived_rows: Vec<_> =
                if showing_archived { archived.iter().map(|t| thread_row(t, cx)).collect() } else { Vec::new() };
            let image_choices = self.image_choices(&project.id, cx);
            let header_menu = {
                let (workspace, id) = (workspace.clone(), project.id.clone());
                move |menu: PopupMenu, _: &mut Window, _: &mut Context<_>| {
                    let (w1, w2) = (workspace.clone(), workspace.clone());
                    let (i1, i2) = (id.clone(), id.clone());
                    let menu = menu
                        .item(
                            PopupMenuItem::new("New thread")
                                .on_click(move |_, window, cx| w1.update(cx, |w, cx| w.new_thread(&i1, window, cx))),
                        )
                        .separator();
                    image_choices.add_to(menu).separator().item(PopupMenuItem::new("Remove from TJ…").on_click(
                        move |_, window, cx| w2.update(cx, |w, cx| w.confirm_remove_project(&i2, window, cx)),
                    ))
                }
            };
            let toggle_id = project.id.clone();
            let history_id = project.id.clone();
            projects.push(
                v_flex()
                    .pb_2()
                    .child(
                        h_flex()
                            .id(SharedString::from(format!("project-{}", project.id)))
                            .px_4()
                            .pt_2()
                            .pb_1()
                            .gap_2()
                            .child(self.project_avatar(project, 18., cx))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_ellipsis()
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
                                        cx.listener(move |this, _, window, cx| {
                                            this.new_thread(&project_id, window, cx)
                                        }),
                                    ),
                            )
                            .context_menu(header_menu),
                    )
                    .children(open)
                    .when(history > 0, |el| {
                        el.child(
                            h_flex()
                                .id(SharedString::from(format!("history-{}", project.id)))
                                .test_support()
                                .gap_1p5()
                                .mx_2()
                                .px_2()
                                .py_0p5()
                                .rounded_md()
                                .text_xs()
                                .text_color(muted)
                                .cursor_pointer()
                                .hover(|el| el.bg(accent_bg.opacity(0.6)))
                                .child(gpui_kit::component::Icon::new(IconName::Clock).xsmall())
                                .child(div().flex_1().child("History"))
                                .child(format!("{history} more"))
                                .on_click(
                                    cx.listener(move |this, _, window, cx| this.open_history(&history_id, window, cx)),
                                ),
                        )
                    })
                    .when(!archived.is_empty(), |el| {
                        el.child(
                            div()
                                .id(SharedString::from(format!("archived-{}", project.id)))
                                .test_support()
                                .mx_2()
                                .px_2()
                                .py_0p5()
                                .text_xs()
                                .text_color(muted)
                                .cursor_pointer()
                                .child(format!(
                                    "{} Archived ({})",
                                    if showing_archived { "▾" } else { "▸" },
                                    archived.len()
                                ))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    if !this.show_archived.remove(&toggle_id) {
                                        this.show_archived.insert(toggle_id.clone());
                                    }
                                    cx.notify();
                                })),
                        )
                    })
                    .children(archived_rows),
            );
        }

        v_flex()
            .w(px(SIDEBAR_WIDTH))
            .h_full()
            .flex_none()
            .bg(sidebar_bg)
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
        let (waiting, muted, fg) = (theme.warning, theme.muted_foreground, theme.foreground);
        let (tab, tab_fg, page) = (theme.tab, theme.tab_foreground, theme.background);
        let thread = self.active_thread().map(|t| t.read(cx));
        let title = thread.and_then(|t| t.title.clone()).unwrap_or_else(|| "New thread".into());
        // Work in progress shows in the sidebar and the composer; only a question for the user
        // shows up here.
        let status = match thread.map(|t| t.status) {
            Some(Status::AwaitingApproval) => "Waiting for you",
            _ => "",
        };
        let project_name = self.active_project().map(|p| p.name.clone());
        let avatar = |size: f32| {
            self.active_project().map(|p| {
                let choices = self.image_choices(&p.id, cx);
                div()
                    .id("header-project-image")
                    .flex_none()
                    .child(self.project_avatar(p, size, cx))
                    .context_menu(move |menu, _, _| choices.add_to(menu))
                    .into_any_element()
            })
        };
        let panes = self.active_project().and_then(|p| self.panes.get(&p.id)).cloned();
        let has_panes = panes.is_some();
        // One pane: its tabs are the top bar's. Split: each pane keeps its strip and the bar
        // names the project and thread.
        let tabs = panes.as_ref().and_then(|panes| panes.read(cx).header_tabs(panes.clone(), avatar(16.), cx));
        let agent = self.active_project().and_then(|p| self.agents.get(&p.id)).cloned();

        // Every action is its own round chip.
        let chip = |id: &'static str, icon: IconName, tip: &'static str| {
            div()
                .id(id)
                .test_support()
                .flex_none()
                .size(px(FLOAT_HEIGHT))
                // In the middle of the bar, on the same line as the tabs.
                .mb(px(FLOAT_INSET))
                .rounded_full()
                .flex()
                .items_center()
                .justify_center()
                .cursor_pointer()
                // Solid, like the tabs not on show.
                .bg(tab)
                .text_color(tab_fg)
                .hover(|el| el.bg(page).text_color(fg))
                .tooltip(move |window, cx| gpui_kit::component::tooltip::Tooltip::new(tip).build(window, cx))
                .child(Icon::new(icon).small())
        };
        let status_toggle = agent.filter(|a| a.read(cx).has_status(cx)).map(|agent| {
            let shown = agent.read(cx).status_shown(cx);
            chip(
                "header-status",
                if shown { IconName::PanelRightClose } else { IconName::PanelRightOpen },
                if shown { "Hide the thread's status" } else { "Show the thread's status" },
            )
            .on_click(move |_, _, cx| agent.update(cx, |a, cx| a.toggle_status(cx)))
        });
        let with_panes = |f: fn(&mut Panes, &mut Window, &mut Context<Panes>)| {
            cx.listener(move |this: &mut Self, _: &ClickEvent, window, cx| {
                if let Some(panes) = this.active_project().and_then(|p| this.panes.get(&p.id)).cloned() {
                    panes.update(cx, |p, cx| f(p, window, cx));
                }
            })
        };
        // Like a browser's tab-search button, at the far left: it hides or shows the sidebar.
        let collapsed = self.state.sidebar_collapsed;
        let sidebar_toggle = chip(
            "header-sidebar",
            if collapsed { IconName::PanelLeftOpen } else { IconName::PanelLeftClose },
            if collapsed { "Show the sidebar (Ctrl+Alt+S)" } else { "Hide the sidebar (Ctrl+Alt+S)" },
        )
        .on_click(cx.listener(|this, _, _, cx| this.toggle_sidebar(cx)));
        let new_terminal = has_panes.then(|| {
            chip("header-new-terminal", IconName::Plus, "New terminal tab (Ctrl+Shift+T)")
                .on_click(with_panes(|p, window, cx| p.new_terminal(window, cx)))
        });

        h_flex()
            .h(px(HEADER_HEIGHT))
            .flex_none()
            .items_end()
            // The same gap beside the floating things as above and below them, so the sidebar
            // button sits in the middle between the window's edge and the first tab.
            .gap(px(FLOAT_INSET))
            .px(px(FLOAT_INSET))
            .child(sidebar_toggle)
            .map(|el| match tabs {
                Some(tabs) => el.child(tabs).children(new_terminal),
                None => el
                    .child(
                        h_flex()
                            .h(px(FLOAT_HEIGHT))
                            .mb(px(FLOAT_INSET))
                            .min_w_0()
                            .gap_2()
                            .children(avatar(20.))
                            .when_some(project_name, |el, name| {
                                el.child(div().flex_none().text_sm().text_color(muted).child(name))
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
                            ),
                    )
                    .children(new_terminal),
            })
            .child(drag_area())
            .child(
                h_flex()
                    .h(px(FLOAT_HEIGHT))
                    .mb(px(FLOAT_INSET))
                    .flex_none()
                    .text_sm()
                    .text_color(waiting)
                    .child(status),
            )
            .when(has_panes, |el| {
                el.child(
                    chip("header-split-right", IconName::Columns2, "Split right (Ctrl+Shift+E)")
                        .on_click(with_panes(|p, window, cx| p.split(SplitDirection::Right, window, cx))),
                )
                .child(
                    chip("header-split-down", IconName::Rows2, "Split down (Ctrl+Shift+O)")
                        .on_click(with_panes(|p, window, cx| p.split(SplitDirection::Down, window, cx))),
                )
            })
            .children(status_toggle)
            .child(chip("close-window", IconName::X, "Close").on_click(cx.listener(|this, _, window, cx| {
                this.save_layouts(cx);
                window.remove_window();
            })))
    }
}

/// The picture items of a project's right-click menu.
#[derive(Clone)]
struct ImageChoices {
    workspace: Entity<Workspace>,
    project_id: String,
    /// It shows a picture now (not its initials).
    showing: bool,
    /// The user chose its picture (or chose initials).
    chosen: bool,
    /// An icon was found in the project.
    found: bool,
}

impl ImageChoices {
    fn add_to(&self, menu: PopupMenu) -> PopupMenu {
        let item = |label: &'static str, image: Option<Option<store::ProjectImage>>| {
            let (workspace, id) = (self.workspace.clone(), self.project_id.clone());
            PopupMenuItem::new(label).on_click(move |_, window, cx| {
                workspace.update(cx, |w, cx| match image.clone() {
                    None => w.choose_project_image(&id, window, cx),
                    Some(image) => {
                        w.set_project_image(&id, image);
                        cx.notify();
                    }
                })
            })
        };
        menu.item(item("Choose image…", None))
            .when(self.chosen && self.found, |menu| menu.item(item("Use the project's icon", Some(None))))
            .when(self.showing, |menu| menu.item(item("Use initials", Some(Some(store::ProjectImage::Initials)))))
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

/// `/home/me/Projects/TJ` -> `~/Projects/TJ`.
fn display_path(path: &std::path::Path, home: &std::path::Path) -> String {
    match path.strip_prefix(home) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".into(),
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}

/// The checked-out branch, read from `.git/HEAD` (a worktree's `.git` file points at its own
/// HEAD). A detached HEAD shows its short commit; outside git, `None`.
fn git_branch(project: &std::path::Path) -> Option<String> {
    let dot_git = project.join(".git");
    let head_file = if dot_git.is_file() {
        let pointer = std::fs::read_to_string(&dot_git).ok()?;
        let dir = pointer.trim().strip_prefix("gitdir:")?.trim();
        let dir = std::path::Path::new(dir);
        let dir = if dir.is_absolute() { dir.to_path_buf() } else { project.join(dir) };
        dir.join("HEAD")
    } else {
        dot_git.join("HEAD")
    };
    let head = std::fs::read_to_string(head_file).ok()?;
    let head = head.trim();
    match head.strip_prefix("ref: refs/heads/") {
        Some(branch) => Some(branch.to_string()),
        None => (head.len() >= 7).then(|| head[..7].to_string()),
    }
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

fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

/// TJ asks for client-side decorations, and gpui-kit sets a 20 px shadow inset for them. When
/// the compositor draws the frame instead (Hyprland does), that inset is never cleared, and gpui
/// still counts it when keeping overlays inside the window: a full-window dialog backdrop gets
/// pushed 20 px down, leaving a strip at the top undimmed. A server-decorated window has no
/// shadow, so its inset is zero (the platform already sizes the window that way).
fn clear_stale_client_inset(window: &mut Window) {
    if matches!(window.window_decorations(), Decorations::Server) && window.client_inset().is_some_and(|i| i > px(0.)) {
        window.set_client_inset(px(0.));
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        clear_stale_client_inset(window);
        crate::caret::begin_frame(cx);
        let panes = self.active_project().and_then(|p| self.panes.get(&p.id)).cloned();
        let theme = cx.theme();
        let (bg, fg, danger) = (theme.background, theme.foreground, theme.danger);
        // Around the content is the window's frame, like a browser's: the sidebar's colour,
        // see-through with it. The top bar is part of it, and the content below is the page
        // the shown tab opens into.
        let alpha = cx.try_global::<Settings>().map(Settings::sidebar_alpha).unwrap_or(1.0);
        let frame = theme.sidebar.opacity(alpha);

        // The window itself is transparent: only the content under the top bar paints an
        // opaque background, so the sidebar and the frame can be see-through.
        let sidebar = (!self.state.sidebar_collapsed).then(|| self.sidebar(window, cx).into_any_element());
        crate::caret::watch(h_flex())
            .key_context(CONTEXT)
            .on_action(cx.listener(|this, _: &ToggleSidebar, _, cx| this.toggle_sidebar(cx)))
            .size_full()
            .text_color(fg)
            .children(sidebar)
            .child(
                v_flex().flex_1().min_w_0().h_full().bg(frame).child(self.content_header(cx)).child(
                    v_flex()
                        .flex_1()
                        .min_h_0()
                        .min_w_0()
                        // Rounded at the top like a browser's page, so it sits in the frame. The
                        // views in it round their own backgrounds to match.
                        .rounded_t(px(crate::panes::CONTENT_RADIUS))
                        .overflow_hidden()
                        .bg(bg)
                        .when_some(self.problem.clone(), |el, problem| {
                            el.child(div().px_4().py_2().text_sm().text_color(danger).child(problem))
                        })
                        .child(div().flex_1().min_h_0().min_w_0().children(panes)),
                ),
            )
    }
}

/// Which idle Claude processes to stop, given each thread's id, how long its process has been
/// idle, and whether it's the thread on screen: any idle for [`IDLE_STOP`], then the longest
/// idle past the [`IDLE_KEEP`] used last. The thread on screen is only stopped by time.
fn processes_to_stop(mut idle: Vec<(String, Duration, bool)>) -> Vec<String> {
    idle.sort_by_key(|(_, for_, _)| *for_);
    let mut kept = 0;
    let mut stop = Vec::new();
    for (id, for_, shown) in idle {
        if for_ < IDLE_STOP && (shown || kept < IDLE_KEEP) {
            kept += 1;
        } else {
            stop.push(id);
        }
    }
    stop
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

    /// Two threads in the demo project: (project id, first thread, second thread, which is active).
    fn two_threads(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> (String, String, String) {
        let (project_id, first) =
            workspace.read_with(cx, |w, _| (w.state.projects[0].id.clone(), w.active.clone().unwrap()));
        workspace.update_in(cx, |w, window, cx| w.new_thread(&project_id, window, cx));
        let second = workspace.read_with(cx, |w, _| w.active.clone().unwrap());
        (project_id, first, second)
    }

    #[gpui_kit::test]
    fn cursor_settings_are_offered_and_saved(cx: &mut TestAppContext) {
        let (window, workspace, paths, _) = open(cx);
        let cx = &mut VisualTestContext::from_window(window, cx);
        cx.update(|window, cx| {
            window.render_frame(cx);
            window.click("settings-button", cx);
            window.render_frame(cx);
        });
        workspace.update(cx, |w, cx| w.update_settings(cx, |s| s.cursor_glide_ms = 0));
        assert_eq!(crate::settings::Settings::load(&paths).cursor_glide_ms, 0);
        cx.update(|window, cx| window.render_frame(cx));
    }

    /// The carets tell a key's move from a click's by what the window root reports.
    #[gpui_kit::test]
    fn the_caret_clock_sees_frames_keys_and_clicks(cx: &mut TestAppContext) {
        use crate::caret::Clock;
        use gpui_kit::{Modifiers, MouseButton, MouseDownEvent, MouseUpEvent};
        let (window, workspace, _, _) = open(cx);
        let cx = &mut VisualTestContext::from_window(window, cx);
        cx.update(|_, cx| crate::caret::init(cx));
        let clock =
            |cx: &mut VisualTestContext| cx.update(|_, cx| cx.try_global::<Clock>().copied().unwrap_or_default());
        cx.update(|window, cx| window.render_frame(cx));
        let first = clock(cx).frame;
        cx.update(|window, cx| window.render_frame(cx));
        assert_eq!(clock(cx).frame, first + 1, "each draw counts");

        let id = workspace.read_with(cx, |w, _| w.active.clone().unwrap());
        workspace.update_in(cx, |w, window, cx| w.start_rename(&id, window, cx));
        cx.update(|window, cx| window.render_frame(cx));
        for keys in ["a", "left"] {
            cx.update(|_, cx| cx.default_global::<Clock>().key = None);
            cx.simulate_keystrokes(keys);
            assert!(clock(cx).key.is_some(), "{keys} is seen");
        }

        let position = point(px(400.), px(300.));
        cx.simulate_event(MouseDownEvent {
            button: MouseButton::Left,
            position,
            modifiers: Modifiers::default(),
            click_count: 1,
            first_mouse: false,
        });
        assert!(clock(cx).pointer_down, "a press is seen");
        cx.simulate_event(MouseUpEvent {
            button: MouseButton::Left,
            position,
            modifiers: Modifiers::default(),
            click_count: 1,
        });
        assert!(!clock(cx).pointer_down && clock(cx).pointer.is_some(), "and its release");
    }

    #[gpui_kit::test]
    fn a_renamed_thread_keeps_its_name_until_cleared(cx: &mut TestAppContext) {
        let (window, workspace, paths, _) = open(cx);
        let cx = &mut VisualTestContext::from_window(window, cx);
        let id = workspace.read_with(cx, |w, _| w.active.clone().unwrap());
        workspace.update_in(cx, |w, window, cx| w.start_rename(&id, window, cx));
        cx.simulate_input("Release notes");
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        let saved = State::load(&paths).unwrap();
        let meta = &saved.projects[0].threads[0];
        assert_eq!((meta.title.as_deref(), meta.named), (Some("Release notes"), true));

        let thread = workspace.read_with(cx, |w, _| w.active_thread().unwrap());
        let overview = crate::overview::Overview { title: Some("Changelog work".into()), ..Default::default() };
        thread.update(cx, |t, cx| t.apply_overview(overview, 0, cx));
        assert_eq!(thread.read_with(cx, |t, _| t.title.clone()).as_deref(), Some("Release notes"));

        // Escape keeps the old name; an empty name hands naming back to the overview.
        workspace.update_in(cx, |w, window, cx| w.start_rename(&id, window, cx));
        cx.simulate_input("Typo");
        cx.update(|window, cx| window.render_frame(cx));
        cx.simulate_keystrokes("escape");
        assert_eq!(thread.read_with(cx, |t, _| t.title.clone()).as_deref(), Some("Release notes"));
        assert!(workspace.read_with(cx, |w, _| w.renaming.is_none()));
        workspace.update(cx, |w, cx| w.rename_thread(&id, "  ", cx));
        cx.run_until_parked();
        let meta = State::load(&paths).unwrap().projects[0].threads[0].clone();
        assert_eq!((meta.title.as_deref(), meta.named), (Some("Changelog work"), false));
    }

    #[gpui_kit::test]
    fn picking_threads_swaps_the_one_agent_tab_and_keeps_the_terminals(cx: &mut TestAppContext) {
        let (window, workspace, _, _) = open(cx);
        let cx = &mut VisualTestContext::from_window(window, cx);
        let (_, first, second) = two_threads(&workspace, cx);
        let panes = workspace.read_with(cx, |w, _| w.panes.values().next().cloned().unwrap());
        panes.update_in(cx, |p, window, cx| p.new_terminal(window, cx));
        assert_eq!(panes.read_with(cx, |p, _| p.shape()), vec![2], "the agent tab and a terminal");

        cx.update(|window, cx| {
            window.render_frame(cx);
            window.click(SharedString::from(format!("thread-{first}")), cx);
        });
        assert_eq!(panes.read_with(cx, |p, _| p.shape()), vec![2], "no tab per thread");
        assert_eq!(panes.read_with(cx, |p, cx| p.active_thread_id(cx)), Some(first.clone()));
        assert_eq!(workspace.read_with(cx, |w, _| w.active.clone()), Some(first));

        cx.update(|window, cx| {
            window.render_frame(cx);
            window.click(SharedString::from(format!("thread-{second}")), cx);
        });
        assert_eq!(panes.read_with(cx, |p, _| p.shape()), vec![2]);
        assert_eq!(panes.read_with(cx, |p, cx| p.active_thread_id(cx)), Some(second));
        let layout = panes.read_with(cx, |p, cx| p.snapshot(cx)).unwrap();
        let crate::store::PaneLayout::Pane { tabs, .. } = layout else { panic!("{layout:?}") };
        assert_eq!(tabs[0], crate::store::TabState::Agent, "saved without a thread id");
    }

    #[gpui_kit::test]
    fn the_status_column_sits_in_the_agent_tab_and_folds_when_narrow(cx: &mut TestAppContext) {
        let (window, workspace, _, _) = open(cx);
        let cx = &mut VisualTestContext::from_window(window, cx);
        let thread = workspace.read_with(cx, |w, _| w.active_thread().unwrap());
        thread.update(cx, |t, cx| {
            t.handle(crate::agent::AgentEvent::TextDelta("Started.".into()), cx);
            let overview = crate::overview::Overview { goal: Some("Ship it".into()), ..Default::default() };
            t.apply_overview(overview, 0, cx);
        });
        let settle = |cx: &mut VisualTestContext| {
            for _ in 0..3 {
                cx.update(|window, cx| window.render_frame(cx));
                cx.run_until_parked();
            }
        };
        let status_showing = |cx: &mut VisualTestContext| {
            cx.update(|window, cx| {
                window.render_frame(cx);
                window.try_find(SharedString::from("status")).is_some()
            })
        };
        settle(cx);
        assert!(status_showing(cx), "a wide agent tab shows the status column");

        // Split twice: the agent tab gets narrow and the column folds away.
        cx.update(|window, cx| {
            window.click(SharedString::from("header-split-right"), cx);
            window.render_frame(cx);
        });
        settle(cx);
        assert!(!status_showing(cx), "folded when narrow");

        // The top bar shows it again by hand.
        cx.update(|window, cx| window.click(SharedString::from("header-status"), cx));
        settle(cx);
        assert!(status_showing(cx), "shown by hand");
        cx.update(|window, cx| window.click(SharedString::from("header-status"), cx));
        settle(cx);
        assert!(!status_showing(cx), "and hidden again");
    }

    #[gpui_kit::test]
    fn archiving_hides_a_thread_until_unarchived(cx: &mut TestAppContext) {
        let (window, workspace, paths, _) = open(cx);
        let cx = &mut VisualTestContext::from_window(window, cx);
        let (project_id, first, second) = two_threads(&workspace, cx);
        workspace.update_in(cx, |w, window, cx| w.set_archived(&second, true, window, cx));
        let saved = State::load(&paths).unwrap();
        assert!(saved.projects[0].threads.iter().any(|t| t.id == second && t.archived));
        assert_eq!(workspace.read_with(cx, |w, _| w.active.clone()), Some(first.clone()), "its tab closed");

        cx.update(|window, cx| {
            window.render_frame(cx);
            window.click(SharedString::from(format!("archived-{project_id}")), cx);
        });
        assert!(workspace.read_with(cx, |w, _| w.show_archived.contains(&project_id)));
        workspace.update_in(cx, |w, window, cx| w.set_archived(&second, false, window, cx));
        assert!(!State::load(&paths).unwrap().projects[0].threads.iter().any(|t| t.archived));
    }

    #[test]
    fn idle_processes_past_the_time_or_the_limit_are_stopped() {
        use super::processes_to_stop;
        use std::time::Duration;
        let min = |m: u64| Duration::from_secs(m * 60);
        let idle = |list: &[(&str, u64, bool)]| {
            let mut stop =
                processes_to_stop(list.iter().map(|(id, m, shown)| (id.to_string(), min(*m), *shown)).collect());
            stop.sort();
            stop
        };
        assert!(idle(&[("a", 1, false), ("b", 2, false), ("c", 3, false)]).is_empty());
        assert_eq!(idle(&[("a", 1, false), ("b", 12, false)]), vec!["b"], "idle too long");
        assert_eq!(
            idle(&[("a", 1, false), ("b", 2, false), ("c", 3, false), ("d", 4, false), ("e", 5, false)]),
            vec!["d", "e"],
            "past the three used last"
        );
        // The thread on screen is kept past the limit, but not past the time.
        assert_eq!(idle(&[("a", 1, false), ("b", 2, false), ("c", 3, false), ("d", 4, true)]), Vec::<String>::new());
        assert_eq!(idle(&[("a", 1, false), ("d", 11, true)]), vec!["d"]);
    }

    #[gpui_kit::test]
    fn older_threads_are_opened_from_the_history(cx: &mut TestAppContext) {
        let (window, workspace, _, _) = open(cx);
        let cx = &mut VisualTestContext::from_window(window, cx);
        let (project_id, oldest) =
            workspace.read_with(cx, |w, _| (w.state.projects[0].id.clone(), w.active.clone().unwrap()));
        for _ in 0..6 {
            workspace.update_in(cx, |w, window, cx| w.new_thread(&project_id, window, cx));
        }
        let history = |cx: &mut VisualTestContext| {
            workspace.read_with(cx, |w, cx| w.history(&project_id, cx).into_iter().map(|t| t.id).collect::<Vec<_>>())
        };
        // Seven threads: the sidebar lists five, the history the two oldest.
        assert_eq!(history(cx).len(), 2);
        assert!(history(cx).contains(&oldest));

        cx.update(|window, cx| {
            window.render_frame(cx);
            window.click(SharedString::from(format!("history-{project_id}")), cx);
            window.render_frame(cx);
        });
        assert!(cx.update(gpui_kit::component::WindowExt::has_active_dialog));
        cx.update(|window, cx| {
            window.click(SharedString::from(format!("history-thread-{oldest}")), cx);
            window.render_frame(cx);
        });
        assert!(!cx.update(gpui_kit::component::WindowExt::has_active_dialog), "opening one closes the history");
        assert_eq!(workspace.read_with(cx, |w, _| w.active.clone()), Some(oldest.clone()));
        // While it's open the sidebar lists it, so the history no longer does.
        assert!(!history(cx).contains(&oldest));
        assert_eq!(history(cx).len(), 1);
    }

    #[gpui_kit::test]
    fn deleting_a_thread_removes_its_files_and_moves_on(cx: &mut TestAppContext) {
        let (window, workspace, paths, _) = open(cx);
        let cx = &mut VisualTestContext::from_window(window, cx);
        let (_, first, second) = two_threads(&workspace, cx);
        let transcript = paths.transcript(&second);
        std::fs::create_dir_all(crate::composer::files_dir(&transcript)).unwrap();
        std::fs::write(&transcript, "{}\n").unwrap();

        // The confirmation opens without trouble; the test then deletes directly.
        workspace.update_in(cx, |w, window, cx| w.confirm_delete_thread(&second, window, cx));
        cx.update(|window, cx| window.render_frame(cx));
        workspace.update_in(cx, |w, window, cx| w.delete_thread(&second, window, cx));

        let saved = State::load(&paths).unwrap();
        assert_eq!(saved.projects[0].threads.iter().map(|t| t.id.clone()).collect::<Vec<_>>(), vec![first.clone()]);
        assert!(!transcript.exists() && !crate::composer::files_dir(&transcript).exists());
        assert_eq!(workspace.read_with(cx, |w, _| w.active.clone()), Some(first));
        assert!(workspace.read_with(cx, |w, _| !w.threads.contains_key(&second)));
    }

    #[gpui_kit::test]
    fn removing_a_project_forgets_it_but_leaves_its_folder(cx: &mut TestAppContext) {
        let (window, workspace, paths, project) = open(cx);
        let cx = &mut VisualTestContext::from_window(window, cx);
        let (project_id, first, _) = two_threads(&workspace, cx);
        std::fs::create_dir_all(paths.transcript(&first).parent().unwrap()).unwrap();
        std::fs::write(paths.transcript(&first), "{}\n").unwrap();
        workspace.update_in(cx, |w, window, cx| w.confirm_remove_project(&project_id, window, cx));
        cx.update(|window, cx| window.render_frame(cx));
        workspace.update_in(cx, |w, window, cx| w.remove_project(&project_id, window, cx));

        assert!(State::load(&paths).unwrap().projects.is_empty());
        assert!(!paths.transcript(&first).exists());
        assert!(project.exists(), "the project's own folder is untouched");
        assert_eq!(workspace.read_with(cx, |w, _| (w.active.clone(), w.panes.len(), w.threads.len())), (None, 0, 0));
        cx.update(|window, cx| window.render_frame(cx));
    }

    #[gpui_kit::test]
    fn a_project_shows_its_icon_or_a_chosen_image_or_initials(cx: &mut TestAppContext) {
        use crate::store::ProjectImage;
        let (window, workspace, paths, project) = open(cx);
        let cx = &mut VisualTestContext::from_window(window, cx);
        let project_id = workspace.read_with(cx, |w, _| w.state.projects[0].id.clone());
        let picture = |workspace: &Entity<Workspace>, cx: &mut VisualTestContext| {
            workspace.read_with(cx, |w, _| w.project_picture(&w.state.projects[0]))
        };
        assert_eq!(picture(&workspace, cx), None, "no icon in the folder: initials");

        // An icon in the project is found in the background.
        std::fs::create_dir_all(project.join("assets")).unwrap();
        image::RgbaImage::new(256, 256).save(project.join("assets/logo.png")).unwrap();
        workspace.update(cx, |w, cx| {
            w.found_icons.clear();
            w.find_icons(cx);
        });
        cx.run_until_parked();
        assert_eq!(picture(&workspace, cx), Some(project.join("assets/logo.png")));

        // A chosen image is copied into TJ's folder and saved.
        let chosen = project.join("mine.png");
        image::RgbaImage::new(64, 64).save(&chosen).unwrap();
        workspace.update(cx, |w, _| w.use_image_file(&project_id, &chosen)).unwrap();
        let copy = picture(&workspace, cx).unwrap();
        assert!(copy.starts_with(paths.project_images_dir()) && copy.is_file());
        assert_eq!(State::load(&paths).unwrap().projects[0].image, Some(ProjectImage::File { path: copy.clone() }));
        assert!(workspace.update(cx, |w, _| w.use_image_file(&project_id, &project.join("nope.txt"))).is_err());
        cx.update(|window, cx| window.render_frame(cx));

        // Initials replace it (and its copy goes); resetting goes back to the project's icon.
        workspace.update(cx, |w, _| w.set_project_image(&project_id, Some(ProjectImage::Initials)));
        assert_eq!(picture(&workspace, cx), None);
        assert!(!copy.exists());
        workspace.update(cx, |w, _| w.set_project_image(&project_id, None));
        assert_eq!(picture(&workspace, cx), Some(project.join("assets/logo.png")));
        cx.update(|window, cx| window.render_frame(cx));

        // Removing the project deletes its copied image.
        workspace.update(cx, |w, _| w.use_image_file(&project_id, &chosen)).unwrap();
        let copy = picture(&workspace, cx).unwrap();
        workspace.update_in(cx, |w, window, cx| w.remove_project(&project_id, window, cx));
        assert!(!copy.exists());
        assert!(chosen.exists(), "the original is never touched");
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

        // While the thread is on its intro, the overview pane (and its file list) stays hidden.
        cx.update(|window, cx| {
            window.render_frame(cx);
            assert!(window.try_find(SharedString::from("change-0")).is_none());
        });
        let thread = workspace.read_with(cx, |w, _| w.active_thread().unwrap());
        thread.update(cx, |t, cx| t.handle(crate::agent::AgentEvent::TextDelta("Started.".into()), cx));

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
        assert!(panes.read_with(cx, |p, cx| p.active_thread_id(cx).is_some()));
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
    fn the_top_bar_holds_a_single_panes_tabs_and_splits_hand_them_back(cx: &mut TestAppContext) {
        let (window, workspace, _, _) = open(cx);
        let cx = &mut VisualTestContext::from_window(window, cx);
        let panes = workspace.read_with(cx, |w, _| w.panes.values().next().cloned().unwrap());
        assert_eq!(panes.read_with(cx, |p, _| p.shape()), vec![1], "a new project starts with just its conversation");
        cx.update(|window, cx| {
            window.render_frame(cx);
            assert!(window.try_find(SharedString::from("header-tab-0")).is_some(), "the agent is a tab in the top bar");
            window.click(SharedString::from("header-new-terminal"), cx);
            window.render_frame(cx);
            assert!(window.try_find(SharedString::from("header-tab-1")).is_some(), "a terminal joins it there");
            assert!(window.try_find(SharedString::from("tab-1-1")).is_none(), "no strip over a single pane");
            window.click(SharedString::from("header-split-right"), cx);
            window.render_frame(cx);
            assert!(window.try_find(SharedString::from("header-tab-0")).is_none(), "split panes keep their own tabs");
            assert!(window.try_find(SharedString::from("tab-1-1")).is_some(), "so the strip comes back");
        });
        assert_eq!(panes.read_with(cx, |p, _| p.shape()), vec![2, 1]);
    }

    #[gpui_kit::test]
    fn the_sidebar_folds_away_from_the_top_bar_or_ctrl_alt_s_and_stays_so(cx: &mut TestAppContext) {
        let (window, workspace, paths, _) = open(cx);
        let cx = &mut VisualTestContext::from_window(window, cx);
        let collapsed = |cx: &mut VisualTestContext| workspace.read_with(cx, |w, _| w.state.sidebar_collapsed);
        assert!(!collapsed(cx), "the sidebar starts out shown");
        cx.update(|window, cx| {
            window.render_frame(cx);
            window.click(SharedString::from("header-sidebar"), cx);
        });
        assert!(collapsed(cx), "the top bar's button hides it");
        assert!(State::load(&paths).unwrap().sidebar_collapsed, "and that's saved");
        cx.simulate_keystrokes("ctrl-alt-s");
        assert!(!collapsed(cx), "Ctrl+Alt+S brings it back");
        cx.simulate_keystrokes("ctrl-alt-s");
        assert!(collapsed(cx), "and hides it again");
    }

    #[test]
    fn branch_and_display_path() {
        let root = std::env::temp_dir().join(format!("tj-branch-{}", new_id()));
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(root.join(".git/HEAD"), "ref: refs/heads/feat/intro\n").unwrap();
        assert_eq!(super::git_branch(&root).as_deref(), Some("feat/intro"));
        std::fs::write(root.join(".git/HEAD"), "0123456789abcdef\n").unwrap();
        assert_eq!(super::git_branch(&root).as_deref(), Some("0123456"), "detached shows the short commit");

        let worktree = root.join("wt");
        std::fs::create_dir_all(root.join(".git/worktrees/wt")).unwrap();
        std::fs::write(root.join(".git/worktrees/wt/HEAD"), "ref: refs/heads/side\n").unwrap();
        std::fs::create_dir_all(&worktree).unwrap();
        std::fs::write(worktree.join(".git"), format!("gitdir: {}\n", root.join(".git/worktrees/wt").display()))
            .unwrap();
        assert_eq!(super::git_branch(&worktree).as_deref(), Some("side"));
        assert_eq!(super::git_branch(&std::env::temp_dir().join("tj-no-git-here")), None);

        let home = std::path::Path::new("/home/me");
        assert_eq!(super::display_path(std::path::Path::new("/home/me/Projects/TJ"), home), "~/Projects/TJ");
        assert_eq!(super::display_path(home, home), "~");
        assert_eq!(super::display_path(std::path::Path::new("/srv/x"), home), "/srv/x");
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
    fn a_model_made_the_project_default_starts_new_threads(cx: &mut TestAppContext) {
        let (window, workspace, paths, _) = open(cx);
        let cx = &mut VisualTestContext::from_window(window, cx);
        let thread = workspace.read_with(cx, |w, _| w.active_thread().unwrap());
        thread.update(cx, |t, cx| {
            t.set_model_choice(Some("sonnet".into()), cx);
            t.set_effort(Some("low".into()), cx);
        });
        thread.update(cx, |_, cx| cx.emit(crate::thread::ThreadAction::MakeModelDefault));
        cx.run_until_parked();
        let saved = State::load(&paths).unwrap();
        assert_eq!(saved.projects[0].model.as_deref(), Some("sonnet"));
        assert_eq!(saved.projects[0].effort.as_deref(), Some("low"));
        assert_eq!(saved.projects[0].threads[0].model, None, "matching the project, the thread keeps no override");

        let project_id = saved.projects[0].id.clone();
        workspace.update_in(cx, |w, window, cx| w.new_thread(&project_id, window, cx));
        let (model, effort) = workspace.read_with(cx, |w, cx| {
            let t = w.active_thread().unwrap().read(cx);
            (t.model_choice.clone(), t.effort.clone())
        });
        assert_eq!((model.as_deref(), effort.as_deref()), (Some("sonnet"), Some("low")));
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
