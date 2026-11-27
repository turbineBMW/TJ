# TJ roadmap: where it's headed

**Done:** a real Claude backend with approval cards and permission modes (milestone 1); projects and threads in a full-height sidebar, accounts per project with a temporary switch, saved transcripts and resumed sessions, and the window without a title bar (milestone 2).

**Done (milestone 3a):** the Changes list and the diff review with a summary of each file.

**Done (milestone 3b):** the pi-style overview (Goal, Now, Next, Tasks) with context, usage and cost.

**Done (milestone 3c):** following the Omarchy theme live (window and terminal) and the transparent-sidebar setting.

**Done (milestone 3d):** panes and tabs with Vmux's shortcuts, where a tab is a conversation, a terminal or the review; the overview in its own right pane that appears when there's something to show. Saved and restored per project.

**Everything from the original brief is now in.** Possible next steps: mouse selection and copy in the terminal, drag-to-resize splits, more agent backends (Codex or pi, through the `AgentSession` interface), and an `.desktop` file plus a package so TJ shows up in the launcher.

These are steering decisions, not a schedule. Build toward them, but don't let them block the agent core.

## Window layout: like a GNOME app

The model is Rustle and Bubo (`~/Projects/Rustle`, `~/Projects/Bubo`):

- **No title bar.** The window draws its own header (client-side decorations). In gpui that's `WindowOptions { titlebar: None, window_decorations: Some(WindowDecorations::Client), .. }`, plus our own drag area and window controls.
- **The sidebar runs the full height of the window**, up to the top edge, with its own header strip. The content area has a separate header beside it, like libadwaita's split view.
- **Optional transparent sidebar**, using the same setting as the rest of the app family: an in-app switch (off by default) plus an opacity slider (default 0.8). The Omarchy theme never decides transparency. This needs a transparent window background (`window_background: WindowBackgroundAppearance::Transparent`), with every surface except the sidebar painted opaque.
- Follow the Omarchy theme live. Bubo's `src/omarchy/` palette code is the reference to port.

## Terminals: panes and tabs like Vmux/tmux

- The main area has **tabs** in the top bar, and each tab can **split right** or **split down** into its own panes. A tab is titled after its focused pane. Closing a pane folds its split; closing a tab's last pane closes the tab.
- Use the same shortcuts as Vmux (`~/Projects/Vmux/src/keybinds.rs`):

  | Keys | Action |
  |---|---|
  | `Ctrl+T` / `Ctrl+W` (in a terminal, `Ctrl+Shift+T` / `Ctrl+Shift+W`) | New tab (same directory) / close tab |
  | `Ctrl+Alt+H` / `Ctrl+Alt+V` | Split right / down |
  | `Ctrl+Alt+S/F/E/D` or `Ctrl+Alt+←/→/↑/↓` | Focus pane left / right / up / down |
  | `Ctrl+Shift+A` | Focus next pane |
  | `Ctrl+Shift+=` / `Ctrl+Shift+-` | Grow / shrink pane |
  | `Ctrl+PageDown` / `Ctrl+PageUp` | Next / previous tab |
  | `Ctrl+Shift+PageDown` / `Ctrl+Shift+PageUp` | Move tab right / left |

- Layout, tabs and working directories should persist per project.

## Keyboard rules

- **Keyboard first.** TJ is a GUI, but everything in it must be doable from the keyboard alone: every button, card, menu, list and dialog needs a key path, and a feature isn't finished until it has one. The mouse is a convenience, never the only way. Show the keys where they apply (the question modal's hint line is the model). TODO.md lists what still needs the mouse.
- **Tab never moves focus.** Terminals (and the prompt box) use Tab. Move focus with explicit shortcuts such as the pane-focus keys above.
- gpui runs key bindings before a view's key handler. Any binding an outer view makes that a terminal needs must be unbound in the `Terminal` key context, via `terminal::PASSTHROUGH_KEYS` and `NoAction`. gpui-kit's root view binds Tab, Shift+Tab and Ctrl+C, and TJ binds Ctrl+T and Ctrl+W to tabs. The test `tab_and_ctrl_c_reach_the_shell` guards this.
- App shortcuts use `Ctrl+Shift` or `Ctrl+Alt`, so plain `Ctrl+letter` always reaches the shell. The one exception is browser-style `Ctrl+T` / `Ctrl+W` for tabs: a focused terminal passes them to its shell (transpose, delete a word), and the `Ctrl+Shift` spellings work everywhere.

## Agent features (from the original brief)

- Projects first, threads under each project.
- A default agent and account per project, with a temporary switch on a thread (one `CLAUDE_CONFIG_DIR` per account).
- A sidebar like pi's: usage (cost for API accounts, % for subscriptions), plus Goal, Now, Next and Tasks.
- Files added, modified and deleted. Click one to open a diff of every file, with a summary of each file written by the agent.
