# Maestro

A modal text editor written in Rust, built for a workflow where one person and several coding agents edit the same codebase. The editing core comes first: it has to be good with every agent feature turned off.

Status: milestone M0 is done (a working terminal editor). M1 is in progress. M2 (agents in worktrees, review, integration) is done: Claude Code and Codex have been run through the agent panel on a real task, reviewed, one rejected and one merged, and agent records survive a restart. Agents speaking the Agent Client Protocol are tested only against a fake agent so far. M3 has a first slice of each of its capabilities: edit proposals that an external agent sends and you approve, task contracts and conflict detection between agents, and structural selection pipelines. [ROADMAP.md](ROADMAP.md) says exactly what works.

## What works today

- Open, edit and save files. Saves are atomic (write to a temp file, fsync, rename) and keep permissions and symlinks.
- Selection-first modal editing in the Kakoune and Helix style, with Vim-familiar motions. Multiple selections everywhere: select by regex, split on regex, add cursors on the lines below, and edit them all at once.
- Undo and redo as a revision tree: editing after an undo starts a branch instead of discarding the old one, and `g-`/`g+` step through every state across branches. A whole insert session is one undo step.
- Registers (one value per selection), the system clipboard, macros (`Q` records, `q` replays), dot repeat, counts, marks and a jump list.
- Search with live preview, `n`/`N`, and `:s/pat/rep/g` or `:%s` substitution with capture groups.
- Text objects and surround: `mi(`, `ma"`, `ms[`, `mr([`, `md{`, `mm` for the matching bracket.
- Splits (`C-w v`, `C-w s`), multiple buffers, a buffer picker and a command palette listing every command with its key.
- A file finder (`space f`) that respects `.gitignore`, `.ignore` and Git's global excludes and fills in on a background thread, so you can type while a large tree is still being listed. Workspace search (`space /` or `:grep <regex>`) streams `path:line:col` results; accepting one opens the file there. Both pickers preview the file on the right when the terminal is at least 100 columns wide.
- Session restore: quitting saves the open files, the split layout, and each view's cursor and scroll for the directory; starting `maestro` there with no file arguments reopens them. `space r` lists recently opened files.
- Detects files changed on disk (metadata, file identity, and a content hash at save time). A clean buffer reloads; a modified one warns and refuses to overwrite until you choose.
- Crash recovery: unsaved changes are journaled after two idle seconds, one journal per editor session, and `:recover` in a later session brings them back.
- UTF-8, UTF-8 with BOM, and UTF-16 files with a BOM. LF and CRLF are preserved. Binary files and invalid UTF-8 open read-only.
- The Duskfox theme from nightfox.nvim, with every interface text pair checked for WCAG AA contrast by a test.

- Language servers (rust-analyzer, pyright or pylsp, typescript-language-server, clangd when installed): diagnostics in the gutter and status line, `]d`/`[d`, `gd`, `gr`, hover on `space k`, and completion in insert mode: a popup opens after a trigger character, after a short pause in typing an identifier, or on `C-x`, and narrows as you type. Snippets go in as plain text. Rename (`space n`) asks for the new name and changes every file the server names; files that were not open come up as unsaved buffers, so nothing is written until you save (`:wa`). `space =` or `:format` formats the buffer as one undo step. `space c` lists code actions (quick fixes, refactors) for the cursor or selection. In insert mode, typing `(` or `,` in a call, or `C-k`, shows the signature with the current parameter marked. An edit from the server is applied whole or not at all: if the server says which version of a buffer it edited and the buffer has changed since, or the edits overlap or point past the end, nothing changes and the status line says why.
- Git: `+`, `~` and `-` signs in the gutter, the branch in the status line, and `:git-status` and `:worktrees` pickers.
- An automation API for agents and scripts: `maestro --headless` (JSON-RPC on stdio) or `--listen <socket>` alongside the interactive editor, with version-checked edits. See [docs/automation.md](docs/automation.md).
- Edit proposals from those clients: a change to an open file that waits for you. The status line says one is waiting; `space e` or `:proposals` shows who sent it, why, and the diff with changed words marked. `a` approves after a dialog that names the exact verification command the client asked for, `x` rejects with an optional reason. An approved proposal is one undo step; if the file changed since the client looked, it is refused as stale. With a verification command the file is saved and the command runs in the background, and its exit code and output go back to the client and into the file's history. Headless runs refuse proposals.

- Tree-sitter syntax highlighting for Rust, Python, TOML, JSON, Markdown, JavaScript, TypeScript and C, and structural selection: `A-o` grows the selection to the enclosing syntax node, `A-i` shrinks it, `A-n`/`A-p` move between siblings.
- Structural pipelines: `:select-nodes` selects every syntax node a tree-sitter query (or a shortcut such as `pub-async-fn`) matches, filters keep or drop selections by query or regex, `:expand-to` grows them to an enclosing node kind, and `:insert-above` adds a line above each, as one undo step. Steps chain with `|`, and the status line shows how many selections each step left. For example, `:select-nodes pub-async-fn | keep-nodes returns-result | remove-nodes instrumented | insert-above #[tracing::instrument]` adds the attribute to every `pub async fn` that returns `Result` and has no `#[instrument]` or `#[tracing::instrument]` above it. Agents can run the same line through the automation API's `editor.execute`. See [docs/editor-model.md](docs/editor-model.md#structural-pipelines).

