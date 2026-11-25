//! App-wide key binding rules. See ROADMAP.md, "Keyboard rules".

use gpui_kit::{App, KeyBinding, NoAction};

/// gpui-kit's root view binds Tab and Shift-Tab to move focus. TJ never moves focus with
/// Tab (terminals and the prompt box use it), so those bindings are removed here.
/// A later binding in the same context takes precedence, so call this after `gpui_kit::init`.
pub fn init(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("tab", NoAction, Some("Root")),
        KeyBinding::new("shift-tab", NoAction, Some("Root")),
    ]);
    crate::terminal::init(cx);
    // Vmux's pane and tab shortcuts, inside the terminal area only.
    cx.bind_keys(crate::panes::bindings());
    // Answering Claude's questions in their modal.
    cx.bind_keys(crate::thread::question_bindings());
}
