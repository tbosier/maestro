# Contributing

## Setup

```sh
cargo build
cargo test --workspace
```

In a sandbox where `~/.cargo` is read-only, point `CARGO_HOME` at a writable directory (the repo ignores `.cargo-home/`):

```sh
export CARGO_HOME=$PWD/.cargo-home
```

## Gates

Every change must pass all three before it merges to `main`:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

CI (`.github/workflows/ci.yml`) runs the same commands on Linux, checks that the workspace builds with the minimum Rust version (1.88), and runs the tests on macOS as a job that is not required yet: nothing has been checked on macOS by hand.

## Pull requests and releases

Every change reaches `main` through a pull request that fills in the template and passes CI. After a pull request merges, `main` is tagged: a new minor version (`v0.2.0`) for a pull request that adds a feature, a new patch version (`v0.1.1`) for fixes and documentation.

Changes that affect what you see or how input behaves also need an interactive check in a real terminal. The pattern used so far:

```sh
tmux -f /dev/null new-session -d -s t -x 100 -y 20 "./target/release/maestro file.rs"
tmux send-keys -t t j 'f"' 'mi"' c bye Escape
tmux capture-pane -p -t t
```

tmux delivers an Escape immediately followed by another key in one read, which the terminal parser takes as an Alt chord. Send Escape in its own `send-keys` call and pause briefly before the next key.

## Where things go

- A new editing action: a function in `crates/maestro-editor/src/commands.rs` plus a line in the `commands!` table. Bind it in `keymap.rs` if it deserves a key.
- A new `:` command: an entry in `TYPED` in `typed.rs`.
- Pure text logic (motions, text objects): `motion.rs`, with unit tests next to it.
- Anything that changes text goes through a `Transaction`. Do not mutate a rope directly.
- Colors: add a role to `UiTheme` in `crates/maestro-config/src/theme.rs` and add the pair to the contrast test if text is drawn on it.

## Tests

- Unit tests live next to the code.
- Behavior tests in `crates/maestro-editor/tests/editing.rs` feed key scripts (`"wd"`, `"%slet<ret>"`) and check text and selections. Prefer these for anything a user does.
- Property tests (proptest) cover change-set inversion and position mapping in `maestro-core`.

## Performance

Measure before and after anything on the input path. `docs/performance.md` has the method and the current numbers. Do not write a number into docs that you did not measure.