- Coding agents in Git worktrees (`space a` or `:agents`). `n` asks for a task and a provider, makes a worktree on a new `maestro/<task>-<id>` branch from the current commit, and starts the agent there: over the Agent Client Protocol (`gemini --acp`, `opencode acp`) or in a terminal (`claude`, `codex`, or any program you configure). `N` starts the same task with another provider, and the list shows each agent's status, files changed, lines added and removed, and last test result side by side. Status comes from what the agent reports, the process exit, test runs and your decisions; a terminal agent says "running" and nothing more, because Maestro cannot see inside it. `C-\` gives your keys to an agent's terminal and takes them back.
- Review per agent (`Enter` in the panel): changed files, unified or side-by-side diffs with changed words marked, line comments (`c`), `D` to drop one file's change (a stray cache file, say), and `Enter` to open the worktree's file at that line. `t` runs the tests in the worktree, `f` sends your comments and notes back to the agent, `p` answers a permission request, `X` rejects, `A` archives the worktree, and `I` commits the agent's work and merges, fast-forwards or cherry-picks it into your branch. Each of those asks first and says what it will do. Integrating refuses before committing anything when your worktree has uncommitted changes; a conflict is found after the agent's work is committed on its own branch, and then nothing reaches your branch.
- Task contracts: lines like `@allow src/** README.md`, `@forbid secrets/**`, `@verify cargo test -p core` and `@accept ...` in the task, or `@contract <file.toml>` naming a file, attach the paths a task may change, its verification command and acceptance criteria. The contract is shown before the agent starts and sent to it with the task, as a request. Maestro refuses ACP file writes through it outside the allowed paths, marks changed files outside them in the review, needs `O` instead of `y` to integrate them, runs the contract's command on `t`, and warns at integrate when that did not pass on the current files. It does not stop the agent's own tools; see [docs/security.md](docs/security.md).
- Overlaps between agents: when two agents change the same file, the panel says so and runs a merge dry run of their work (uncommitted files included) to show whether they would conflict. The integrate dialog says when this agent's own work conflicts with your branch, and names the overlapping agents whose work would conflict with it after this one is merged.
- Agent records survive a restart: they are saved per repository with the editor sessions and come back when the panel opens, marked "not running (from an earlier session)". Review, tests, integrate and archive work on them; `R` starts one again in its worktree (an ACP agent gets its saved session back when it supports `session/load`; a terminal agent starts without its task). Worktrees on `maestro/` branches that no record names are listed as found worktrees to review, archive or adopt (`a`).

Not built yet: an OS sandbox for agents (a worktree keeps their files apart, not their permissions; see [docs/security.md](docs/security.md)). That is tracked in the roadmap.

## Build and run

Requires Rust 1.88 or newer.

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
| `C-x` (insert mode) | ask the language server for completions |
| `C-n` `C-p`, Tab or Enter, Escape | in the completion popup: move, accept, close |
| `C-k` (insert mode) | show the signature of the call around the cursor |
| `gd` / `gr` / `space k` | go to the definition / list references / hover |
| `space n` / `:rename [name]` | rename the symbol under the cursor across files (unsaved until `:wa`) |
| `space c` / `:code-action` | pick a code action for the cursor or selection |
| `space =` / `:format`, `:format-selection` | format the buffer, or the primary selection, with the language server |
| `space a` | the agent panel: `n` new task (with `@allow`, `@forbid`, `@verify`, `@accept` or `@contract` lines for a task contract), `N` same task with another provider, `ret` review, `t` test, `f` feedback, `p` permission, `I` integrate (`O` instead of `y` when files are outside the contract), `X` reject, `A` archive, `R` relaunch, `a` adopt a found worktree, `v` history or test output, `C-\` keys to the agent's terminal and back |
| in the review | `j`/`k` lines, `]`/`[` hunks, `tab` files, `s` side by side, `c` comment, `D` drop the file's change, `ret` open the file there |

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

[agents]
worktree-dir = "~/worktrees"          # default: <repository>-worktrees next to it
test-command = ["cargo", "test", "--workspace"]   # default: cargo test when there is a Cargo.toml

[agents.provider.mine]                # any command-line agent, run in a terminal
kind = "terminal"
command = "my-agent"
args = ["--prompt", "{task}"]         # without {task}, the task is typed in after start

[agents.provider.codex]
kind = "terminal"
command = "codex"
enabled = false                       # hides a built-in provider
```

Unknown keys in the file are errors, so a typo does not silently do nothing. `--clean` starts without the config, and reads and writes neither the session, the recent-files list nor the agent records.

## Documentation

- [ARCHITECTURE.md](ARCHITECTURE.md): crates, dependency direction and the main types.
- [ROADMAP.md](ROADMAP.md): milestones and their status.
- [docs/performance.md](docs/performance.md): how performance is measured, and the numbers.
- [docs/security.md](docs/security.md): trust boundaries.
- [CONTRIBUTING.md](CONTRIBUTING.md) and [AGENTS.md](AGENTS.md): how to work on it.

## License

MIT. The Duskfox palette comes from [nightfox.nvim](https://github.com/EdenEast/nightfox.nvim) by EdenEast, MIT licensed; see `third_party/`.
