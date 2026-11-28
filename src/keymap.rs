//! App-wide key binding rules. See ROADMAP.md, "Keyboard rules".

use gpui_kit::{App, KeyBinding, NoAction};

/// gpui-kit's root view binds Tab and Shift-Tab to move focus. TJ never moves focus with
/// Tab (terminals and the prompt box use it), so those bindings are removed here.
/// A later binding in the same context takes precedence, so call this after `gpui_kit::init`,
/// and after the settings are loaded: the app shortcuts take their keys from them.
pub fn init(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("tab", NoAction, Some("Root")),
        KeyBinding::new("shift-tab", NoAction, Some("Root")),
    ]);
    crate::terminal::init(cx);
    // Answering Claude's questions in their modal, and moving around the settings modal.
    cx.bind_keys(crate::thread::question_bindings());
    cx.bind_keys(crate::settings_view::bindings());
    // Moving through pull requests and sections in the GitHub tab.
    cx.bind_keys(crate::github_view::bindings());
    // Commenting on lines of the review and sending the comments to the agent.
    cx.bind_keys(crate::changes::bindings());
    // Moving through notes and acting on them in the Notes tab.
    cx.bind_keys(crate::notes_view::bindings());
    // Moving through tracked pull requests and acting on them in the PRs tab.
    cx.bind_keys(crate::prs_view::bindings());
    // The shortcuts settings can change: Vmux's pane and tab keys inside the main area, and the
    // window-wide ones (projects, the switcher, settings, the sidebar).
    crate::keybindings::apply(cx);
}
