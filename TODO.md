# TODO

What's missing or rough, roughly in priority order. ROADMAP.md has the direction and design rules; this is the working list.

## Agent

- [x] **Claude's questions and plans.** AskUserQuestion opens a modal over the conversation, answered question by question from the keyboard (or mouse); ExitPlanMode shows the plan with Start, accepting edits / Start, asking first / Keep planning.
- [x] **Subagent activity.** The Agent tool's row shows what its (background) subagent is doing, its tool calls, counts and report.
- [x] **Model picker.** Model and effort from a dropdown next to Send, listed by `claude` for the account, switchable mid-session, with project defaults.
- [x] **Attachments and `@file` mentions** in the prompt box: an `@` file picker with chips, and images pasted, dropped or picked.
- [x] **Account-switch handoff** sends a summary for long conversations (word for word when short), and survives a restart.
- [ ] **Other agent backends** (Codex, pi) through the `AgentSession` interface.

## Keyboard

TJ should work entirely from the keyboard (ROADMAP.md, "Keyboard rules"). Still mouse-only:

- [ ] Approval cards: Approve, Approve for session, Decline, Cancel turn.
- [ ] The plan card: Start, accepting edits / Start, asking first / Keep planning.
- [x] Moving between projects (Ctrl+Alt+N/P), a new thread (Ctrl+Shift+O), and finding any project or thread (Ctrl+Space).
- [ ] Moving through the threads in the sidebar itself, and opening a project's history.
- [ ] The account, mode and model menus under the prompt box.
- [ ] Thread and project right-click menus (rename, archive, delete, remove, project picture).
- [x] Commenting on lines of the review and sending the comments to the agent (Shift+↑/↓, Enter, Ctrl+Enter).
- [ ] Opening and folding tool cards, opening a long Bash card ("Click to expand"), adding a card to the prompt, and opening the review from the changes list.
- [x] Settings: a modal driven from the keyboard, with every app shortcut changeable.

## Threads and projects

- [x] Rename, archive and delete threads (right-click menu; double-click renames).
- [x] Remove a project (right-click its name; the folder is not touched).

## Terminal and panes

- [x] Mouse selection and copy in the terminal (drag, double- and triple-click; primary selection, Ctrl+C copies a selection, middle-click paste).
- [x] Mouse reporting for terminal apps (vim, htop, less): clicks, drags, motion and the wheel; Shift selects instead.
- [x] Send a terminal selection to the agent as a chip (Ctrl+Alt+A, or the terminal's right-click menu).
- [x] Smooth cursor, in the terminal and the text fields: glides, fades as it blinks, optional trail; speed and trail in settings.
- [x] Inline images: kitty graphics, SIXEL and iTerm2 (no kitty animation or relative placements).
- [x] Drag to resize splits (the divider between panes; double-click centres it).
- [x] Tabs in the top bar hold their own splits: a split stays in its tab, and the tab is titled after its focused pane.

## Distribution

- [x] A `.desktop` file, icon and `install.sh` / `uninstall.sh`, like Bubo, so TJ shows up in the launcher.
- [ ] A PKGBUILD, if TJ should install as a pacman package.

## Verified

- [x] Following an Omarchy theme switch live.
- [x] Both accounts work (Personal and Work).
