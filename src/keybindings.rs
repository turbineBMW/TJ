//! TJ's app shortcuts: every command that has a key, its default keys, and the keys settings
//! give it instead. Settings keep only the changes (`[keybindings]` in `settings.toml`), so a
//! command you never touched follows its default. [`apply`] rebuilds gpui's keymap from them,
//! so a change takes effect at once.
//!
//! Keys are written as gpui writes them: `ctrl-alt-n`, `ctrl-shift-=`, `ctrl-space`.

use std::collections::BTreeMap;
use std::rc::Rc;

use gpui_kit::{Action, App, KeyBinding, KeyBindingContextPredicate, Keystroke};

use crate::panes;
use crate::settings::Settings;
use crate::workspace;

/// Where a command sits in the Keybindings tab.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Group {
    Projects,
    Window,
    Panes,
}

impl Group {
    pub const ALL: [Group; 3] = [Group::Projects, Group::Window, Group::Panes];

    pub fn title(self) -> &'static str {
        match self {
            Group::Projects => "Projects and threads",
            Group::Window => "Window",
            Group::Panes => "Panes and tabs",
        }
    }
}

pub struct Command {
    /// Its name in `settings.toml`.
    pub id: &'static str,
    pub label: &'static str,
    pub group: Group,
    /// The key context it's bound in.
    context: &'static str,
    defaults: &'static [&'static str],
    action: fn() -> Box<dyn Action>,
}

macro_rules! command {
    ($id:literal, $label:literal, $group:ident, $context:expr, [$($key:literal),*], $action:expr) => {
        Command {
            id: $id,
            label: $label,
            group: Group::$group,
            context: $context,
            defaults: &[$($key),*],
            action: || Box::new($action),
        }
    };
}

const WS: &str = workspace::CONTEXT;
const PANES: &str = panes::CONTEXT;

/// Every command with a shortcut, in the order the Keybindings tab lists them.
pub static COMMANDS: &[Command] = &[
    command!("next_project", "Next project", Projects, WS, ["ctrl-alt-n"], workspace::NextProject),
    command!("previous_project", "Previous project", Projects, WS, ["ctrl-alt-p"], workspace::PreviousProject),
    command!("new_thread", "New thread in this project", Projects, WS, ["ctrl-shift-o"], workspace::NewThread),
    command!("focus_agent", "Focus the agent's prompt box", Projects, WS, ["ctrl-shift-l"], workspace::FocusAgent),
    command!("find", "Find a project or thread", Projects, WS, ["ctrl-space"], workspace::OpenSwitcher),
    command!("open_github", "Open the project's GitHub tab", Projects, WS, ["ctrl-shift-g"], workspace::OpenGitHub),
    command!("open_notes", "Open the project's notes", Projects, WS, ["ctrl-shift-n"], workspace::OpenNotes),
    command!(
        "open_markdown",
        "Open the Markdown file Claude mentioned last",
        Projects,
        WS,
        ["ctrl-shift-m"],
        workspace::OpenMarkdown
    ),
    command!(
        "open_activity",
        "Open what's running in the background, or the latest background shell or subagent",
        Projects,
        WS,
        ["ctrl-shift-j"],
        workspace::OpenActivity
    ),
    command!(
        "send_to_agent",
        "Send the terminal's selection to the agent",
        Projects,
        crate::terminal::KEY_CONTEXT,
        ["ctrl-alt-a"],
        crate::terminal::SendToAgent
    ),
    command!("settings", "Open settings", Window, WS, ["ctrl-,"], workspace::OpenSettings),
    command!("toggle_sidebar", "Show or hide the sidebar", Window, WS, ["ctrl-shift-s"], workspace::ToggleSidebar),
    // Ctrl+T and Ctrl+W, like a browser. A terminal keeps them for its shell (transpose, delete a
    // word), so the Shift spellings work there too.
    command!("new_tab", "New terminal tab", Panes, PANES, ["ctrl-t", "ctrl-shift-t"], panes::NewTab),
    command!(
        "close_tab",
        "Close the pane (its tab, with the last)",
        Panes,
        PANES,
        ["ctrl-w", "ctrl-shift-w"],
        panes::CloseTab
    ),
    command!("next_tab", "Next tab", Panes, PANES, ["ctrl-pagedown"], panes::NextTab),
    command!("previous_tab", "Previous tab", Panes, PANES, ["ctrl-pageup"], panes::PreviousTab),
    command!("move_tab_right", "Move the tab right", Panes, PANES, ["ctrl-shift-pagedown"], panes::MoveTabRight),
    command!("move_tab_left", "Move the tab left", Panes, PANES, ["ctrl-shift-pageup"], panes::MoveTabLeft),
    command!("split_right", "Split right", Panes, PANES, ["ctrl-alt-h"], panes::SplitRight),
    command!("split_down", "Split down", Panes, PANES, ["ctrl-alt-v"], panes::SplitDown),
    command!("focus_next_pane", "Focus the next pane", Panes, PANES, ["ctrl-shift-a"], panes::FocusNextPane),
    command!(
        "focus_pane_left",
        "Focus the pane to the left",
        Panes,
        PANES,
        ["ctrl-alt-s", "ctrl-alt-left"],
        panes::FocusPaneLeft
    ),
    command!(
        "focus_pane_right",
        "Focus the pane to the right",
        Panes,
        PANES,
        ["ctrl-alt-f", "ctrl-alt-right"],
        panes::FocusPaneRight
    ),
    command!("focus_pane_up", "Focus the pane above", Panes, PANES, ["ctrl-alt-e", "ctrl-alt-up"], panes::FocusPaneUp),
    command!(
        "focus_pane_down",
        "Focus the pane below",
        Panes,
        PANES,
        ["ctrl-alt-d", "ctrl-alt-down"],
        panes::FocusPaneDown
    ),
    // Shift turns `=` into `+` and `-` into `_` on most layouts; either spelling grows or shrinks.
    command!("grow_pane", "Grow the pane", Panes, PANES, ["ctrl-shift-=", "ctrl-shift-+"], panes::GrowPane),
    command!("shrink_pane", "Shrink the pane", Panes, PANES, ["ctrl-shift--", "ctrl-shift-_"], panes::ShrinkPane),
];

