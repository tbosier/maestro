# Maestro

A modal text editor written in Rust, built for a workflow where one person and several coding agents edit the same codebase. The editing core comes first: it has to be good with every agent feature turned off.

Status: milestone M0 is done (a working terminal editor). M1 is in progress. Agent features (M2 and later) are not built yet. [ROADMAP.md](ROADMAP.md) says exactly what works.

## What works today

- Open, edit and save files. Saves are atomic (write to a temp file, fsync, rename) and keep permissions and symlinks.
- Selection-first modal editing in the Kakoune and Helix style, with Vim-familiar motions. Multiple selections everywhere: select by regex, split on regex, add cursors on the lines below, and edit them all at once.
- Undo and redo as a revision tree: editing after an undo starts a branch instead of discarding the old one, and `g-`/`g+` step through every state across branches. A whole insert session is one undo step.
- Registers (one value per selection), the system clipboard, macros (`Q` records, `q` replays), dot repeat, counts, marks and a jump list.
- Search with live preview, `n`/`N`, and `:s/pat/rep/g` or `:%s` substitution with capture groups.
- Text objects and surround: `mi(`, `ma"`, `ms[`, `mr([`, `md{`, `mm` for the matching bracket.
- Splits (`C-w v`, `C-w s`), multiple buffers, a buffer picker and a command palette listing every command with its key.
- A file finder (`space f`) that respects `.gitignore`, `.ignore` and Git's global excludes and fills in on a background thread, so you can type while a large tree is still being listed. Workspace search (`space /` or `:grep <regex>`) streams `path:line:col` results; accepting one opens the file there. Both pickers preview the file on the right when the terminal is at least 100 columns wide.
- Session restore: quitting saves the open files and cursors for the directory; starting `maestro` there with no file arguments reopens them. `space r` lists recently opened files.
- Detects files changed on disk (metadata, file identity, and a content hash at save time). A clean buffer reloads; a modified one warns and refuses to overwrite until you choose.
- Crash recovery: unsaved changes are journaled after two idle seconds, one journal per editor session, and `:recover` in a later session brings them back.
- UTF-8, UTF-8 with BOM, and UTF-16 files with a BOM. LF and CRLF are preserved. Binary files and invalid UTF-8 open read-only.
- The Duskfox theme from nightfox.nvim, with every interface text pair checked for WCAG AA contrast by a test.

- Language servers (rust-analyzer, pyright or pylsp, typescript-language-server, clangd when installed): diagnostics in the gutter and status line, `]d`/`[d`, `gd`, `gr` and hover on `space k`. No completion yet.
- Git: `+`, `~` and `-` signs in the gutter, the branch in the status line, and `:git-status` and `:worktrees` pickers.
- An automation API for agents and scripts: `maestro --headless` (JSON-RPC on stdio) or `--listen <socket>` alongside the interactive editor, with version-checked edits. See [docs/automation.md](docs/automation.md).

- Tree-sitter syntax highlighting for Rust, Python, TOML, JSON, Markdown, JavaScript, TypeScript and C, and structural selection: `A-o` grows the selection to the enclosing syntax node, `A-i` shrinks it, `A-n`/`A-p` move between siblings.

Not built yet: completion and the agent features (M2). Those are tracked in the roadmap.

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
| `g- g+` | step through every state, across undo branches |
| `v` | select mode (motions extend) |
| `/ ? n N *` | search |
| `.` | repeat the last change |
| `gg G` / `3G` / `:42` | go to the start, end, or a line |
| `space f` / `space /` / `space r` | find a file / search the workspace / recent files |

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
restore-session = true     # reopen the last session's files in this directory

[keys.normal]
"space w" = "write"
```

Unknown keys in the file are errors, so a typo does not silently do nothing. `--clean` starts without the config, and reads and writes neither the session nor the recent-files list.

## Documentation

- [ARCHITECTURE.md](ARCHITECTURE.md): crates, dependency direction and the main types.
- [ROADMAP.md](ROADMAP.md): milestones and their status.
- [docs/performance.md](docs/performance.md): how performance is measured, and the numbers.
- [docs/security.md](docs/security.md): trust boundaries.
- [CONTRIBUTING.md](CONTRIBUTING.md) and [AGENTS.md](AGENTS.md): how to work on it.

## License

MIT. The Duskfox palette comes from [nightfox.nvim](https://github.com/EdenEast/nightfox.nvim) by EdenEast, MIT licensed; see `third_party/`.
