# TODO

What's missing or rough, roughly in priority order. ROADMAP.md has the direction and design rules; this is the working list.

## Agent

- [x] **Claude's questions and plans.** AskUserQuestion has its own card (pick options, multi-select, or type an answer); ExitPlanMode shows the plan with Start, accepting edits / Start, asking first / Keep planning.
- [x] **Subagent activity.** The Agent tool's row shows what its (background) subagent is doing, its tool calls, counts and report.
- [x] **Model picker.** Model and effort from a dropdown next to Send, listed by `claude` for the account, switchable mid-session, with project defaults.
- [ ] **Attachments and `@file` mentions** in the prompt box.
- [ ] **Account-switch handoff** sends the raw transcript (up to 30k characters); a summary would be shorter and cheaper.
- [ ] **Other agent backends** (Codex, pi) through the `AgentSession` interface.

## Threads and projects

- [ ] Rename, archive and delete threads.
- [ ] Remove a project.

## Terminal and panes

- [ ] Mouse selection and copy in the terminal.
- [ ] Mouse reporting for terminal apps (vim, htop, less).
- [ ] Blinking cursor.
- [ ] Images (SIXEL/kitty), as in the vte fork.
- [ ] Drag to resize splits (today: Ctrl+Shift+= and Ctrl+Shift+-).

## Distribution

- [ ] A `.desktop` file and a package (PKGBUILD and install script), like the rest of the app family, so TJ shows up in the launcher.

## Verified

- [x] Following an Omarchy theme switch live.
- [x] Both accounts work (Personal and Work).