pub fn command(id: &str) -> Option<&'static Command> {
    COMMANDS.iter().find(|c| c.id == id)
}

impl Command {
    pub fn defaults(&self) -> Vec<String> {
        self.defaults.iter().map(|k| normalize(k).unwrap_or_else(|| k.to_string())).collect()
    }

    /// The command's keys under `overrides`: its own entry there, else its defaults.
    pub fn keys(&self, overrides: &BTreeMap<String, Vec<String>>) -> Vec<String> {
        overrides.get(self.id).cloned().unwrap_or_else(|| self.defaults())
    }

    pub fn is_default(&self, overrides: &BTreeMap<String, Vec<String>>) -> bool {
        !overrides.contains_key(self.id)
    }
}

fn overrides(cx: &App) -> BTreeMap<String, Vec<String>> {
    cx.try_global::<Settings>().map(|s| s.keybindings.clone()).unwrap_or_default()
}

/// The command's first key, for a tooltip or hint: "Ctrl+Alt+N".
pub fn label(id: &str, cx: &App) -> Option<String> {
    command(id)?.keys(&overrides(cx)).first().map(|k| display(k))
}

/// `text`, followed by the command's key in brackets when it has one.
pub fn tooltip(text: &str, id: &str, cx: &App) -> String {
    match label(id, cx) {
        Some(key) => format!("{text} ({key})"),
        None => text.to_string(),
    }
}

/// Rebuild gpui's keymap: every binding that isn't one of [`COMMANDS`] stays as it was (in its
/// order), and the commands get their keys from settings. Call after `gpui_kit::init`, and
/// again whenever the keybindings in settings change.
pub fn apply(cx: &mut App) {
    let overrides = overrides(cx);
    let ours: Vec<&'static str> = COMMANDS.iter().map(|c| (c.action)().name()).collect();
    let mut bindings: Vec<KeyBinding> = {
        let keymap = cx.key_bindings();
        let keymap = keymap.borrow();
        keymap.bindings().filter(|b| !ours.contains(&b.action().name())).cloned().collect()
    };
    for command in COMMANDS {
        let context = KeyBindingContextPredicate::parse(command.context).ok().map(Rc::new);
        for key in command.keys(&overrides) {
            match KeyBinding::load(&key, (command.action)(), context.clone(), false, None, cx) {
                Ok(binding) => bindings.push(binding),
                Err(err) => eprintln!("tj: skipping the key {key:?} for {}: {err}", command.id),
            }
        }
    }
    cx.clear_key_bindings();
    cx.bind_keys(bindings);
}

// ---------- changing keys (pure, on the overrides) ----------

/// Give `id` the single key `key`. Any other command holding that key loses it; their ids are
/// returned, so the caller can say where it came from.
pub fn assign(overrides: &mut BTreeMap<String, Vec<String>>, id: &str, key: &str) -> Vec<&'static str> {
    let taken = take_from_others(overrides, id, &[key.to_string()]);
    set(overrides, id, vec![key.to_string()]);
    taken
}

