# TJ

## Before every commit

CI runs these three in order and stops at the first failure, so run all of them
before committing and fix anything they report:

```bash
cargo fmt
```

```bash
cargo clippy --all-targets -- -D warnings
```

```bash
cargo test
```

`rustfmt.toml` sets `max_width = 120` and `use_small_heuristics = "Max"`, so
rustfmt often joins builder chains onto one line. Don't hand-format them; let
`cargo fmt` decide, or `cargo fmt --check` fails in CI.
