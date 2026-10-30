# Maestro

A modal text editor written in Rust, built for a workflow where one person and several coding agents edit the same codebase. The editing core comes first: it has to be good with every agent feature turned off.

Status: milestone M0 is done (a working terminal editor). M1 is in progress. Agent features (M2 and later) are not built yet. [ROADMAP.md](ROADMAP.md) says exactly what works.

## What works today

- Open, edit and save files. Saves are atomic (write to a temp file, fsync, rename) and keep permissions and symlinks.
- Selection-first modal editing in the Kakoune and Helix style, with Vim-familiar motions. Multiple selections everywhere: select by regex, split on regex, add cursors on the lines below, and edit them all at once.
- Undo and redo as a revision tree, so undoing and then editing never throws away history. A whole insert session is one undo step.
- Registers (one value per selection), the system clipboard, macros (`Q` records, `q` replays), dot repeat, counts, marks and a jump list.
- Search with live preview, `n`/`N`, and `:s/pat/rep/g` or `:%s` substitution with capture groups.
- Text objects and surround: `mi(`, `ma"`, `ms[`, `mr([`, `md{`, `mm` for the matching bracket.
- Splits (`C-w v`, `C-w s`), multiple buffers, a buffer picker, a simple file finder, and a command palette listing every command with its key.
- Detects files changed on disk. A clean buffer reloads; a modified one warns and refuses to overwrite until you choose.
- Crash recovery: unsaved changes are journaled after two idle seconds, and `:recover` brings them back after a crash.
- UTF-8, UTF-8 with BOM, and UTF-16 files with a BOM. LF and CRLF are preserved. Binary files and invalid UTF-8 open read-only.
- The Duskfox theme from nightfox.nvim, with every interface text pair checked for WCAG AA contrast by a test.

Not built yet: syntax highlighting, language servers, workspace search, Git, agents. Those are tracked in the roadmap.

## Build and run

Requires a Rust toolchain that supports edition 2024 (1.85 or newer).

```sh
cargo build --release
./target/release/maestro path/to/file.rs
./target/release/maestro src/main.rs:42   # open at line 42
```

`:q` quits, `:w` saves, `space ?` opens the command palette, and pressing `space`, `g`, `m` or `C-w` shows what can follow.

## A short tour of the keys

| Keys | Does |
|---|---|
| `h j k l`, arrows | move |
| `w b e` | select to the next word start, previous word start, word end |
| `x` | select the line (again to extend) |
| `%` | select the whole file |
| `s` | select regex matches inside the selections |
| `C` | add a cursor on the next line |
| `,` / `;` | keep only the primary selection / collapse selections |
| `d c y p` | delete, change, yank, paste |
| `i a o O I A` | insert modes |
| `u U` | undo, redo |
| `v` | select mode (motions extend) |
| `/ ? n N *` | search |
| `.` | repeat the last change |
| `gg G` / `3G` / `:42` | go to the start, end, or a line |

The full model is in [docs/editor-model.md](docs/editor-model.md).

## Configuration

Optional. `~/.config/maestro/config.toml`:

```toml
profile = "maestro"        # or "vim" for a partial Vim layer
[editor]
tab-width = 4
indent = "spaces"          # or "tabs"
indent-width = 4
line-numbers = "absolute"  # relative, off
scrolloff = 5
recovery-interval-secs = 2 # 0 turns crash recovery off
mouse = true

[keys.normal]
"space w" = "write"
```

Unknown keys in the file are errors, so a typo does not silently do nothing. `--clean` starts without the config.

## Documentation

- [ARCHITECTURE.md](ARCHITECTURE.md): crates, dependency direction and the main types.
- [ROADMAP.md](ROADMAP.md): milestones and their status.
- [docs/performance.md](docs/performance.md): how performance is measured, and the numbers.
- [docs/security.md](docs/security.md): trust boundaries.
- [CONTRIBUTING.md](CONTRIBUTING.md) and [AGENTS.md](AGENTS.md): how to work on it.

## License

MIT. The Duskfox palette comes from [nightfox.nvim](https://github.com/EdenEast/nightfox.nvim) by EdenEast, MIT licensed; see `third_party/`.
