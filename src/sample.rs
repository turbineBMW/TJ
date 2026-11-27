//! Canned agent output for the streaming spike: a long markdown reply with the
//! shapes a real coding-agent answer has (headings, lists, code, a table).

pub const USER_PROMPT: &str =
    "Can you add per-project default accounts to the session launcher, and explain how switching works?";

pub const REPLY: &str = r#"## Plan

I read `src/session/launcher.rs` and `src/config/project.rs`. Right now every session starts with the global account, because `Launcher::spawn` reads `Settings::default_account` and never looks at the project.

Here's what I'll change:

1. Add an optional `account` field to `ProjectConfig`, stored in `.tj/project.toml`.
2. Resolve the account in this order: **thread override → project default → global default**.
3. Keep a *temporary switch* on the thread only, so it never rewrites the project file.
4. Show the resolved account in the sidebar, with a marker when it's an override.

### Config shape

```toml
# .tj/project.toml
name = "Vord"
agent = "claude"
account = "personal"   # matches a key in ~/.config/tj/accounts.toml

[models]
default = "claude-opus-5-5"
cheap = "claude-haiku-5-5"
```

Accounts live outside the repo, so nothing secret gets committed:

```toml
# ~/.config/tj/accounts.toml
[personal]
kind = "claude-subscription"
config_dir = "~/.claude_personal"

[work]
kind = "claude-subscription"
config_dir = "~/.claude"
```

### Resolution

```rust
pub fn resolve_account<'a>(
    thread: &'a Thread,
    project: &'a ProjectConfig,
    settings: &'a Settings,
) -> &'a AccountId {
    thread
        .account_override
        .as_ref()
        .or(project.account.as_ref())
        .unwrap_or(&settings.default_account)
}
```

Each account maps to its own `CLAUDE_CONFIG_DIR`, so the spawned process sees only that login:

```rust
let mut cmd = Command::new("claude");
cmd.env("CLAUDE_CONFIG_DIR", account.config_dir())
    .args(["--input-format", "stream-json", "--output-format", "stream-json"])
    .current_dir(&project.root);
```

## How switching works

A Claude session belongs to the account that started it: the conversation state is stored under that account's config directory. So a switch **can't move a live session**. Instead, a temporary switch does this:

- Ends the current turn cleanly (it waits if a tool call is running).
- Asks the cheap model for a hand-off summary: goal, decisions, open tasks, files touched.
- Starts a new session on the other account with that summary as its first message.
- Links the two sessions in the thread, so the transcript reads as one conversation.

Switching back works the same way. The project default never changes unless you pick **Make default for project**.

| Action | Changes thread | Changes project file | Starts new session |
|---|---|---|---|
| Temporary switch | yes | no | yes |
| Make default for project | no | yes | no |
| Change global default | no | no | no |

### Edge cases I checked

- **Missing account.** If `project.toml` names an account that isn't in `accounts.toml`, the launcher falls back to the global default and shows a warning in the sidebar instead of failing.
- **Logged-out account.** The first spawn fails with an auth error. I surface that as a *Log in* button that runs `claude auth login` with the right `CLAUDE_CONFIG_DIR` in the built-in terminal.
- **API-key accounts.** These report cost in dollars, while subscription accounts report a percentage of the usage window. The sidebar shows whichever one the account has.

## Verification

```bash
cargo test -p tj-session resolve_account
cargo run -- --project ~/Projects/Vord
```

I added four tests to `launcher_tests.rs`:

- `thread_override_wins`
- `project_default_beats_global`
- `missing_account_falls_back`
- `switch_keeps_project_file_unchanged`

All four pass. I didn't touch the existing `spawn_*` tests; they still pass unchanged.

> **Note:** the hand-off summary costs one cheap-model call per switch, about a cent on an API account. Subscription accounts don't see a charge, but it does count toward the usage window.

---

## 80/20

✅ Result: **Per-thread account switching works**; four new tests pass.
❓ Decide: **Wire Make default for project next, or the sidebar marker first?**
⚠️ Watch out: each switch makes **one cheap-model call** for the hand-off summary.
🔀 Git: changes are on **account-switching**, not committed.
"#;
