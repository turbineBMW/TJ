# TODO

What's missing or rough, roughly in priority order. ROADMAP.md has the direction and design rules; this is the working list.

## Agent

- [x] **Claude's questions and plans.** AskUserQuestion has its own card (pick options, multi-select, or type an answer); ExitPlanMode shows the plan with Start, accepting edits / Start, asking first / Keep planning.
- [x] **Subagent activity.** The Agent tool's row shows what its (background) subagent is doing, its tool calls, counts and report.
- [x] **Model picker.** Model and effort from a dropdown next to Send, listed by `claude` for the account, switchable mid-session, with project defaults.
- [x] **Attachments and `@file` mentions** in the prompt box: an `@` file picker with chips, and images pasted, dropped or picked.
- [x] **Account-switch handoff** sends a summary for long conversations (word for word when short), and survives a restart.
- [ ] **Other agent backends** (Codex, pi) through the `AgentSession` interface.

## Threads and projects

- [x] Rename, archive and delete threads (right-click menu; double-click renames).
- [x] Remove a project (right-click its name; the folder is not touched).

## Terminal and panes

- [x] Mouse selection and copy in the terminal (drag, double- and triple-click; primary selection, Ctrl+Shift+C, middle-click paste).
- [x] Mouse reporting for terminal apps (vim, htop, less): clicks, drags, motion and the wheel; Shift selects instead.
- [x] Smooth cursor, in the terminal and the text fields: glides, fades as it blinks, optional trail; speed and trail in settings.
- [ ] Images (SIXEL/kitty), as in the vte fork.
- [x] Drag to resize splits (the divider between panes; double-click centres it).

## Distribution

- [x] A `.desktop` file, icon and `install.sh` / `uninstall.sh`, like Bubo, so TJ shows up in the launcher.
- [ ] A PKGBUILD, if TJ should install as a pacman package.

## Verified

- [x] Following an Omarchy theme switch live.
- [x] Both accounts work (Personal and Work).
