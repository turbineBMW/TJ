# TJ roadmap: where it's headed

**Done:** a real Claude backend with approval cards and permission modes (milestone 1); projects and threads in a full-height sidebar, accounts per project with a temporary switch, saved transcripts and resumed sessions, and the window without a title bar (milestone 2).

**Done (milestone 3a):** the Changes list and the diff review with a summary of each file.

**Next:** the pi-style task panel, then the transparent-sidebar setting and following the Omarchy theme, then terminal splits and tabs.

These are steering decisions, not a schedule. Build toward them, but don't let them block the agent core.

## Window layout: like a GNOME app

The model is Rustle and Bubo (`~/Projects/Rustle`, `~/Projects/Bubo`):

- **No title bar.** The window draws its own header (client-side decorations). In gpui that's `WindowOptions { titlebar: None, window_decorations: Some(WindowDecorations::Client), .. }`, plus our own drag area and window controls.
- **The sidebar runs the full height of the window**, up to the top edge, with its own header strip. The content area has a separate header beside it, like libadwaita's split view.
- **Optional transparent sidebar**, using the same setting as the rest of the app family: an in-app switch (off by default) plus an opacity slider (default 0.8). The Omarchy theme never decides transparency. This needs a transparent window background (`window_background: WindowBackgroundAppearance::Transparent`), with every surface except the sidebar painted opaque.
- Follow the Omarchy theme live. Bubo's `src/omarchy/` palette code is the reference to port.

## Terminals: panes and tabs like Vmux/tmux

- A terminal area can **split right**, **split down**, or **add a tab**. Every pane has its own tab strip. Closing a pane's last tab folds the split back up.
- Use the same shortcuts as Vmux (`~/Projects/Vmux/src/keybinds.rs`):

  | Keys | Action |
  |---|---|
  | `Ctrl+Shift+T` / `Ctrl+Shift+W` | New tab (same directory) / close tab |
  | `Ctrl+Shift+E` / `Ctrl+Shift+O` | Split right / down |
  | `Ctrl+Alt+←/→/↑/↓` | Focus pane in that direction |
  | `Ctrl+Shift+A` | Focus next pane |
  | `Ctrl+Shift+=` / `Ctrl+Shift+-` | Grow / shrink pane |
  | `Ctrl+PageDown` / `Ctrl+PageUp` | Next / previous tab |

- Layout, tabs and working directories should persist per project.

## Keyboard rules

- **Tab never moves focus.** Terminals (and the prompt box) use Tab. Move focus with explicit shortcuts such as the pane-focus keys above.
- gpui runs key bindings before a view's key handler. Any binding an outer view makes that a terminal needs must be unbound in the `Terminal` key context, via `terminal::PASSTHROUGH_KEYS` and `NoAction`. gpui-kit's root view binds Tab, Shift+Tab and Ctrl+C. The test `tab_and_ctrl_c_reach_the_shell` guards this.
- App shortcuts use `Ctrl+Shift` or `Ctrl+Alt`, so plain `Ctrl+letter` always reaches the shell.

## Agent features (from the original brief)

- Projects first, threads under each project.
- A default agent and account per project, with a temporary switch on a thread (one `CLAUDE_CONFIG_DIR` per account).
- A sidebar like pi's: usage (cost for API accounts, % for subscriptions), plus Goal, Now, Next and Tasks.
- Files added, modified and deleted. Click one to open a diff of every file, with a summary of each file written by the agent.