/// Give `id` its default keys back, taking them from any command that has since been given one.
pub fn reset(overrides: &mut BTreeMap<String, Vec<String>>, id: &str) -> Vec<&'static str> {
    let Some(command) = command(id) else { return Vec::new() };
    let taken = take_from_others(overrides, id, &command.defaults());
    overrides.remove(id);
    taken
}

/// Leave `id` without a key.
pub fn clear(overrides: &mut BTreeMap<String, Vec<String>>, id: &str) {
    set(overrides, id, Vec::new());
}

fn set(overrides: &mut BTreeMap<String, Vec<String>>, id: &str, keys: Vec<String>) {
    let Some(command) = command(id) else { return };
    if keys == command.defaults() {
        overrides.remove(id);
    } else {
        overrides.insert(id.to_string(), keys);
    }
}

fn take_from_others(overrides: &mut BTreeMap<String, Vec<String>>, id: &str, keys: &[String]) -> Vec<&'static str> {
    let mut taken = Vec::new();
    for other in COMMANDS.iter().filter(|c| c.id != id) {
        let had = other.keys(overrides);
        let kept: Vec<String> = had.iter().filter(|k| !keys.contains(k)).cloned().collect();
        if kept.len() != had.len() {
            set(overrides, other.id, kept);
            taken.push(other.id);
        }
    }
    taken
}

// ---------- keys as text ----------

/// The key a pressed keystroke stands for, written the way bindings are (`ctrl-alt-n`), or
/// `None` for a modifier on its own.
pub fn from_keystroke(keystroke: &Keystroke) -> Option<String> {
    if keystroke.is_modifier_key() || keystroke.key.is_empty() {
        return None;
    }
    let m = keystroke.modifiers;
    Some(join(m.control, m.alt, m.shift, m.platform, &keystroke.key))
}

/// Why `key` can't be a shortcut, if it can't: it needs Ctrl, Alt or Super, unless it's a
/// function key, or typing in the prompt box would set it off.
pub fn unusable(key: &str) -> Option<&'static str> {
    let (ctrl, alt, _, sup, k) = split(key)?;
    let function_key = k.len() >= 2 && k.starts_with('f') && k[1..].chars().all(|c| c.is_ascii_digit());
    (!(ctrl || alt || sup || function_key)).then_some("Hold Ctrl, Alt or Super with the key, so typing still works.")
}

/// Whether the shell would miss `key`: plain Ctrl plus a letter is the shell's (Ctrl+C,
/// Ctrl+R...), and ROADMAP.md keeps app shortcuts off those, except the few a terminal keeps
/// for itself while it has focus (Ctrl+T, Ctrl+W).
pub fn shadows_shell(key: &str) -> bool {
    let ctrl_letter = matches!(split(key), Some((true, false, false, false, k)) if k.len() == 1 && k.chars().all(|c| c.is_ascii_alphabetic()));
    ctrl_letter && !normalize(key).is_some_and(|k| crate::terminal::PASSTHROUGH_KEYS.contains(&k.as_str()))
}

/// `key` in one spelling: modifiers in a fixed order, lower case. `None` if it isn't a key.
pub fn normalize(key: &str) -> Option<String> {
    let (ctrl, alt, shift, sup, k) = split(key)?;
    Some(join(ctrl, alt, shift, sup, &k))
}

/// How a key is shown: "Ctrl+Alt+N", "Ctrl+Shift+=", "Ctrl+Space", "Ctrl+Alt+←".
pub fn display(key: &str) -> String {
    let Some((ctrl, alt, shift, sup, k)) = split(key) else { return key.to_string() };
    let mut parts: Vec<String> = Vec::new();
    for (on, name) in [(ctrl, "Ctrl"), (alt, "Alt"), (shift, "Shift"), (sup, "Super")] {
        if on {
            parts.push(name.into());
        }
    }
    let named = match k.as_str() {
        "left" => "←",
        "right" => "→",
        "up" => "↑",
        "down" => "↓",
        "space" => "Space",
        "enter" => "Enter",
        "escape" => "Esc",
        "tab" => "Tab",
        "backspace" => "Backspace",
        "delete" => "Delete",
        "insert" => "Insert",
        "home" => "Home",
        "end" => "End",
        "pageup" => "PageUp",
        "pagedown" => "PageDown",
        _ => "",
    };
    parts.push(if !named.is_empty() {
        named.to_string()
    } else if k.len() >= 2 && k.starts_with('f') && k[1..].chars().all(|c| c.is_ascii_digit()) {
        k.to_uppercase()
    } else {
        let mut chars = k.chars();
        chars.next().map(|c| c.to_uppercase().chain(chars).collect()).unwrap_or_default()
    });
    parts.join("+")
}

