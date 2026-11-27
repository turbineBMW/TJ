//! The window: a full-height sidebar of projects and threads, and the project's panes, where
//! the agent (the picked thread with its status column), terminals, the review and the
//! project's GitHub page are tabs.
//! Laid out like a GNOME app: no title bar; the headers move it.

use std::{collections::HashMap, path::PathBuf, time::Duration};

use gpui_kit::base::{h_flex, v_flex};
use gpui_kit::component::{
    ActiveTheme as _, Icon, Sizable as _, WindowExt as _,
    button::{Button, ButtonVariant, ButtonVariants as _},
    input::{Input, InputEvent, InputState},
    menu::{ContextMenuExt as _, PopupMenu, PopupMenuItem},
};
use gpui_kit::{TestSupportExt as _, prelude::FluentBuilder as _, *};

use crate::agent::{AgentEvent, claude::SessionOptions};
use crate::agent_slot::{AgentSlot, AgentSlotEvent};
use crate::attention::{self, Attention, AttentionKind, Reminder};
use crate::changes::{ProjectChanges, ReviewView, Summarizer};
use crate::github_view::{GitHubEvent, GitHubView};
use crate::notes::NotesStore;
use crate::notes_view::{NotesEvent, NotesView};
use crate::panes::{FLOAT_HEIGHT, FLOAT_INSET, PaneEvent, Panes, Start};
use crate::settings::Settings;
use crate::store::SplitDirection;
use crate::store::{self, Accounts, Paths, State};
use crate::thread::{
    self, Backend, MenuContext, Status, ThreadAction, ThreadChanged, ThreadNeedsYou, ThreadSent, ThreadSetup,
    ThreadView,
};
use gpui_kit::assets::IconName;

/// How wide the sidebar is. The status column on the other side matches it.
pub const SIDEBAR_WIDTH: f32 = 302.0;
/// The status column folds away when it would leave the content narrower than this, unless it
/// was shown by hand.
const STATUS_FOLD_WIDTH: f32 = 640.0;
const HEADER_HEIGHT: f32 = 46.0;

// The top bar's floating things sit in its middle.
const _: () = assert!(HEADER_HEIGHT == FLOAT_HEIGHT + 2.0 * FLOAT_INSET);

pub const CONTEXT: &str = "Workspace";

// Window-wide commands. Their keys are in `keybindings::COMMANDS`, and settings can change them.
gpui_kit::actions!(
    workspace,
    [
        ToggleSidebar,
        NextProject,
        PreviousProject,
        NewThread,
        OpenSwitcher,
        OpenSettings,
        OpenGitHub,
        OpenNotes,
        FocusAgent
    ]
);
/// The agent mark at the end of a thread's row in the sidebar.
const MARK_SIZE: f32 = 14.0;
/// An idle thread's Claude process (about 300 MB) is stopped after this long; the next
/// message resumes the session in a new one.
const IDLE_STOP: Duration = Duration::from_secs(10 * 60);
/// At most this many idle Claude processes are kept, the most recently used.
const IDLE_KEEP: usize = 3;
/// How often reminders picked on a notification are checked for being due.
const REMINDER_CHECK: Duration = Duration::from_secs(15);

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
    /// Open the project's GitHub tab at launch (test hook).
    pub github: bool,
    /// Open the project's notes at launch (test hook).
    pub notes: bool,
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
    /// Per project: its GitHub page, shown in a tab.
    githubs: HashMap<String, Entity<GitHubView>>,
    /// Every project's notes, kept in step with their files.
    notes: Entity<NotesStore>,
    /// Per project: its notes, shown in a tab.
    notes_views: HashMap<String, Entity<NotesView>>,
    /// Per pull request ("<project id>#<number>"): its files with their summaries, for the
    /// review modal, kept so summaries survive closing it.
    pr_changes: HashMap<String, Entity<ProjectChanges>>,
    /// The thread the header follows (the one last shown or focused).
    active: Option<String>,
    /// The status column shown or hidden by hand; `None` follows the window's width.
    status_choice: Option<bool>,
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
    /// Per account and project folder: the slash commands `claude` accepts there (loaded once,
    /// in the background; a project's own commands and skills are part of it).
    commands: HashMap<(String, PathBuf), Vec<crate::agent::SlashCommand>>,
    commands_loading: std::collections::HashSet<(String, PathBuf)>,
    no_summaries: bool,
    /// The thread whose title is being edited in the sidebar, and the editor.
    renaming: Option<(String, Entity<InputState>)>,
    /// Projects whose archived threads are listed.
    show_archived: std::collections::HashSet<String>,
    /// Per project: the icon found in its folder, once looked for (`None` inside: there isn't one).
    found_icons: HashMap<String, Option<PathBuf>>,
    /// The settings modal, while it's open.
    settings_view: Option<WeakEntity<crate::settings_view::SettingsView>>,
    /// The window has the keyboard, so a thread on screen is being looked at.
    window_active: bool,
    _subscriptions: Vec<Subscription>,
    /// Checks every minute for Claude processes idle long enough to stop.
    _idle_check: Option<Task<()>>,
    /// Shows reminders picked on notifications when they're due.
    _reminder_check: Option<Task<()>>,
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
        // The last usage and context windows heard, so the status column has numbers at once.
        crate::usage::Usage::load(cx, paths.usage_file());

        let notes = cx.new(|cx| NotesStore::new(&paths.data_dir, cx));
        // The sidebar's note counts follow the files.
        let notes_observer = cx.observe(&notes, |_, _, cx| cx.notify());
        let mut this = Self {
            paths,
            home,
            accounts,
            state,
            threads: HashMap::new(),
            panes: HashMap::new(),
            agents: HashMap::new(),
            reviews: HashMap::new(),
            githubs: HashMap::new(),
            notes,
            notes_views: HashMap::new(),
            pr_changes: HashMap::new(),
            active: None,
            status_choice: None,
            shell: launch.shell,
            model: launch.model,
            scripted: launch.scripted,
            type_text: launch.type_text,
            problem,
            changes: HashMap::new(),
            models: HashMap::new(),
            models_loading: Default::default(),
            commands: Default::default(),
            commands_loading: Default::default(),
            renaming: None,
            show_archived: Default::default(),
            found_icons: HashMap::new(),
            no_summaries: launch.no_summaries,
            settings_view: None,
            window_active: window.is_window_active(),
            _subscriptions: vec![notes_observer],
            _idle_check: None,
            _reminder_check: None,
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
        if let Some(id) = &opened_project {
            this.bring_back_notes(id, cx);
        }

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
        if launch.github {
            this.open_github(window, cx);
        }
        if launch.notes {
            this.open_notes(window, cx);
        }
        this.find_icons(cx);
        this.watch_attention(window, cx);
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

    /// The active project's agent, which builds the status column.
    fn active_agent(&self) -> Option<Entity<AgentSlot>> {
        self.active_project().and_then(|p| self.agents.get(&p.id)).cloned()
    }

    /// Whether the status column has something to show for the active thread.
    fn has_status(&self, cx: &App) -> bool {
        self.active_agent().is_some_and(|a| a.read(cx).has_status())
    }

    /// Whether the status column is on screen: by hand, else while the content keeps
    /// [`STATUS_FOLD_WIDTH`] beside it.
    fn status_shown(&self, window: &Window, cx: &App) -> bool {
        let sidebar = if self.state.sidebar_collapsed { 0. } else { SIDEBAR_WIDTH };
        let room = window.viewport_size().width - px(sidebar + crate::agent_slot::STATUS_WIDTH);
        self.has_status(cx) && self.status_choice.unwrap_or(room >= px(STATUS_FOLD_WIDTH))
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
            let project_path = project.path.clone();
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
                    eighty_twenty: cx.try_global::<crate::settings::Settings>().is_none_or(|s| s.eighty_twenty),
                    notes: std::env::current_exe().ok().map(|binary| {
                        Box::new(crate::agent::claude::NotesServer {
                            binary,
                            context: crate::notes_mcp::Context {
                                file: crate::notes::notes_file(&self.paths.data_dir, &project.id),
                                project_dir: project.path.clone(),
                                project_name: project.name.clone(),
                                thread: Some(meta.id.clone()),
                                transcript: Some(self.paths.transcript(&meta.id)),
                            },
                        })
                    }),
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
            self._subscriptions
                .push(cx.subscribe(&thread, move |this, _, event: &ThreadNeedsYou, cx| this.needs_you(&id, event, cx)));
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
                        ThreadAction::RunInTerminal(command) => this.run_in_terminal(command, window, cx),
                    }
                },
            ));
            self.load_models(&account.id, cx);
            self.load_commands(&account.id, &project_path, cx);
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

    /// Ask `claude` once, in the background, which slash commands a project offers on an account.
    fn load_commands(&mut self, account_id: &str, project: &std::path::Path, cx: &mut Context<Self>) {
        let key = (account_id.to_string(), project.to_path_buf());
        if self.no_summaries || self.commands.contains_key(&key) || !self.commands_loading.insert(key.clone()) {
            return;
        }
        let config_dir = self.accounts.get(account_id).and_then(|a| a.config_path(&self.home));
        cx.spawn(async move |this, cx| {
            let dir = key.1.clone();
            let commands = cx
                .background_executor()
                .spawn(async move { crate::agent::claude::list_commands(config_dir.as_deref(), &dir) })
                .await;
            let _ = this.update(cx, |this: &mut Workspace, cx| {
                this.commands_loading.remove(&key);
                match commands {
                    Ok(commands) => {
                        this.commands.insert(key, commands);
                        this.refresh_menus(cx);
                    }
                    // A running session lists them too, so the picker fills in after the first message.
                    Err(err) => eprintln!("tj: could not list slash commands: {err:#}"),
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
                commands: self
                    .commands
                    .get(&(thread.read(cx).account_id.clone(), project.path.clone()))
                    .cloned()
                    .unwrap_or_default(),
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
        self.mark_seen(thread_id);
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

    // ---------- notifications ----------

    /// Follow the window's focus, answer clicks on notifications, and show reminders when due.
    fn watch_attention(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self._subscriptions.push(cx.observe_window_activation(window, |this, window, cx| {
            this.window_active = window.is_window_active();
            this.mark_shown_seen(cx);
        }));
        let (workspace, handle) = (cx.entity().downgrade(), window.window_handle());
        cx.on_system_notification_response(move |response, cx| {
            let _ = handle.update(cx, |_, window, cx| {
                workspace.update(cx, |w, cx| w.notification_response(response, window, cx))
            });
        });
        self._reminder_check = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(REMINDER_CHECK).await;
                if this.update(cx, |w: &mut Workspace, cx| w.show_due_reminders(cx)).is_err() {
                    break;
                }
            }
        }));
    }

    /// Whether the user is looking at the thread: the window has the keyboard and the thread
    /// is in its project's agent, in the tab on show.
    fn on_screen(&self, thread_id: &str, cx: &App) -> bool {
        self.window_active
            && self.active.as_deref() == Some(thread_id)
            && self.active_project().and_then(|p| self.panes.get(&p.id)).is_some_and(|p| p.read(cx).agent_on_show())
    }

    /// A thread asked something or finished. Unless the user is looking at it, mark it in the
    /// sidebar and say so in a notification.
    fn needs_you(&mut self, thread_id: &str, event: &ThreadNeedsYou, cx: &mut Context<Self>) {
        if self.on_screen(thread_id, cx) {
            return;
        }
        let Some(meta) = self.state.thread_mut(thread_id) else { return };
        // Approvals often come several at once, and a failed turn is often followed by its
        // process exiting: one notification until the thread is seen.
        let again = meta.unseen.as_ref().is_some_and(|a| {
            a.kind == event.kind && a.kind != AttentionKind::Finished || a.kind.is_waiting() && event.kind.is_waiting()
        });
        meta.unseen = Some(Attention { kind: event.kind, detail: event.detail.clone(), at: store::now() });
        if !again {
            // A reminder about what it wanted before is out of date now.
            self.state.reminders.retain(|r| r.thread_id != thread_id);
            self.show_notification(thread_id, cx);
        }
        self.save();
        cx.notify();
    }

    /// The notification for what the thread wants, if it still hasn't been seen.
    fn show_notification(&self, thread_id: &str, cx: &App) {
        let Some((project, meta)) = self.state.thread(thread_id) else { return };
        let Some(attention) = &meta.unseen else { return };
        let title = meta.title.as_deref().unwrap_or("New thread");
        cx.show_system_notification(attention::notification(
            thread_id,
            meta.agent.label(),
            &project.name,
            title,
            attention,
        ));
    }

    /// A click on a notification opens its thread; a button brings the notification back later.
    fn notification_response(
        &mut self,
        response: SystemNotificationResponse,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let thread_id = response.tag.to_string();
        if self.state.thread(&thread_id).is_none() {
            return;
        }
        match response.action_id {
            None => {
                self.open_thread(&thread_id, window, cx);
                window.activate_window();
                attention::focus_on_hyprland();
            }
            Some(action) => {
                let Some(due) = attention::remind_at(&action, store::now()) else { return };
                self.state.reminders.retain(|r| r.thread_id != thread_id);
                self.state.reminders.push(Reminder { thread_id, due });
                self.save();
            }
        }
    }

    /// Show the notifications whose reminders are due, for threads still not looked at.
    /// Reminders due while TJ was closed show once it's open again.
    fn show_due_reminders(&mut self, cx: &mut Context<Self>) {
        let now = store::now();
        if !self.state.reminders.iter().any(|r| r.due <= now) {
            return;
        }
        let (due, later) = std::mem::take(&mut self.state.reminders).into_iter().partition(|r| r.due <= now);
        self.state.reminders = later;
        for reminder in due {
            self.show_notification(&reminder.thread_id, cx);
        }
        self.save();
    }

    /// The thread has been looked at: take its mark off, and drop its reminders.
    fn mark_seen(&mut self, thread_id: &str) {
        let reminders = self.state.reminders.len();
        self.state.reminders.retain(|r| r.thread_id != thread_id);
        let marked = self.state.thread_mut(thread_id).and_then(|meta| meta.unseen.take()).is_some();
        if marked || reminders != self.state.reminders.len() {
            self.save();
        }
    }

    /// Mark the thread on screen seen, if the user is looking at it.
    fn mark_shown_seen(&mut self, cx: &mut Context<Self>) {
        if let Some(id) = self.active.clone().filter(|id| self.on_screen(id, cx)) {
            self.mark_seen(&id);
            cx.notify();
        }
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
        self.githubs.remove(project_id);
        // Its notes stay on disk, for the notes tab's cleanup page or adding the folder again.
        self.notes_views.remove(project_id);
        self.pr_changes.retain(|key, _| !key.starts_with(&format!("{project_id}#")));
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
    /// one working or waiting for the user, and one not looked at since it asked something or
    /// finished, so none of those is lost in the history.
    fn pinned(&self, meta: &store::ThreadMeta, cx: &App) -> bool {
        self.active.as_deref() == Some(meta.id.as_str())
            || self.renaming.as_ref().is_some_and(|(id, _)| *id == meta.id)
            || self.threads.get(&meta.id).is_some_and(|t| t.read(cx).status != Status::Idle)
            || meta.unseen.is_some()
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

    pub(crate) fn new_thread(&mut self, project_id: &str, window: &mut Window, cx: &mut Context<Self>) {
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
                this.bring_back_notes(&project, cx);
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
    pub(crate) fn project_avatar(&self, project: &store::Project, size: f32, cx: &App) -> AnyElement {
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
        if let Some(path) = self.active_project().map(|p| p.path.clone()) {
            self.load_commands(&account.id, &path, cx);
        }
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

    /// Open a terminal tab in the thread's project with `command` on its command line, not run:
    /// the user reads it, edits it if need be, and presses Enter.
    fn run_in_terminal(&mut self, command: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(project_id) = self.active_project().map(|p| p.id.clone()) else { return };
        let Some(panes) = self.project_panes(&project_id, None, window, cx) else { return };
        let Some(term) = panes.update(cx, |p, cx| p.new_project_terminal(window, cx)) else { return };
        crate::terminal::TerminalView::insert_command(&term, command.to_string(), cx);
        let focus = term.read(cx).focus_handle(cx);
        window.focus(&focus, cx);
        cx.notify();
    }

    /// Change a setting, save it, and re-apply the theme if following it was switched.
    pub(crate) fn update_settings(&mut self, cx: &mut Context<Self>, change: impl FnOnce(&mut Settings)) {
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
        let fonts_changed = (&after.ui_font, &after.mono_font) != (&before.ui_font, &before.mono_font);
        if after.follow_omarchy_theme != before.follow_omarchy_theme || fonts_changed {
            crate::omarchy::apply(cx, after.follow_omarchy_theme);
        }
        if after.keybindings != before.keybindings {
            crate::keybindings::apply(cx);
        }
        cx.notify();
    }

    // ---------- settings and the switcher ----------

    pub(crate) fn paths(&self) -> &Paths {
        &self.paths
    }

    pub(crate) fn home(&self) -> &std::path::Path {
        &self.home
    }

    pub(crate) fn accounts(&self) -> &Accounts {
        &self.accounts
    }

    /// Make `account_id` the account threads use when neither they nor their project name one.
    /// Threads already open keep the account they started on.
    pub(crate) fn set_default_account(&mut self, account_id: &str, cx: &mut Context<Self>) {
        if self.accounts.default == account_id || self.accounts.get(account_id).is_none() {
            return;
        }
        self.accounts.default = account_id.to_string();
        if let Err(err) = self.accounts.save_default(&self.paths) {
            self.problem = Some(format!("Could not save the default account: {err:#}").into());
        }
        self.refresh_menus(cx);
        cx.notify();
    }

    fn open_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let workspace = cx.entity().downgrade();
        let view = cx.new(|cx| crate::settings_view::SettingsView::new(workspace, window, cx));
        self.settings_view = Some(view.downgrade());
        let shown = view.clone();
        window.open_dialog(cx, move |dialog, _, _| dialog.w(px(720.)).title("Settings").child(shown.clone()));
        view.update(cx, |v, cx| v.focus(window, cx));
    }

    fn open_switcher(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let workspace = cx.entity().downgrade();
        let view = cx.new(|cx| crate::switcher::SwitcherView::new(workspace, window, cx));
        let shown = view.clone();
        window.open_dialog(cx, move |dialog, _, _| {
            dialog.w(px(620.)).margin_top(px(90.)).close_button(false).child(shown.clone())
        });
        view.update(cx, |v, cx| v.focus(window, cx));
    }

    /// Every project, each followed by its threads (newest first, archived ones last), for the
    /// switcher.
    pub(crate) fn switcher_entries(&self) -> Vec<crate::switcher::Entry> {
        use crate::switcher::{Entry, EntryKind};
        let mut entries = Vec::new();
        for project in &self.state.projects {
            entries.push(Entry {
                kind: EntryKind::Project,
                id: project.id.clone(),
                project_id: project.id.clone(),
                title: project.name.clone(),
                detail: display_path(&project.path, &self.home),
                project_name: project.name.clone(),
                updated_at: 0,
                agent: Default::default(),
                archived: false,
            });
            let mut threads: Vec<&store::ThreadMeta> = project.threads.iter().collect();
            threads.sort_by_key(|t| (t.archived, std::cmp::Reverse(t.updated_at)));
            entries.extend(threads.into_iter().map(|t| Entry {
                kind: EntryKind::Thread,
                id: t.id.clone(),
                project_id: project.id.clone(),
                title: t.title.clone().unwrap_or_else(|| "New thread".into()),
                detail: String::new(),
                project_name: project.name.clone(),
                updated_at: t.updated_at,
                agent: t.agent,
                archived: t.archived,
            }));
        }
        entries
    }

    pub(crate) fn project_by_id(&self, project_id: &str) -> Option<&store::Project> {
        self.state.project(project_id)
    }

    /// The project on show, by id.
    pub(crate) fn active_project_id(&self) -> Option<String> {
        self.active_project().map(|p| p.id.clone())
    }

    // ---------- moving between projects ----------

    /// Show the project as it was left: the thread its agent tab last showed, with its panes
    /// and tabs untouched. A project not opened yet shows its newest thread, or a new one.
    pub(crate) fn open_project(&mut self, project_id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(project) = self.state.project(project_id) else { return };
        let last_shown = self
            .agents
            .get(project_id)
            .and_then(|a| a.read(cx).thread_id().map(str::to_string))
            .filter(|id| self.state.thread(id).is_some());
        if let (Some(thread_id), Some(panes)) = (last_shown, self.panes.get(project_id).cloned()) {
            self.active = Some(thread_id);
            self.state.selected_thread = self.active.clone();
            self.save();
            panes.update(cx, |p, cx| p.focus_active(window, cx));
            // Coming back to it by keyboard is looking at it, as a click on its row is.
            self.mark_shown_seen(cx);
            cx.notify();
            return;
        }
        match project.threads.iter().find(|t| !t.archived).map(|t| t.id.clone()) {
            Some(thread_id) => self.open_thread(&thread_id, window, cx),
            None => self.new_thread(project_id, window, cx),
        }
    }

    /// Show the next (`step` 1) or previous (-1) project in the sidebar's order, wrapping round.
    fn step_project(&mut self, step: isize, window: &mut Window, cx: &mut Context<Self>) {
        let count = self.state.projects.len() as isize;
        if count == 0 {
            return;
        }
        let current = self
            .active_project()
            .and_then(|p| self.state.projects.iter().position(|q| q.id == p.id))
            .map(|ix| ix as isize);
        let next = match current {
            Some(ix) => (ix + step).rem_euclid(count),
            None => 0,
        };
        let id = self.state.projects[next as usize].id.clone();
        self.open_project(&id, window, cx);
    }

    /// A new thread in the project on show: its start screen, with the prompt box focused.
    fn new_thread_here(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let project =
            self.active_project().map(|p| p.id.clone()).or_else(|| self.state.projects.first().map(|p| p.id.clone()));
        if let Some(project) = project {
            self.new_thread(&project, window, cx);
        }
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

    /// Show the review tab with summaries being written: just `path`, if given, or every file.
    fn open_review(&mut self, path: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(changes) = self.project_changes(cx) else { return };
        if self.project_review(cx).is_none() {
            return;
        }
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
            // Focus first, so turning summaries on doesn't start on every file.
            c.set_focus(path, cx);
            c.set_auto_summarize(Some((summarizer, requests)), cx);
            c.refresh(cx);
        });
        let Some(project_id) = self.active_project().map(|p| p.id.clone()) else { return };
        if let Some(panes) = self.project_panes(&project_id, None, window, cx) {
            panes.update(cx, |p, cx| p.show_review(window, cx));
        }
        cx.notify();
    }

    /// A pull request's file in the review modal: its diff and summary, with Previous and Next
    /// file.
    fn open_pr_file(
        &mut self,
        project_id: &str,
        root: std::path::PathBuf,
        file: crate::github_view::ExpandFile,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = format!("{project_id}#{}", file.number);
        let changes = match self.pr_changes.get(&key) {
            Some(changes) => {
                changes.update(cx, |c, cx| c.set_patch(&file.patch, cx));
                changes.clone()
            }
            None => {
                let changes = cx.new(|cx| ProjectChanges::from_fixed_patch(root, &file.patch, cx));
                self.pr_changes.insert(key, changes.clone());
                changes
            }
        };
        let summarizer = if self.no_summaries {
            Summarizer::Off
        } else {
            let config_dir = self
                .active_thread()
                .and_then(|t| self.accounts.get(&t.read(cx).account_id).and_then(|a| a.config_path(&self.home)));
            Summarizer::Claude { config_dir }
        };
        changes.update(cx, |c, cx| {
            // Focus first, so only this file is summarized.
            c.set_focus(Some(file.path.clone()), cx);
            c.set_auto_summarize(Some((summarizer, vec![file.context.clone()])), cx);
        });
        let view = cx.new(|cx| ReviewView::with_navigator(changes, self.no_summaries, cx));
        let shown = view.clone();
        window.open_dialog(cx, move |dialog, window, cx| {
            use gpui_kit::component::ActiveTheme as _;
            let size = window.viewport_size();
            // As tall as the window allows: the same gap above as the dialog keeps below.
            let gap = cx.theme().spacing_tokens().lg;
            dialog
                .w((size.width * 0.9).min(px(1600.)))
                .margin_top(gap)
                .h(size.height - gap * 2.)
                .title(format!("Pull request #{}", file.number))
                .child(div().size_full().rounded_md().overflow_hidden().child(shown.clone()))
        });
        view.update(cx, |v, cx| v.focus(window, cx));
    }

    /// Bring the project's agent to the front from wherever focus is (a terminal, the GitHub
    /// tab, another split) and put the caret in its prompt box. A closed agent opens again in
    /// a tab of its own; a project with no thread to show starts one.
    fn focus_agent(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(project_id) = self.active_project_id() else { return };
        let shown = self
            .agents
            .get(&project_id)
            .and_then(|a| a.read(cx).thread_id().map(str::to_string))
            .or_else(|| self.active.clone())
            .filter(|id| self.state.thread(id).is_some());
        match shown {
            Some(thread_id) => self.open_thread(&thread_id, window, cx),
            None => self.new_thread(&project_id, window, cx),
        }
    }

    /// Show the project's GitHub tab.
    fn open_github(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(project_id) = self.active_project().map(|p| p.id.clone()) else { return };
        if let Some(panes) = self.project_panes(&project_id, None, window, cx) {
            panes.update(cx, |p, cx| p.show_github(window, cx));
        }
        cx.notify();
    }

    /// Show the project's notes tab.
    fn open_notes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(project_id) = self.active_project().map(|p| p.id.clone()) else { return };
        self.open_project_notes(&project_id, window, cx);
    }

    fn open_project_notes(&mut self, project_id: &str, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(panes) = self.project_panes(project_id, None, window, cx) {
            panes.update(cx, |p, cx| p.show_notes(window, cx));
        }
        cx.notify();
    }

    /// A project was added: bring back the notes left when a project in the same folder was
    /// removed.
    fn bring_back_notes(&mut self, project_id: &str, cx: &mut Context<Self>) {
        let Some(project) = self.state.project(project_id) else { return };
        let (name, path) = (project.name.clone(), project.path.clone());
        let live: Vec<String> = self.state.projects.iter().map(|p| p.id.clone()).collect();
        let live: Vec<&str> = live.iter().map(String::as_str).collect();
        self.notes.update(cx, |n, cx| n.reattach(project_id, &name, &path, &live, cx));
    }

    /// Every project: its id, name and folder.
    pub(crate) fn project_list(&self) -> Vec<(String, String, PathBuf)> {
        self.state.projects.iter().map(|p| (p.id.clone(), p.name.clone(), p.path.clone())).collect()
    }

    /// A thread's title, if it still exists.
    pub(crate) fn thread_title(&self, thread_id: &str) -> Option<String> {
        self.state.thread(thread_id).map(|(_, meta)| meta.title.clone().unwrap_or_else(|| "New thread".into()))
    }

    /// The thread the project's agent tab shows.
    pub(crate) fn shown_thread(&self, project_id: &str, cx: &App) -> Option<String> {
        self.agents.get(project_id).and_then(|a| a.read(cx).thread_id().map(str::to_string))
    }

    /// Open a thread at the user's `prompt`th message (its end when unknown).
    fn open_thread_at(&mut self, thread_id: &str, prompt: Option<usize>, window: &mut Window, cx: &mut Context<Self>) {
        if self.state.thread(thread_id).is_none() {
            return;
        }
        self.open_thread(thread_id, window, cx);
        if let (Some(prompt), Some(thread)) = (prompt, self.threads.get(thread_id).cloned()) {
            thread.update(cx, |t, cx| t.reveal_prompt(prompt, cx));
        }
    }

    /// Start a thread for a note: a new thread with the note in its prompt box, ready to send,
    /// and the note marked in progress.
    fn work_on_note(&mut self, project_id: &str, number: u64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(project) = self.state.project(project_id) else { return };
        let (name, path) = (project.name.clone(), project.path.clone());
        let Some(note) = self.notes.read(cx).book(project_id).and_then(|b| b.get(number)).cloned() else { return };
        let Some(thread_id) = self.state.new_thread(project_id) else { return };
        self.open_thread(&thread_id, window, cx);
        let prompt = crate::notes::work_prompt(&note);
        if let Some(thread) = self.threads.get(&thread_id).cloned() {
            thread.update(cx, |t, cx| t.set_prompt(&prompt, window, cx));
        }
        let id = thread_id.clone();
        self.notes.update(cx, |n, cx| {
            n.edit(
                project_id,
                &name,
                &path,
                |book| {
                    if let Some(note) = book.get_mut(number) {
                        note.status = crate::notes::NoteStatus::Doing;
                        note.work_thread = Some(id);
                        note.updated = store::now();
                    }
                },
                cx,
            )
        });
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
        let github = cx.new(|cx| {
            GitHubView::new(
                project.path.clone(),
                project.github_account.clone(),
                project.github_merge_method,
                window,
                cx,
            )
        });
        let github_project = project.id.clone();
        let github_root = project.path.clone();
        self._subscriptions.push(cx.subscribe_in(&github, window, move |this, _, event: &GitHubEvent, window, cx| {
            match event {
                GitHubEvent::AccountChosen(login) => {
                    if let Some(project) = this.state.project_mut(&github_project) {
                        project.github_account = login.clone();
                    }
                    this.save();
                }
                GitHubEvent::MergeMethodChosen(method) => {
                    if let Some(project) = this.state.project_mut(&github_project) {
                        project.github_merge_method = Some(*method);
                    }
                    this.save();
                }
                GitHubEvent::ExpandFile(file) => {
                    this.open_pr_file(&github_project, github_root.clone(), file.clone(), window, cx)
                }
            }
        }));
        self.githubs.insert(project.id.clone(), github.clone());
        let workspace = cx.entity().downgrade();
        let store = self.notes.clone();
        let notes = cx.new(|cx| {
            NotesView::new(store, workspace, project.id.clone(), project.name.clone(), project.path.clone(), cx)
        });
        let notes_project = project.id.clone();
        self._subscriptions.push(cx.subscribe_in(&notes, window, move |this, _, event: &NotesEvent, window, cx| {
            match event {
                NotesEvent::OpenThread { thread, prompt } => this.open_thread_at(thread, *prompt, window, cx),
                NotesEvent::WorkOn(number) => this.work_on_note(&notes_project, *number, window, cx),
            }
        }));
        self.notes_views.insert(project.id.clone(), notes.clone());
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
        let area = project.area_layout();
        let panes = cx.new(|cx| {
            Panes::new(
                project.path.clone(),
                area.as_ref(),
                agent,
                review,
                github,
                notes,
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
                    project.area = layout;
                    project.layout = None;
                }
                // The agent's tab may have come on show.
                this.mark_shown_seen(cx);
                this.save();
            }
            PaneEvent::ThreadFocused(thread) => {
                this.active = Some(thread.clone());
                this.state.selected_thread = this.active.clone();
                this.mark_shown_seen(cx);
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
                project.area = layout;
                project.layout = None;
            }
        }
        self.save();
    }

    // ---------- rendering ----------

    fn sidebar(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let settings = Button::new("settings-button")
            .ghost()
            .small()
            .icon(IconName::Settings)
            .tooltip(crate::keybindings::tooltip("Settings", "settings", cx))
            .on_click(cx.listener(|this, _, window, cx| this.open_settings(window, cx)));
        // A working thread's agent mark breathes instead of showing a dot.
        let working = self.threads.values().any(|t| t.read(cx).status == Status::Running);
        let breath = if working { crate::working::breath(crate::working::tick(window, cx)) } else { 1.0 };
        let theme = cx.theme();
        let (muted, accent_bg, fg) =
            (theme.sidebar_foreground.opacity(0.6), theme.sidebar_accent, theme.sidebar_foreground);
        let (waiting, done, stopped) = (theme.primary, theme.success, theme.danger);
        let alpha = cx.try_global::<Settings>().map(Settings::sidebar_alpha).unwrap_or(1.0);
        let sidebar_bg = theme.sidebar.opacity(alpha);
        // The selected thread is a card like the status column's sections, but nearly solid so it
        // stands out from the rows that want attention; a hovered row is a fainter one. On a light
        // theme a near-solid card reads lighter than a see-through frame, so the row is shaded
        // with the ink instead: darker than whatever is behind it, solid or not.
        let card_alpha = cx.try_global::<Settings>().map(Settings::card_alpha).unwrap_or(0.5);
        let (selected_bg, hover_bg) = if theme.is_dark() {
            (accent_bg.opacity((card_alpha + 0.4).min(1.0)), accent_bg.opacity(card_alpha * 0.6))
        } else {
            (fg.opacity(0.12), fg.opacity(0.06))
        };
        let active = self.active.clone();

        let workspace = cx.entity();
        let renaming = self.renaming.clone();
        let thread_row = |meta: &store::ThreadMeta, cx: &mut Context<Self>| {
            let id = meta.id.clone();
            let status = self.threads.get(&meta.id).map(|t| t.read(cx).status).unwrap_or(Status::Idle);
            // Not looked at since it asked something (?), finished (✓) or stopped on a problem
            // (!): the row is outlined in the mark's colour and its title bold, with the mark
            // where the waiting dot would be.
            let unseen = meta.unseen.as_ref().map(|a| a.kind);
            let unseen_mark = unseen.map(|kind| match kind {
                AttentionKind::Finished => (IconName::CircleCheck, done, "Finished, not looked at yet"),
                AttentionKind::Stopped => (IconName::CircleAlert, stopped, "Stopped on a problem, not looked at yet"),
                _ => (IconName::CircleQuestionMark, waiting, "Waiting for you, not looked at yet"),
            });
            let dot = (status == Status::AwaitingApproval && unseen.is_none()).then_some(waiting);
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
                .when(unseen.is_some(), |el| el.font_weight(FontWeight::SEMIBOLD))
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
                // Every row has the border, so outlining one doesn't shift its text.
                .border_1()
                .border_color(unseen_mark.map_or(transparent_black(), |(_, color, _)| color.opacity(0.8)))
                .map(|el| if selected { el.bg(selected_bg) } else { el.hover(|el| el.bg(hover_bg)) })
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
                .when_some(unseen_mark, |el, (icon, color, tip)| {
                    el.child(
                        div()
                            .id(SharedString::from(format!("thread-unseen-{}", meta.id)))
                            .debug_selector({
                                let id = meta.id.clone();
                                move || format!("thread-unseen-{id}")
                            })
                            .flex_none()
                            .child(Icon::new(icon).small().text_color(color))
                            .tooltip(move |window, cx| {
                                gpui_kit::component::tooltip::Tooltip::new(tip).build(window, cx)
                            }),
                    )
                })
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
                    let (w1, w2, w3) = (workspace.clone(), workspace.clone(), workspace.clone());
                    let (i1, i2, i3) = (id.clone(), id.clone(), id.clone());
                    let menu = menu
                        .item(
                            PopupMenuItem::new("New thread")
                                .on_click(move |_, window, cx| w1.update(cx, |w, cx| w.new_thread(&i1, window, cx))),
                        )
                        .item(PopupMenuItem::new("Notes").on_click(move |_, window, cx| {
                            w3.update(cx, |w, cx| w.open_project_notes(&i3, window, cx))
                        }))
                        .separator();
                    image_choices.add_to(menu).separator().item(PopupMenuItem::new("Remove from TJ…").on_click(
                        move |_, window, cx| w2.update(cx, |w, cx| w.confirm_remove_project(&i2, window, cx)),
                    ))
                }
            };
            let toggle_id = project.id.clone();
            let history_id = project.id.clone();
            let notes_id = project.id.clone();
            let pending_notes = self.notes.read(cx).pending(&project.id);
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
                            .when(pending_notes > 0, |el| {
                                el.child(
                                    Button::new(SharedString::from(format!("project-notes-{}", project.id)))
                                        .ghost()
                                        .xsmall()
                                        .icon(IconName::StickyNote)
                                        .label(pending_notes.to_string())
                                        .tooltip(format!(
                                            "{pending_notes} note{} to do",
                                            if pending_notes == 1 { "" } else { "s" }
                                        ))
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            this.open_project_notes(&notes_id, window, cx)
                                        })),
                                )
                            })
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
                                .hover(|el| el.bg(hover_bg))
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
    fn content_header(&self, status_shown: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (waiting, muted, fg) = (theme.warning, theme.muted_foreground, theme.foreground);
        let (tab, tab_fg, shown) = (theme.tab, theme.tab_foreground, theme.tab_active);
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
        // The tabs are the top bar's, each titled after its focused pane.
        let tabs = panes.as_ref().and_then(|panes| panes.read(cx).header_tabs(panes.clone(), avatar(16.), cx));

        // Every action is its own round chip.
        let chip = |id: &'static str, icon: IconName, tip: SharedString| {
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
                .hover(|el| el.bg(shown).text_color(fg))
                .tooltip(move |window, cx| gpui_kit::component::tooltip::Tooltip::new(tip.clone()).build(window, cx))
                .child(Icon::new(icon).small())
        };
        let status_toggle = self.has_status(cx).then(|| {
            let shown = status_shown;
            chip(
                "header-status",
                if shown { IconName::PanelRightClose } else { IconName::PanelRightOpen },
                if shown { "Hide the thread's status" } else { "Show the thread's status" }.into(),
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                this.status_choice = Some(!shown);
                cx.notify();
            }))
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
            crate::keybindings::tooltip(
                if collapsed { "Show the sidebar" } else { "Hide the sidebar" },
                "toggle_sidebar",
                cx,
            )
            .into(),
        )
        .on_click(cx.listener(|this, _, _, cx| this.toggle_sidebar(cx)));
        let new_terminal = has_panes.then(|| {
            chip(
                "header-new-terminal",
                IconName::Plus,
                crate::keybindings::tooltip("New terminal tab", "new_tab", cx).into(),
            )
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
                    chip(
                        "header-github",
                        IconName::Github,
                        crate::keybindings::tooltip("Pull requests and branches on GitHub", "open_github", cx).into(),
                    )
                    .on_click(cx.listener(|this, _, window, cx| this.open_github(window, cx))),
                )
                .child(
                    chip(
                        "header-notes",
                        IconName::StickyNote,
                        crate::keybindings::tooltip("Project notes: what was put off for later", "open_notes", cx)
                            .into(),
                    )
                    .on_click(cx.listener(|this, _, window, cx| this.open_notes(window, cx))),
                )
                .child(
                    chip(
                        "header-split-right",
                        IconName::Columns2,
                        crate::keybindings::tooltip("Split right", "split_right", cx).into(),
                    )
                    .on_click(with_panes(|p, window, cx| p.split(SplitDirection::Right, window, cx))),
                )
                .child(
                    chip(
                        "header-split-down",
                        IconName::Rows2,
                        crate::keybindings::tooltip("Split down", "split_down", cx).into(),
                    )
                    .on_click(with_panes(|p, window, cx| p.split(SplitDirection::Down, window, cx))),
                )
            })
            .children(status_toggle)
            .child(chip("close-window", IconName::X, "Close".into()).on_click(cx.listener(|this, _, window, cx| {
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
        // see-through with it. The top bar is part of it, with the tabs floating on it, and the
        // content below is the theme's own background.
        let alpha = cx.try_global::<Settings>().map(Settings::sidebar_alpha).unwrap_or(1.0);
        let frame = theme.sidebar.opacity(alpha);
        let card_alpha = cx.try_global::<Settings>().map(Settings::card_alpha).unwrap_or(0.5);
        let card = theme.sidebar_accent.opacity(card_alpha);

        // The thread's status sits in the frame at the right, under the top bar, mirroring the
        // sidebar on the left, for whichever pane is focused.
        let status_shown = self.status_shown(window, cx);
        let status = self
            .active_agent()
            .filter(|_| status_shown)
            .and_then(|agent| agent.update(cx, |a, cx| a.status_column(card, cx)));

        // The window itself is transparent: only the content under the top bar paints an
        // opaque background, so the sidebar and the frame can be see-through.
        let sidebar = (!self.state.sidebar_collapsed).then(|| self.sidebar(window, cx).into_any_element());
        crate::caret::watch(h_flex())
            .key_context(CONTEXT)
            .on_action(cx.listener(|this, _: &ToggleSidebar, _, cx| this.toggle_sidebar(cx)))
            .on_action(cx.listener(|this, _: &NextProject, window, cx| this.step_project(1, window, cx)))
            .on_action(cx.listener(|this, _: &PreviousProject, window, cx| this.step_project(-1, window, cx)))
            .on_action(cx.listener(|this, _: &NewThread, window, cx| this.new_thread_here(window, cx)))
            .on_action(cx.listener(|this, _: &OpenSwitcher, window, cx| this.open_switcher(window, cx)))
            .on_action(cx.listener(|this, _: &OpenGitHub, window, cx| this.open_github(window, cx)))
            .on_action(cx.listener(|this, _: &OpenNotes, window, cx| this.open_notes(window, cx)))
            .on_action(cx.listener(|this, _: &FocusAgent, window, cx| this.focus_agent(window, cx)))
            .on_action(cx.listener(|this, _: &OpenSettings, window, cx| this.open_settings(window, cx)))
            .size_full()
            .text_color(fg)
            .children(sidebar)
            .child(
                v_flex().flex_1().min_w_0().h_full().bg(frame).child(self.content_header(status_shown, cx)).child(
                    h_flex()
                        .flex_1()
                        .min_h_0()
                        .min_w_0()
                        .child(
                            v_flex()
                                .flex_1()
                                .h_full()
                                .min_w_0()
                                // Rounded at the top like a browser's page, so it sits in the
                                // frame. The views in it round their own backgrounds to match.
                                .rounded_t(px(crate::panes::CONTENT_RADIUS))
                                .overflow_hidden()
                                .bg(bg)
                                .when_some(self.problem.clone(), |el, problem| {
                                    el.child(div().px_4().py_2().text_sm().text_color(danger).child(problem))
                                })
                                .child(div().flex_1().min_h_0().min_w_0().children(panes)),
                        )
                        .children(status),
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

    type Opened = (gpui_kit::AnyWindowHandle, Entity<Workspace>, Paths, std::path::PathBuf);

    fn open(cx: &mut TestAppContext) -> Opened {
        open_with_shell(cx, alacritty_terminal::tty::Shell::new("/bin/sh".into(), vec![]))
    }

    /// Terminals that print nothing (`sleep`), for tests that open one and let the scheduler
    /// run: a shell's prompt would wake the view from the PTY's thread, which the test
    /// scheduler rejects.
    fn open_quiet(cx: &mut TestAppContext) -> Opened {
        open_with_shell(cx, alacritty_terminal::tty::Shell::new("sleep".into(), vec!["60".into()]))
    }

    fn open_with_shell(cx: &mut TestAppContext, shell: alacritty_terminal::tty::Shell) -> Opened {
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
            shell: Some(shell),
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
    fn a_thread_that_asks_or_finishes_unseen_is_marked_until_looked_at_and_can_remind(cx: &mut TestAppContext) {
        use crate::attention::{AttentionKind, REMIND_10_MINUTES, REMIND_TOMORROW};
        use crate::thread::ThreadNeedsYou;
        let (window, workspace, paths, _) = open(cx);
        let cx = &mut VisualTestContext::from_window(window, cx);
        let (_, first, second) = two_threads(&workspace, cx);
        let ask = |thread: &str, kind: AttentionKind, cx: &mut VisualTestContext| {
            let view = workspace.read_with(cx, |w, _| w.threads[thread].clone());
            view.update(cx, |_, cx| cx.emit(ThreadNeedsYou { kind, detail: "Which colour?".into() }));
        };
        let unseen = |thread: &str, _: &mut VisualTestContext| {
            let saved = State::load(&paths).unwrap();
            saved.projects[0].threads.iter().find(|t| t.id == thread).unwrap().unseen.as_ref().map(|a| a.kind)
        };

        // The thread on screen, with the window focused, is being looked at: nothing to mark.
        workspace.update(cx, |w, _| w.window_active = true);
        ask(&second, AttentionKind::Finished, cx);
        assert_eq!(unseen(&second, cx), None);

        // One that isn't on screen is marked, and the mark is drawn in its row.
        ask(&first, AttentionKind::Question, cx);
        assert_eq!(unseen(&first, cx), Some(AttentionKind::Question));
        cx.update(|window, cx| window.render_frame(cx));
        // Selectors are 'static.
        let mark = |thread: &str| -> &'static str { format!("thread-unseen-{thread}").leak() };
        assert!(cx.debug_bounds(mark(&first)).is_some());
        assert!(cx.debug_bounds(mark(&second)).is_none());

        // With the window in the background, the thread on screen is marked too, until the
        // window is focused again.
        workspace.update(cx, |w, _| w.window_active = false);
        ask(&second, AttentionKind::Finished, cx);
        assert_eq!(unseen(&second, cx), Some(AttentionKind::Finished));
        workspace.update(cx, |w, cx| {
            w.window_active = true;
            w.mark_shown_seen(cx);
        });
        assert_eq!(unseen(&second, cx), None);

        // A button on the notification brings it back later; a reminder past due shows and goes.
        let respond = |action: Option<&str>, cx: &mut VisualTestContext| {
            let response = gpui_kit::SystemNotificationResponse {
                tag: first.clone().into(),
                action_id: action.map(|a| SharedString::from(a.to_string())),
            };
            workspace.update_in(cx, |w, window, cx| w.notification_response(response, window, cx));
        };
        let now = crate::store::now();
        respond(Some(REMIND_10_MINUTES), cx);
        let reminders = State::load(&paths).unwrap().reminders;
        assert_eq!(reminders.len(), 1);
        assert!(reminders[0].thread_id == first && (now + 600..=now + 602).contains(&reminders[0].due));
        respond(Some(REMIND_TOMORROW), cx);
        let reminders = State::load(&paths).unwrap().reminders;
        assert!(reminders.len() == 1 && reminders[0].due > now + 600, "the later one replaces it");
        workspace.update(cx, |w, cx| {
            w.state.reminders[0].due = now - 1;
            w.show_due_reminders(cx);
        });
        assert!(State::load(&paths).unwrap().reminders.is_empty());
        assert_eq!(unseen(&first, cx), Some(AttentionKind::Question), "still not looked at");

        // Clicking the notification opens the thread, which takes its mark and reminders off.
        respond(Some(REMIND_10_MINUTES), cx);
        respond(None, cx);
        assert_eq!(workspace.read_with(cx, |w, _| w.active.clone()), Some(first.clone()));
        assert_eq!(unseen(&first, cx), None);
        assert!(State::load(&paths).unwrap().reminders.is_empty());
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
        assert_eq!(panes.read_with(cx, |p, _| p.shape()), vec![1, 1], "the agent's tab and a terminal's");

        cx.update(|window, cx| {
            window.render_frame(cx);
            window.click(SharedString::from(format!("thread-{first}")), cx);
        });
        assert_eq!(panes.read_with(cx, |p, _| p.shape()), vec![1, 1], "no tab per thread");
        assert_eq!(panes.read_with(cx, |p, cx| p.active_thread_id(cx)), Some(first.clone()));
        assert_eq!(workspace.read_with(cx, |w, _| w.active.clone()), Some(first));

        cx.update(|window, cx| {
            window.render_frame(cx);
            window.click(SharedString::from(format!("thread-{second}")), cx);
        });
        assert_eq!(panes.read_with(cx, |p, _| p.shape()), vec![1, 1]);
        assert_eq!(panes.read_with(cx, |p, cx| p.active_thread_id(cx)), Some(second));
        let layout = panes.read_with(cx, |p, cx| p.snapshot(cx)).unwrap();
        assert_eq!(
            layout.tabs[0],
            crate::store::SplitLayout::Pane { view: crate::store::TabState::Agent },
            "saved without a thread id"
        );
    }

    #[gpui_kit::test]
    fn the_status_column_sits_in_the_frame_and_folds_when_the_window_is_narrow(cx: &mut TestAppContext) {
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
        assert!(status_showing(cx), "a wide window shows the status column");

        // A narrow window folds it away.
        cx.simulate_resize(size(px(1000.), px(900.)));
        settle(cx);
        assert!(!status_showing(cx), "folded when narrow");

        // The top bar shows it again by hand.
        cx.update(|window, cx| window.click(SharedString::from("header-status"), cx));
        settle(cx);
        assert!(status_showing(cx), "shown by hand");
        cx.update(|window, cx| window.click(SharedString::from("header-status"), cx));
        settle(cx);
        assert!(!status_showing(cx), "and hidden again");

        // It belongs to the window, not the agent tab: in a wide window a split leaves it shown.
        cx.update(|window, cx| window.click(SharedString::from("header-status"), cx));
        cx.simulate_resize(size(px(1400.), px(900.)));
        settle(cx);
        cx.update(|window, cx| {
            window.click(SharedString::from("header-split-right"), cx);
            window.render_frame(cx);
        });
        settle(cx);
        assert!(status_showing(cx), "still shown beside a split");
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
    fn the_github_button_opens_the_projects_github_tab_and_remembers_its_account(cx: &mut TestAppContext) {
        let (window, workspace, paths, _) = open(cx);
        let cx = &mut VisualTestContext::from_window(window, cx);
        cx.update(|window, cx| {
            window.render_frame(cx);
            window.click(SharedString::from("header-github"), cx);
        });
        let panes = workspace.read_with(cx, |w, _| w.panes.values().next().cloned().unwrap());
        assert!(panes.read_with(cx, |p, _| p.github_is_showing()), "the GitHub page opens as a tab");
        assert!(panes.read_with(cx, |p, cx| p.snapshot(cx)).is_some_and(|l| {
            l.tabs.contains(&crate::store::SplitLayout::Pane { view: crate::store::TabState::GitHub })
        }));
        // Opening it again shows the same tab instead of another.
        workspace.update_in(cx, |w, window, cx| w.open_github(window, cx));
        assert_eq!(panes.read_with(cx, |p, _| p.shape()), vec![1, 1]);

        // An account picked in the tab is kept for the project.
        let github = workspace.read_with(cx, |w, _| w.githubs.values().next().cloned().unwrap());
        github.update(cx, |_, cx| cx.emit(crate::github_view::GitHubEvent::AccountChosen(Some("work-login".into()))));
        assert_eq!(State::load(&paths).unwrap().projects[0].github_account.as_deref(), Some("work-login"));
        github.update(cx, |_, cx| cx.emit(crate::github_view::GitHubEvent::AccountChosen(None)));
        assert_eq!(State::load(&paths).unwrap().projects[0].github_account, None);

        // So is the merge method picked last.
        use crate::github::MergeMethod;
        github.update(cx, |_, cx| cx.emit(crate::github_view::GitHubEvent::MergeMethodChosen(MergeMethod::Merge)));
        assert_eq!(State::load(&paths).unwrap().projects[0].github_merge_method, Some(MergeMethod::Merge));
    }

    #[gpui_kit::test]
    fn notes_open_from_their_shortcut_start_threads_and_outlive_their_project(cx: &mut TestAppContext) {
        use crate::notes::{Author, NewNote, NoteStatus, Source};
        use crate::notes_view::NotesEvent;
        use std::path::Path;
        let (window, workspace, paths, project) = open_quiet(cx);
        let cx = &mut VisualTestContext::from_window(window, cx);
        cx.update(|window, cx| window.render_frame(cx));
        let (project_id, first) =
            workspace.read_with(cx, |w, _| (w.state.projects[0].id.clone(), w.active.clone().unwrap()));
        let store = workspace.read_with(cx, |w, _| w.notes.clone());

        // A note Claude wrote in the first thread, as the notes server would.
        let file = crate::notes::notes_file(&paths.data_dir, &project_id);
        let source = Source { thread: Some(first.clone()), prompt: Some(1), ..Default::default() };
        crate::notes::update(&file, "demo-project", &project, |b| {
            b.add(NewNote {
                title: "Prune list".into(),
                author: Author::Agent,
                source: Some(source),
                ..Default::default()
            })
        })
        .unwrap();
        store.update(cx, |s, cx| s.edit(&project_id, "", Path::new(""), |_| (), cx));
        assert_eq!(store.read_with(cx, |s, _| s.pending(&project_id)), 1, "the sidebar counts it");

        // Ctrl+Shift+N opens the notes tab, once.
        cx.simulate_keystrokes("ctrl-shift-n");
        let panes = workspace.read_with(cx, |w, _| w.panes.get(&project_id).cloned().unwrap());
        assert!(panes.read_with(cx, |p, _| p.notes_are_showing()), "the notes open as a tab");
        workspace.update_in(cx, |w, window, cx| w.open_notes(window, cx));
        assert!(panes.read_with(cx, |p, cx| p.snapshot(cx)).is_some_and(|l| {
            l.tabs
                .iter()
                .filter(|t| **t == crate::store::SplitLayout::Pane { view: crate::store::TabState::Notes })
                .count()
                == 1
        }));
        cx.update(|window, cx| window.render_frame(cx));

        // A note written in the tab, from the keyboard.
        let notes = workspace.read_with(cx, |w, _| w.notes_views.get(&project_id).cloned().unwrap());
        cx.simulate_keystrokes("alt-n");
        assert!(notes.read_with(cx, |n, _| n.is_editing()));
        cx.simulate_input("Fix the flaky test");
        cx.update(|window, cx| window.render_frame(cx));
        cx.simulate_keystrokes("ctrl-enter");
        assert!(!notes.read_with(cx, |n, _| n.is_editing()));
        let book = store.read_with(cx, |s, _| s.book(&project_id).cloned().unwrap());
        let mine = book.get(2).expect("saved as #2");
        assert_eq!((mine.title.as_str(), mine.author), ("Fix the flaky test", Author::User));
        assert_eq!(notes.read_with(cx, |n, _| n.selected()), Some(2));

        // Work on this: a new thread with the note in its prompt box, and the note in progress.
        cx.update(|window, cx| window.render_frame(cx));
        cx.simulate_keystrokes("down");
        assert_eq!(notes.read_with(cx, |n, _| n.selected()), Some(1));
        cx.simulate_keystrokes("alt-w");
        let worker = workspace.read_with(cx, |w, _| w.active.clone().unwrap());
        assert_ne!(worker, first);
        let thread = workspace.read_with(cx, |w, _| w.threads.get(&worker).cloned().unwrap());
        assert!(
            thread.read_with(cx, |t, cx| t.prompt_text(cx)).starts_with("Let's work on project note #1: Prune list")
        );
        let note = store.read_with(cx, |s, _| s.book(&project_id).unwrap().get(1).cloned().unwrap());
        assert_eq!((note.status, note.work_thread.as_deref()), (NoteStatus::Doing, Some(worker.as_str())));

        // Going to where it was written shows that thread.
        notes.update(cx, |_, cx| cx.emit(NotesEvent::OpenThread { thread: first.clone(), prompt: Some(1) }));
        assert_eq!(workspace.read_with(cx, |w, _| w.active.clone()), Some(first.clone()));

        // Notes left by a removed project are on the cleanup page (Alt+C); Alt+M moves them here.
        let gone = crate::notes::notes_file(&paths.data_dir, "gone-project");
        crate::notes::update(&gone, "Old", Path::new("/nowhere/old"), |b| {
            b.add(NewNote { title: "Port the parser".into(), ..Default::default() })
        })
        .unwrap();
        store.update(cx, |s, cx| s.edit("gone-project", "", Path::new(""), |_| (), cx));
        workspace.update_in(cx, |w, window, cx| w.open_notes(window, cx));
        cx.update(|window, cx| window.render_frame(cx));
        cx.simulate_keystrokes("alt-c");
        assert!(notes.read_with(cx, |n, _| n.is_cleaning_up()));
        cx.update(|window, cx| window.render_frame(cx));
        cx.simulate_keystrokes("alt-m");
        assert!(!gone.exists());
        let titles: Vec<String> =
            store.read_with(cx, |s, _| s.book(&project_id).unwrap().notes.iter().map(|n| n.title.clone()).collect());
        assert_eq!(titles.last().map(String::as_str), Some("Port the parser"), "numbered after this project's");
        cx.update(|window, cx| window.render_frame(cx));
        assert!(!notes.read_with(cx, |n, _| n.is_cleaning_up()), "an empty cleanup page closes itself");

        // Removing the project keeps its notes; adding the folder again brings them back.
        workspace.update_in(cx, |w, window, cx| w.remove_project(&project_id, window, cx));
        assert!(file.is_file(), "the notes outlive the project");
        let orphans = store.read_with(cx, |s, _| s.orphans(&[]).len());
        assert_eq!(orphans, 1, "listed for cleanup");
        let again = workspace.update(cx, |w, cx| {
            let id = w.state.add_project(&project).id.clone();
            w.bring_back_notes(&id, cx);
            id
        });
        assert_ne!(again, project_id);
        assert_eq!(store.read_with(cx, |s, _| s.book(&again).map(|b| b.notes.len())), Some(3));
        assert!(!file.exists(), "moved, not copied");
        cx.update(|window, cx| window.render_frame(cx));
    }

    #[gpui_kit::test]
    fn clicking_a_changed_file_opens_its_review_and_view_all_shows_every_file(cx: &mut TestAppContext) {
        let (window, workspace, _, project) = open(cx);
        let cx = &mut VisualTestContext::from_window(window, cx);
        let run = |args: &[&str]| {
            assert!(
                std::process::Command::new("git").arg("-C").arg(&project).args(args).output().unwrap().status.success()
            )
        };
        run(&["init", "-q"]);
        std::fs::write(project.join("notes.md"), "a new file\n").unwrap();
        std::fs::write(project.join("todo.md"), "another\n").unwrap();

        // Changes are read in the background; wait for them to arrive.
        let changes = workspace.update(cx, |w, cx| w.project_changes(cx).unwrap());
        cx.run_until_parked();
        for _ in 0..200 {
            if changes.read_with(cx, |c, _| c.changes.files.len() == 2) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
            cx.run_until_parked();
        }
        assert_eq!(changes.read_with(cx, |c, _| c.changes.files[0].path.clone()), "notes.md");
        let shown = |cx: &mut VisualTestContext| {
            changes.read_with(cx, |c, cx| (c.focus.clone(), c.shown().count(), c.diff.read(cx).files().len()))
        };

        // The status column (and its file list) shows even while the thread is on its intro.
        // A file opens the review of just that file.
        cx.update(|window, cx| {
            window.render_frame(cx);
            window.click(SharedString::from("change-0"), cx);
        });
        let panes = workspace.read_with(cx, |w, _| w.panes.values().next().cloned().unwrap());
        assert!(panes.read_with(cx, |p, _| p.review_is_showing()), "the review opens as a tab");
        assert_eq!(shown(cx), (Some("notes.md".into()), 1, 1));

        // View all shows (and summarizes) every file.
        cx.update(|window, cx| {
            window.render_frame(cx);
            window.click(SharedString::from("review-changes"), cx);
        });
        assert_eq!(shown(cx), (None, 2, 2));

        // Ctrl+W closes it like any tab, and the conversation shows again.
        cx.update(|window, cx| {
            window.render_frame(cx);
            window.press("ctrl-w", cx);
        });
        assert!(!panes.read_with(cx, |p, _| p.review_is_showing()));
        assert!(panes.read_with(cx, |p, cx| p.active_thread_id(cx).is_some()));
    }

    #[gpui_kit::test]
    fn the_settings_modal_turns_on_the_transparent_sidebar_and_saves_it(cx: &mut TestAppContext) {
        let (window, _workspace, paths, _) = open(cx);
        let cx = &mut VisualTestContext::from_window(window, cx);
        cx.update(|_, cx| cx.set_global(crate::settings::Settings::default()));
        cx.update(|window, cx| {
            window.render_frame(cx);
            window.click(SharedString::from("settings-button"), cx);
        });
        cx.run_until_parked();
        assert!(cx.update(gpui_kit::component::WindowExt::has_active_dialog), "the button opens the modal");
        // General first; Ctrl+PageDown moves to Appearance.
        cx.simulate_keystrokes("ctrl-pagedown");
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
    fn fonts_are_picked_from_the_keyboard(cx: &mut TestAppContext) {
        use crate::settings_view::FontKind;
        let (window, workspace, paths, _) = open(cx);
        let cx = &mut VisualTestContext::from_window(window, cx);
        cx.update(|_, cx| cx.set_global(crate::settings::Settings::default()));
        cx.update(|window, cx| window.render_frame(cx));
        cx.simulate_keystrokes("ctrl-,");
        let view = workspace.read_with(cx, |w, _| w.settings_view.clone()).and_then(|v| v.upgrade()).unwrap();
        let mono =
            |cx: &mut VisualTestContext| cx.update(|_, cx| cx.global::<crate::settings::Settings>().mono_font.clone());
        // Appearance: Follow theme, Interface font, Monospace font.
        cx.simulate_keystrokes("ctrl-pagedown down down enter");
        assert_eq!(view.read_with(cx, |v, _| v.picking_font()), Some(FontKind::Mono), "Enter opens the list");
        cx.simulate_input("r ");
        assert_eq!(view.read_with(cx, |v, _| v.picking_font()), Some(FontKind::Mono), "R and Space are typed");
        cx.simulate_keystrokes("escape");
        assert_eq!(view.read_with(cx, |v, _| v.picking_font()), None, "Escape goes back to the rows");
        assert!(cx.update(gpui_kit::component::WindowExt::has_active_dialog), "and leaves the dialog open");

        // The test platform has no fonts; give it two.
        let first = "Fira Mono".to_string();
        cx.update(|_, cx| {
            let names: std::rc::Rc<[String]> = [first.clone(), "Iosevka".to_string()].into();
            let families = crate::fonts::Families { installed: names.clone(), ui: names.clone(), mono: Some(names) };
            cx.set_global(families);
        });
        // The list starts on the default; Down and Enter use the first family.
        cx.simulate_keystrokes("enter down enter");
        assert_eq!(view.read_with(cx, |v, _| v.picking_font()), None);
        assert!(cx.update(gpui_kit::component::WindowExt::has_active_dialog), "Enter doesn't close the dialog");
        assert_eq!(mono(cx), Some(first.clone()));
        assert_eq!(crate::settings::Settings::load(&paths).mono_font, Some(first.clone()), "saved");
        assert_eq!(
            cx.update(|_, cx| gpui_kit::component::ActiveTheme::theme(cx).mono_font_family.to_string()),
            first,
            "and drawn with"
        );
        cx.simulate_keystrokes("backspace");
        assert_eq!(mono(cx), None, "Backspace gives back the default");
        // Typing filters the list: "iose" leaves only Iosevka.
        cx.simulate_keystrokes("enter");
        cx.simulate_input("iose");
        cx.simulate_keystrokes("enter");
        assert_eq!(mono(cx).as_deref(), Some("Iosevka"));
        cx.simulate_keystrokes("left");
        assert_eq!(mono(cx), Some(first.clone()), "Left steps to the family before");
        cx.simulate_keystrokes("left");
        assert_eq!(mono(cx), None, "Left steps back to the default");
        let default = cx.update(|_, cx| crate::fonts::defaults(cx).mono);
        assert_eq!(cx.update(|_, cx| gpui_kit::component::ActiveTheme::theme(cx).mono_font_family.clone()), default);
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
    fn a_split_stays_in_its_tab_and_the_tab_is_named_after_its_focused_pane(cx: &mut TestAppContext) {
        let (window, workspace, _, _) = open(cx);
        let cx = &mut VisualTestContext::from_window(window, cx);
        let panes = workspace.read_with(cx, |w, _| w.panes.values().next().cloned().unwrap());
        assert_eq!(panes.read_with(cx, |p, _| p.shape()), vec![1], "a new project starts with just its conversation");
        cx.update(|window, cx| {
            window.render_frame(cx);
            assert!(window.try_find(SharedString::from("header-tab-0")).is_some(), "the agent is a tab in the top bar");
            window.click(SharedString::from("header-new-terminal"), cx);
            window.render_frame(cx);
            assert!(window.try_find(SharedString::from("header-tab-1")).is_some(), "a terminal gets a tab of its own");
            window.click(SharedString::from("header-split-right"), cx);
            window.render_frame(cx);
            assert!(window.try_find(SharedString::from("header-tab-1")).is_some(), "split, the tabs stay");
            assert!(window.try_find(SharedString::from("header-tab-2")).is_none(), "and the split adds none");
        });
        assert_eq!(panes.read_with(cx, |p, _| p.shape()), vec![1, 2], "the split lives in the terminal's tab");

        cx.update(|window, cx| window.click(SharedString::from("header-tab-0"), cx));
        assert_eq!(panes.read_with(cx, |p, _| p.active_tab()), 0);
        assert!(panes.read_with(cx, |p, cx| p.active_thread_id(cx)).is_some(), "the agent's tab shows just the agent");
        cx.update(|window, cx| window.render_frame(cx));
        assert!(cx.debug_bounds("divider-").is_none(), "with no split in it");

        let split_pane = cx.update(|window, cx| {
            window.click(SharedString::from("header-tab-1"), cx);
            window.render_frame(cx);
            panes.read(cx).focused_pane()
        });
        assert!(cx.debug_bounds("divider-").is_some(), "back on the terminal's tab, the split is there");
        cx.update(|window, cx| panes.update(cx, |p, cx| p.focus_direction(-1.0, 0.0, window, cx)));
        assert_ne!(panes.read_with(cx, |p, _| p.focused_pane()), split_pane, "focus moves within the tab");
        assert_eq!(panes.read_with(cx, |p, _| p.active_tab()), 1);
    }

    #[gpui_kit::test]
    fn the_sidebar_folds_away_from_the_top_bar_or_ctrl_shift_s_and_stays_so(cx: &mut TestAppContext) {
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
        cx.simulate_keystrokes("ctrl-shift-s");
        assert!(!collapsed(cx), "Ctrl+Shift+S brings it back");
        cx.simulate_keystrokes("ctrl-shift-s");
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

    /// A second project beside the demo one: (first project id, second project id).
    fn two_projects(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> (String, String) {
        let other = std::env::temp_dir().join(format!("tj-ws-other-{}", new_id()));
        std::fs::create_dir_all(&other).unwrap();
        workspace.update(cx, |w, _| {
            w.state.add_project(&other);
            (w.state.projects[0].id.clone(), w.state.projects[1].id.clone())
        })
    }

    fn shown_project(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> Option<String> {
        workspace.read_with(cx, |w, _| w.active_project_id())
    }

    #[gpui_kit::test]
    fn ctrl_alt_n_and_p_move_between_projects_and_ctrl_shift_o_starts_a_thread(cx: &mut TestAppContext) {
        let (window, workspace, _, _) = open(cx);
        let cx = &mut VisualTestContext::from_window(window, cx);
        let (first, second) = two_projects(&workspace, cx);
        cx.update(|window, cx| window.render_frame(cx));
        assert_eq!(shown_project(&workspace, cx).as_ref(), Some(&first));

        cx.simulate_keystrokes("ctrl-alt-n");
        assert_eq!(shown_project(&workspace, cx).as_ref(), Some(&second), "down to the next project");
        let second_thread = workspace.read_with(cx, |w, _| w.active.clone().unwrap());
        assert_eq!(workspace.read_with(cx, |w, _| w.state.project(&second).unwrap().threads.len()), 1);
        cx.simulate_keystrokes("ctrl-alt-n");
        assert_eq!(shown_project(&workspace, cx).as_ref(), Some(&first), "and round to the top");
        cx.simulate_keystrokes("ctrl-alt-p");
        assert_eq!(shown_project(&workspace, cx).as_ref(), Some(&second), "up wraps to the bottom");
        assert_eq!(
            workspace.read_with(cx, |w, _| w.active.clone()),
            Some(second_thread.clone()),
            "coming back shows the thread it was left on"
        );

        cx.simulate_keystrokes("ctrl-shift-o");
        let (active, threads) = workspace
            .read_with(cx, |w, _| (w.active.clone().unwrap(), w.state.project(&second).unwrap().threads.len()));
        assert_ne!(active, second_thread, "a new thread is shown");
        assert_eq!(threads, 2, "in the project on show");
    }

    #[gpui_kit::test]
    fn ctrl_shift_l_brings_back_the_agent_and_its_prompt_box_from_any_pane(cx: &mut TestAppContext) {
        let (window, workspace, _, _) = open_quiet(cx);
        let cx = &mut VisualTestContext::from_window(window, cx);
        cx.update(|window, cx| window.render_frame(cx));
        let panes = workspace.read_with(cx, |w, _| w.panes.values().next().cloned().unwrap());
        let thread = workspace.read_with(cx, |w, _| w.active_thread().unwrap());
        let on_prompt = |cx: &mut VisualTestContext| {
            cx.update(|window, cx| thread.read(cx).prompt_focus(cx).is_focused(window))
                && panes.read_with(cx, |p, cx| p.active_thread_id(cx).is_some())
        };

        // From another tab.
        cx.simulate_keystrokes("ctrl-shift-g");
        assert!(panes.read_with(cx, |p, _| p.github_is_showing()));
        cx.simulate_keystrokes("ctrl-shift-l");
        assert!(on_prompt(cx), "the agent's tab is on show, its prompt box focused");

        // From a terminal split beside it.
        panes.update_in(cx, |p, window, cx| p.split(crate::store::SplitDirection::Right, window, cx));
        assert!(panes.read_with(cx, |p, _| p.active_terminal().is_some()));
        cx.simulate_keystrokes("ctrl-shift-l");
        assert!(on_prompt(cx), "focus leaves the terminal for the prompt box");

        // A closed agent opens again.
        panes.update_in(cx, |p, window, cx| p.close_tab(window, cx));
        assert!(!panes.read_with(cx, |p, _| p.agent_on_show()));
        cx.simulate_keystrokes("ctrl-shift-l");
        assert!(on_prompt(cx), "the agent comes back in a tab of its own");
    }

    #[gpui_kit::test]
    fn moving_to_a_project_by_keyboard_takes_its_threads_mark_off(cx: &mut TestAppContext) {
        use crate::attention::AttentionKind;
        use crate::thread::ThreadNeedsYou;
        let (window, workspace, _, _) = open(cx);
        let cx = &mut VisualTestContext::from_window(window, cx);
        let (_, second) = two_projects(&workspace, cx);
        cx.update(|window, cx| window.render_frame(cx));
        workspace.update(cx, |w, _| w.window_active = true);
        // Open the second project once, so coming back to it shows the thread it was left on.
        cx.simulate_keystrokes("ctrl-alt-n");
        let thread = workspace.read_with(cx, |w, _| w.active.clone().unwrap());
        cx.simulate_keystrokes("ctrl-alt-p");

        let view = workspace.read_with(cx, |w, _| w.threads[&thread].clone());
        view.update(cx, |_, cx| cx.emit(ThreadNeedsYou { kind: AttentionKind::Question, detail: String::new() }));
        let unseen = |cx: &mut VisualTestContext| {
            workspace.read_with(cx, |w, _| w.state.thread(&thread).unwrap().1.unseen.as_ref().map(|a| a.kind))
        };
        assert_eq!(unseen(cx), Some(AttentionKind::Question), "marked while another project is on show");

        cx.simulate_keystrokes("ctrl-alt-n");
        assert_eq!(shown_project(&workspace, cx).as_ref(), Some(&second));
        assert_eq!(unseen(cx), None, "looked at once its project is shown again");
    }

    #[gpui_kit::test]
    fn a_rebound_shortcut_takes_effect_at_once_and_is_saved(cx: &mut TestAppContext) {
        let (window, workspace, paths, _) = open(cx);
        let cx = &mut VisualTestContext::from_window(window, cx);
        let (first, second) = two_projects(&workspace, cx);
        cx.update(|window, cx| window.render_frame(cx));
        workspace.update(cx, |w, cx| {
            w.update_settings(cx, |s| {
                crate::keybindings::assign(&mut s.keybindings, "next_project", "ctrl-alt-j");
            })
        });
        cx.simulate_keystrokes("ctrl-alt-n");
        assert_eq!(shown_project(&workspace, cx).as_ref(), Some(&first), "the old key does nothing now");
        cx.simulate_keystrokes("ctrl-alt-j");
        assert_eq!(shown_project(&workspace, cx).as_ref(), Some(&second), "the new one moves on");
        let saved = crate::settings::Settings::load(&paths);
        assert_eq!(saved.keybindings.get("next_project"), Some(&vec!["ctrl-alt-j".to_string()]));
        let tip = cx.update(|_, cx| crate::keybindings::tooltip("Next", "next_project", cx));
        assert_eq!(tip, "Next (Ctrl+Alt+J)");
    }

    #[gpui_kit::test]
    fn a_shortcut_is_recorded_from_the_keybindings_tab(cx: &mut TestAppContext) {
        let (window, workspace, paths, _) = open(cx);
        let cx = &mut VisualTestContext::from_window(window, cx);
        cx.update(|window, cx| window.render_frame(cx));
        cx.simulate_keystrokes("ctrl-,");
        assert!(cx.update(gpui_kit::component::WindowExt::has_active_dialog), "Ctrl+, opens settings");
        let view = workspace.read_with(cx, |w, _| w.settings_view.clone()).and_then(|v| v.upgrade()).unwrap();
        cx.simulate_keystrokes("ctrl-pagedown ctrl-pagedown");
        // The first row is "Next project"; Enter waits for its new key.
        cx.simulate_keystrokes("enter");
        assert_eq!(view.read_with(cx, |v, _| v.recording()), Some("next_project"));
        cx.simulate_keystrokes("shift-k");
        assert_eq!(
            view.read_with(cx, |v, _| v.recording()),
            Some("next_project"),
            "a key without Ctrl or Alt is refused"
        );
        cx.simulate_keystrokes("ctrl-alt-k");
        assert_eq!(view.read_with(cx, |v, _| v.recording()), None);
        let saved = crate::settings::Settings::load(&paths);
        assert_eq!(saved.keybindings.get("next_project"), Some(&vec!["ctrl-alt-k".to_string()]));
        assert!(
            cx.update(gpui_kit::component::WindowExt::has_active_dialog),
            "the key went to the recorder, not the app"
        );
        // R gives the default back.
        cx.simulate_keystrokes("r");
        assert!(crate::settings::Settings::load(&paths).keybindings.is_empty());
    }

    #[gpui_kit::test]
    fn ctrl_space_finds_a_project_or_a_thread(cx: &mut TestAppContext) {
        let (window, workspace, _, _) = open(cx);
        let cx = &mut VisualTestContext::from_window(window, cx);
        let (project_id, first, second) = two_threads(&workspace, cx);
        cx.update(|window, cx| window.render_frame(cx));
        assert_eq!(workspace.read_with(cx, |w, _| w.active.clone()), Some(second.clone()));

        // The list: the project, then its threads, newest first. The older thread is third.
        cx.simulate_keystrokes("ctrl-space");
        assert!(cx.update(gpui_kit::component::WindowExt::has_active_dialog));
        cx.simulate_keystrokes("down down enter");
        assert!(!cx.update(gpui_kit::component::WindowExt::has_active_dialog), "opening closes it");
        assert_eq!(workspace.read_with(cx, |w, _| w.active.clone()), Some(first), "the thread picked is shown");

        // Picking the project starts a new thread in it.
        cx.simulate_keystrokes("ctrl-space");
        cx.simulate_input("demo");
        cx.simulate_keystrokes("enter");
        let threads = workspace.read_with(cx, |w, _| w.state.project(&project_id).unwrap().threads.len());
        assert_eq!(threads, 3);
    }
}