/// (ctrl, alt, shift, super, key). A trailing `-` is the minus key, as gpui reads it.
fn split(key: &str) -> Option<(bool, bool, bool, bool, String)> {
    let key = key.trim().to_lowercase();
    if key.is_empty() || key.contains(char::is_whitespace) {
        return None;
    }
    let (mods, k) = if let Some(mods) = key.strip_suffix("--") {
        (mods, "-".to_string())
    } else if key == "-" {
        ("", "-".to_string())
    } else {
        match key.rsplit_once('-') {
            Some((mods, k)) => (mods, k.to_string()),
            None => ("", key.clone()),
        }
    };
    if k.is_empty() {
        return None;
    }
    let (mut ctrl, mut alt, mut shift, mut sup) = (false, false, false, false);
    for m in mods.split('-').filter(|m| !m.is_empty()) {
        match m {
            "ctrl" | "control" => ctrl = true,
            "alt" => alt = true,
            "shift" => shift = true,
            "super" | "cmd" | "win" | "platform" => sup = true,
            _ => return None,
        }
    }
    Some((ctrl, alt, shift, sup, k))
}

fn join(ctrl: bool, alt: bool, shift: bool, sup: bool, key: &str) -> String {
    let mut out = String::new();
    for (on, name) in [(ctrl, "ctrl-"), (alt, "alt-"), (shift, "shift-"), (sup, "super-")] {
        if on {
            out.push_str(name);
        }
    }
    out.push_str(&key.to_lowercase());
    out
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::{COMMANDS, assign, clear, command, display, normalize, reset, shadows_shell, unusable};

    #[test]
    fn every_command_is_unique_and_its_defaults_parse() {
        let mut ids: Vec<_> = COMMANDS.iter().map(|c| c.id).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), COMMANDS.len(), "ids are unique");
        let mut keys: Vec<String> = COMMANDS.iter().flat_map(|c| c.defaults()).collect();
        assert!(keys.iter().all(|k| normalize(k).as_ref() == Some(k)), "defaults are written normally: {keys:?}");
        let count = keys.len();
        keys.sort();
        keys.dedup();
        assert_eq!(keys.len(), count, "no two commands share a default key");
        assert!(keys.iter().all(|k| unusable(k).is_none() && !shadows_shell(k)), "every default keeps to the rules");
    }

    #[test]
    fn keys_are_shown_and_written_one_way() {
        assert_eq!(display("ctrl-alt-n"), "Ctrl+Alt+N");
        assert_eq!(display("ctrl-space"), "Ctrl+Space");
        assert_eq!(display("ctrl-shift--"), "Ctrl+Shift+-");
        assert_eq!(display("ctrl-alt-left"), "Ctrl+Alt+←");
        assert_eq!(display("ctrl-,"), "Ctrl+,");
        assert_eq!(display("f5"), "F5");
        assert_eq!(normalize("Shift-Ctrl-N").as_deref(), Some("ctrl-shift-n"));
        assert_eq!(normalize("ctrl-shift--").as_deref(), Some("ctrl-shift--"));
        assert_eq!(normalize("hyper-x"), None);
        assert!(unusable("shift-a").is_some());
        assert!(unusable("f2").is_none() && unusable("alt-x").is_none());
        assert!(shadows_shell("ctrl-r") && !shadows_shell("ctrl-alt-r") && !shadows_shell("ctrl-space"));
        assert!(!shadows_shell("ctrl-w") && !shadows_shell("Ctrl-T"), "a terminal keeps these for its shell");
    }

    #[test]
    fn assigning_a_key_takes_it_from_the_command_that_had_it() {
        let mut overrides = BTreeMap::new();
        let taken = assign(&mut overrides, "find", "ctrl-alt-n");
        assert_eq!(taken, vec!["next_project"]);
        assert_eq!(command("find").unwrap().keys(&overrides), vec!["ctrl-alt-n"]);
        assert!(command("next_project").unwrap().keys(&overrides).is_empty());

        // Resetting gives the default back and takes it from whoever has it now.
        assert_eq!(reset(&mut overrides, "next_project"), vec!["find"]);
        assert!(command("next_project").unwrap().is_default(&overrides));
        assert!(command("find").unwrap().keys(&overrides).is_empty());

        // A command given its own default again keeps no entry.
        assign(&mut overrides, "find", "ctrl-space");
        assert!(overrides.is_empty(), "{overrides:?}");

        clear(&mut overrides, "grow_pane");
        assert_eq!(overrides.get("grow_pane"), Some(&Vec::new()));
    }
}
