# Guide

Everything Maestro does today, the keys, and the config file. The [README](../README.md) has the short version.

## What works

### Editing

- Open, edit and save files. Saves are atomic (write to a temp file, fsync, rename) and keep permissions and symlinks.
- Selection-first modal editing in the Kakoune and Helix style, with Vim-familiar motions. Multiple selections everywhere: select by regex, split on regex, add cursors on the lines below, and edit them all at once.
- Undo and redo as a revision tree: editing after an undo starts a branch instead of discarding the old one, and `g-`/`g+` step through every state across branches. A whole insert session is one undo step.
- Registers (one value per selection), the system clipboard, macros (`Q` records, `q` replays), dot repeat, counts, marks and a jump list.
- Search with live preview, `n`/`N`, and `:s/pat/rep/g` or `:%s` substitution with capture groups.
- Text objects and surround: `mi(`, `ma"`, `ms[`, `mr([`, `md{`, `mm` for the matching bracket.
- Splits (`C-w v`, `C-w s`), multiple buffers, a buffer picker and a command palette listing every command with its key.
- Status messages are kept. `:messages` opens the last 200 (a message repeated in a row counts once) in a scratch buffer, oldest first and the cursor on the newest, each with its time (UTC) and the word `info`, `warning` or `error`, long ones wrapped to fit the window. A message too long for the status line, or with more than one line, ends in `… (:messages)` there. Messages about reloading, saving, closing and quitting, and the integrate refusal for unsaved edits, name files inside the working directory by their relative path.
- A file finder (`space f`) that respects `.gitignore`, `.ignore` and Git's global excludes and fills in on a background thread, so you can type while a large tree is still being listed. Workspace search (`space /` or `:grep <regex>`) streams `path:line:col` results; accepting one opens the file there. Both pickers preview the file on the right when the terminal is at least 100 columns wide.
- Session restore: quitting saves the open files, the split layout, and each view's cursor and scroll for the directory; starting `maestro` there with no file arguments reopens them. `space r` lists recently opened files.
- Detects files changed on disk (metadata, file identity, and a content hash at save time). A clean buffer reloads; a modified one warns and refuses to overwrite until you choose.
- Crash recovery: unsaved changes to named files are journaled after two idle seconds (a new buffer with no file name is not), one journal per editor session, and `:recover` in a later session brings them back.
- UTF-8, UTF-8 with BOM, and UTF-16 files with a BOM. LF and CRLF are preserved. Binary files and invalid UTF-8 open read-only.
- The Duskfox theme from nightfox.nvim. A test checks its main interface text pairs for WCAG AA contrast in 24-bit color; terminals without 24-bit color get the nearest 256-color values, which the test does not check.

### Language servers

- Language servers (rust-analyzer, pyright or pylsp, typescript-language-server, clangd when installed): diagnostics in the gutter and status line, `]d`/`[d`, `gd`, `gr`, hover on `space k`, and completion in insert mode: a popup opens after a trigger character, after a short pause in typing an identifier, or on `C-x`, and narrows as you type. Snippets go in as plain text. Rename (`space n`) asks for the new name and changes every file the server names; files that were not open come up as unsaved buffers, so nothing is written until you save (`:wa`). `space =` or `:format` formats the buffer as one undo step. `space c` lists code actions (quick fixes, refactors) for the cursor or selection. In insert mode, typing `(` or `,` in a call, or `C-k`, shows the signature with the current parameter marked. An edit from the server is applied whole or not at all: if the server says which version of a buffer it edited and the buffer has changed since, or the edits overlap or point past the last line, nothing changes and the status line says why.

### Git

- Git: `+`, `~` and `-` signs in the gutter, the branch in the status line, and `:git-status` and `:worktrees` pickers.
- A Git status screen (`space g` or `:git`) in the style of Magit. The head line shows the branch, its upstream with ahead and behind counts, and any merge, rebase, cherry-pick, revert or bisect in progress. Below it are untracked, unstaged and staged files, stashes and recent commits. Tab opens a file's hunks with changed words marked. `s`, `u` and `x` stage, unstage and discard a file, a hunk, or lines selected with `v`; partial staging changes the index only, never the file. `x` asks first, says what will be lost and whether Git can bring it back, and refuses when the file's diff changed since the screen was drawn. `c c` opens a commit message buffer with the staged summary below a scissors line, and saving and closing it (`:wq`) commits; an empty message aborts. `c a` amends, and asks again when the commit is already on a remote branch. `b` switches or creates a branch, `z` stashes, `Z` pops, `l` shows the log with its graph and Enter a commit's diff. `f`, `F` and `P` fetch, pull (fast-forward only) and push after a dialog that shows the exact git command and remote; nothing force-pushes. Discarding, switching branches, stashing, popping, pulling and amending are refused while an operation is in progress. A commit is refused if `HEAD`, the branch or the operation changed while the message was open, and a pull or push if another branch was checked out after its dialog. Every git command runs off the UI thread.
- In buffers, `]h` and `[h` move between changes, and `space h` stages the change under the cursor or the selected lines (`s`, from the saved file), resets it to the staged text after asking (`r`; it edits only the buffer, so `u` undoes it, and it is allowed during a merge or rebase), shows its diff (`p`), blames the line (`b`) or toggles a blame column in the gutter (`B`).

### Automation API and edit proposals

- An automation API for agents and scripts: `maestro --headless` (JSON-RPC on stdio) or `--listen <socket>` alongside the interactive editor, with version-checked edits. See [docs/automation.md](automation.md).
- Edit proposals from those clients: a change to an open file that waits for you. The status line says one is waiting; `space e` or `:proposals` shows who sent it, why, and the diff with changed words marked. `a` approves after a dialog that names the exact verification command the client asked for, `x` rejects with an optional reason. An approved proposal is one undo step; if the file changed since the client looked, it is refused as stale. With a verification command the file is saved and the command runs in the background, and its exit code and output go back to the client and into the file's history. Headless runs refuse proposals.

### Syntax and structural editing

- Tree-sitter syntax highlighting for Rust, Python, TOML, JSON, Markdown, JavaScript, TypeScript and C, and structural selection: `A-o` grows the selection to the enclosing syntax node, `A-i` shrinks it, `A-n`/`A-p` move between siblings.
- Structural pipelines: `:select-nodes` selects every syntax node a tree-sitter query (or a shortcut such as `pub-async-fn`) matches, filters keep or drop selections by query or regex, `:expand-to` grows them to an enclosing node kind, and one text step changes the text at each (`:insert-above`, `:insert-below`, `:replace-text` with capture groups, `:delete`, `:wrap`, `:prepend`, `:append`), as one undo step. Steps chain with `|`, and the status line shows how many selections each step left. For example, `:select-nodes pub-async-fn | keep-nodes returns-result | remove-nodes instrumented | insert-above #[tracing::instrument]` adds the attribute to every `pub async fn` that returns `Result` and has no `#[instrument]` or `#[tracing::instrument]` above it. Agents can run the same line through the automation API's `editor.execute`, or send it as an intent edit: a proposal whose pipeline runs again on the current text when the document moved on before you approve it, with the review saying it was rebased. See [docs/editor-model.md](editor-model.md#structural-pipelines) and [docs/automation.md](automation.md#intent-edits).

### Agents

- Coding agents in Git worktrees (`space a` or `:agents`). `n` asks for a task and a provider, makes a worktree on a new `maestro/<task>-<id>` branch from the commit checked out on the branch in the repository's main worktree (the branch `I` integrates into, whichever worktree you are in), and starts the agent there: over the Agent Client Protocol (`gemini --acp`, `opencode acp`) or in a terminal (`claude`, `codex`, or any program you configure). The provider list says where the agent starts (`starts from main at 1a2b3c4d`), says so when you are in another worktree, and warns with `!` when the main worktree has uncommitted or untracked files, which the agent will not see unless you commit them first; it names them, and starting is still allowed. When the main worktree has a detached HEAD or its branch has no commits, the agent starts from the commit checked out in the worktree you are in, and the list says so. `N` starts the same task with another provider, from the same commit, and the list shows each agent's status, files changed, lines added and removed, and last test result side by side. Status comes from what the agent reports, the process exit, test runs and your decisions; a terminal agent says "running" and nothing more, because Maestro cannot see inside it. To type into a running terminal agent, press `i` or click in its screen; the header then says `KEYS: TYPING INTO <agent>`, and `C-\` or a click outside the terminal gives the keys back. `m` opens a box for a message to the agent instead, sent with any review comments you have not sent yet (`f`, its old name, still works). Click an agent in the list to select it, and scroll the list or its screen with the wheel. The hint line lists only the keys that work on the selected agent: no `m` once its program has exited, and for an archived agent only `v`, `N`, `d` and, while two other agents on its task can still be composed, `C`. When the hint line is too narrow for every key, it ends with `? all keys`, and `?` lists them.
- Review per agent (`Enter` in the panel): changed files, unified or side-by-side diffs with changed words marked, line comments (`c`), `D` to drop one file's change (a stray cache file, say), and `Enter` to open the worktree's file at that line. `t` runs the tests in the worktree right away. A file that run creates and the agent never wrote (a `Cargo.lock` from `cargo test` in a repository that does not commit one) is listed after the agent's files, marked `T` with "test run" where the line counts go, and is not counted, marked, composed or integrated; the integrate dialog names it as left behind, untracked in the worktree. Once the file changes, is staged or is committed it counts as the agent's again, and `K` counts it as the agent's right away. `m` sends your comments and a message back to the agent, `p` answers a permission request, `X` rejects, `A` archives the worktree, and `I` commits the agent's work and merges, fast-forwards or cherry-picks it into the branch checked out in the repository's main worktree. Reject, archive and integrate ask first and say what they will do. Integrating refuses before committing anything when your worktree has uncommitted changes; a conflict is found after the agent's work is committed on its own branch, and then nothing reaches your branch.
- Task contracts: lines like `@allow src/** README.md`, `@forbid secrets/**`, `@verify cargo test -p core` and `@accept ...` in the task, or `@contract <file.toml>` naming a file, attach the paths a task may change, its verification command and acceptance criteria. The contract is shown before the agent starts and sent to it with the task, as a request. Maestro refuses ACP file writes through it outside the allowed paths, marks changed files outside them in the review, needs `O` instead of `y` to integrate them, runs the contract's command on `t`, and warns at integrate when that did not pass on the current files. It does not stop the agent's own tools; see [docs/security.md](security.md).
- Part of an agent's work: `space` in the review marks the hunk under the cursor, and `v`, then `j`/`k`, then `space` marks some of its lines. With marks, `I` integrates only them: Maestro commits exactly those hunks on the agent's starting commit, on a new `maestro/<agent>-part-<n>` branch, without touching either worktree, and integrates that commit through the same checks as a whole agent. The rest stays in the agent's worktree, and the review shows which hunks went in. A marked hunk that needs one you did not mark (a file added where the agent removed another) is refused by name.
- One result from several agents: `C` on an agent opens the compose view for the agents started on its task with `N`. It shows each file's hunks from every agent side by side, with each agent's name and last test result. Mark hunks from any of them; `ret` checks that they apply together on the agents' common starting commit, names any conflict by hunk and agent, and then commits them on a new `maestro/compose-<id>` branch with a worktree of its own. The result appears in the panel as a composed entry with the same task and contract; `t` tests it and `I` integrates it like any agent's work. Its commit message says which hunk came from which agent.
- Overlaps between agents: when two agents change the same file, the panel says so and runs a merge dry run of their work (uncommitted files included) to show whether they would conflict. The integrate dialog says when this agent's own work conflicts with your branch, and names the overlapping agents whose work would conflict with it after this one is merged.
- Each agent's state has one word, the same in the panel, the sidebar, the jump palette and notifications (`ready for review`, `stopped`, `archived`; the full list is in `docs/agent-model.md`), drawn after a mark (`!` needs you, `~` working, `-` idle, `=` done). Once its worktree is archived an agent says `archived`, and `d` forgets its record after a y/n question: it leaves the panel and the saved records, nothing on disk is deleted, and the branch stays. `d` refuses for an agent that is not archived.
- Agent records survive a restart: they are saved per repository with the editor sessions and come back when the panel opens, marked "not running (from an earlier session)". Review, tests, integrate and archive work on them; `R` starts one again in its worktree (an ACP agent gets its saved session back when it supports `session/load`; a terminal agent starts without its task). Worktrees on `maestro/` branches that no record names are listed as found worktrees to review, archive or adopt (`a`).

### GitHub

- A GitHub inbox (`space i` or `:inbox`), through the GitHub CLI (`gh`) and the account it is logged in to: pull requests that ask for your review, pull requests assigned to you, your own open pull requests, issues assigned to you, and the open pull requests of the repository you are in. Each row shows `owner/repo#number`, the title, the author, the checks (`✓ passed 6`, `✗ failed 3/13`, `● pending 1/4`) and the age. It loads in the background and shows when it last did; `r` reloads, and `[github] refresh-secs` reloads on an interval (off by default). `Enter` on a pull request shows its checks by name, reviews, description as plain text, labels, and each changed file's diff in the agent review's layout. `o` opens it in the browser, `w` checks a pull request of the current repository out into a new worktree next to it and opens a file there. `a` approves, `c` comments and `R` requests changes (both written in a buffer of their own and sent with `:pr-send`), and `m` merges after you pick merge, squash or rebase. Each of those first shows the exact `gh` command and the text it sends, and the merge dialog also warns about drafts, failed or unfinished checks, conflicts and missing reviews; nothing is sent until you press `y`. Approvals and change requests are submitted on the head commit the screen showed, and are not sent if the pull request got new commits since; a merge is refused by GitHub in that case, and afterward the status line says whether it merged or a merge queue or auto-merge took it. The pull request's details and diff are checked to be of one commit. When `gh` is missing or logged out, the screen says so and how to fix it (`gh auth login`). The status line shows how many reviews wait for you, as of the last load. The searches cover gh's default host (github.com unless you changed it); "This repository" also works for a GitHub Enterprise remote.

### Projects, worktrees and switching

- Projects and worktrees in a sidebar (`space s`). The top lists every agent grouped by what it needs from you: needs you (awaiting approval, ready for review, failed tests, failed, blocked), then working, idle and done. Each row has a mark and the same word the agent panel shows for that agent (`! ready for review`, `- stopped`, `= archived`), never color alone; failed tests add `, tests failed` after the word. Below that are the projects: the Git repositories in `[projects] paths`, the ones you added in the sidebar, the directories of earlier sessions and the one you are in. Each unfolds to its worktrees with the branch, the number of changed files, ahead and behind of the upstream when there is one, and the agent working there. `ret` on a worktree or an agent switches the editor to it: the working directory moves (the file finder, workspace search, the agent panel and new language servers follow), your files and splits are saved as that worktree's session, and the target's session comes back. Buffers with unsaved changes are never closed: they stay open, the status line names them, and the sidebar counts them on their worktree's row. The status line names the project and worktree (`alpha:main`). Git runs on a background thread and the sidebar rescans every five seconds while it is open.
- To add a project, press `a` in the sidebar (or run `:project-add`, or pick "add a project" at the end of the jump palette) and type the path of any directory in a Git repository. `~` and paths relative to the working directory work, and `tab` completes directory names. A subdirectory or a linked worktree adds the whole repository. The project shows up selected and unfolded, and the status line says `added <name>`; a path that does not exist or is not in a Git repository is refused with the reason. Added projects are kept in `projects.toml` in the state directory (`$XDG_STATE_HOME/maestro`), not in your config file, so they are there after a restart. `--clean` neither reads nor writes that file.
- To forget a project, select its row in the sidebar and press `d` (or run `:project-remove <name or path>`), then `y`. It only leaves the list: no files, worktrees or branches are deleted. A forgotten project stays off the list even when an earlier session was in it, until you add it again. Projects from `[projects] paths` are not forgotten this way; the status line names the config file to remove them from. The project you are in, and one an agent of this editor works in, always stay listed.
- A file tree (`space t` or `:tree`) in a column right of the views, showing the files of the worktree you are in, with every file that differs from the base branch marked. The base is where your branch left the default branch: the merge-base of `HEAD` with `origin/HEAD`'s target, else `main`, else `master`. So the marks cover what the branch would bring: its commits, staged and unstaged edits and untracked files. Each changed file has a letter in the Git sign color: `M` modified, `A` added, `D` deleted, `R` renamed, `?` untracked, and the selected file also shows the word. Files deleted on the branch stay in the tree so you can see them. A directory shows how many changed files it holds. The header says what the files are compared with, such as `4 changed vs main (merge-base abc1234)`. On the default branch itself, or without one, it compares with `HEAD` and says so (`2 uncommitted vs HEAD (on main)`). Outside a Git repository it lists the files without marks and says why. `c` shows only the changed files, as a tree of the directories that hold them. `d` on a changed file shows its diff against the base over the views, in the agent review's layout with changed words marked (`s` side by side, `ret` opens the file at that line, `q` back). The tree follows `.gitignore` and `.ignore` like the file finder and leaves hidden files out unless they changed. Directories start folded and are listed on a background thread when you unfold them; Git runs on a background thread too. The tree follows you when you switch worktrees, and reads the changes again after you save, when you press `r`, and every five seconds while it is open. Clicking a row selects it, clicking a file opens it, clicking a directory folds or unfolds it, and the wheel scrolls. When the window has no room for it next to the views (they keep at least 20 columns), it is not drawn and takes no keys.
- A jump palette (`space j`): one fuzzy list of projects, worktrees, agents (by name and task) and open buffers. Choosing one goes there, switching worktree when needed.
- When an agent starts needing you, the status line says so once, and the terminal gets a bell or an OSC 9 desktop notification (`[notify] method`).
- Language servers start for the worktree you switch into when its files open. Servers of roots you left stop after five unused minutes, or when more than three roots have servers; a server with an open buffer is never stopped.

## Not built yet

An OS sandbox for agents. A worktree keeps their files apart, not their permissions; see [docs/security.md](security.md). The [roadmap](../ROADMAP.md) tracks it.

## Keys

| Keys | Does |
|---|---|
| `h j k l`, arrows | move |
| `w b e` | select to the next word start, previous word start, word end |
| `x` | select the line (again to extend) |
| `%` | select the whole file |
| `s` | select regex matches inside the selections |
| `C` | copy the selection to the next line it fits on (a cursor below, for a plain cursor) |
| `,` / `;` | keep only the primary selection / collapse selections |
| `d c y p` | delete, change, yank, paste |
| `i a o O I A` | insert modes |
| `u U` | undo, redo |
| `g- g+` | step through every state, across undo branches |
| `v` | select mode (motions extend) |
| `/ ? n N *` | search |
| `.` | repeat the last change |
| `gg G` / `3G` / `:42` | go to the start, end, or a line |
| `:messages` | earlier status messages, including ones cut off or replaced |
| `space f` / `space /` / `space r` | find a file / search the workspace / recent files |
| `C-x` (insert mode) | ask the language server for completions |
| `C-n` `C-p`, Tab or Enter, Escape | in the completion popup: move, accept, close |
| `C-k` (insert mode) | show the signature of the call around the cursor |
| `gd` / `gr` / `space k` | go to the definition / list references / hover |
| `space n` / `:rename [name]` | rename the symbol under the cursor across files (unsaved until `:wa`) |
| `space c` / `:code-action` | pick a code action for the cursor or selection |
| `space =` / `:format`, `:format-selection` | format the buffer, or the primary selection, with the language server |
| `space g` / `:git` | the Git status screen: `tab` hunks, `s` `u` `x` stage, unstage, discard (file, hunk or `v` lines), `c c` commit, `c a` amend, `b` branch, `z`/`Z` stash and pop, `f`/`F`/`P` fetch, pull, push, `l` log (`ret` shows a commit), `r` refresh, `q` close |
| `]h` `[h`, `space h` | next and previous change; `space h s` stage, `r` reset, `p` preview, `b` blame the line, `B` blame column |
| `space a` | the agent panel: `n` new task (with `@allow`, `@forbid`, `@verify`, `@accept` or `@contract` lines for a task contract), `N` same task with another provider, `C` compose hunks of the agents on one task, `ret` review, `i` or a click to type into a running terminal agent (`C-\` or a click outside gives the keys back), `m` message the agent, `t` test, `p` permission, `I` integrate (`O` instead of `y` when files are outside the contract), `X` reject, `A` archive, `R` relaunch, `a` adopt a found worktree, `d` forget an archived agent's record, `v` history or test output, `?` every key |
| in the review | `j`/`k` lines, `]`/`[` hunks, `tab` files, `s` side by side, `c` comment, `space` mark or unmark the hunk, `v` then `j`/`k` and `space` mark lines (`esc` drops the range), `D` drop the file's change, `K` count a file the test run created (marked `T`) as the agent's, `ret` open the file there, `m` message the agent; with marks, `I` integrates only them |
| in the compose view (`C`) | `j`/`k` hunks down and up the agent's column, `h`/`l` the next agent's column, `tab` files, `space` mark, `ret` check and compose, `r` refresh, `esc` back |
| `space i` / `:inbox` | the GitHub inbox: `j`/`k` move, `ret` details, `o` browser, `w` worktree, `a` approve, `c` comment, `R` request changes, `m` merge, `r` reload |
| in a pull request's details | `j`/`k` scroll, `tab` the next file's diff, `]`/`[` hunks, `h`/`l` sideways, `s` side by side, `esc` back to the list; the inbox keys above work here too |
| `:pr-send` | in a comment or change-request buffer: show exactly what will be sent, then `y` sends it |
| `space s` | the sidebar: opens and focuses it, focuses it, or hides it. In it: `j`/`k` move, `ret` switch to a worktree or agent (on a project: fold or unfold), `l`/`h` unfold or fold, `a` add a project, `d` forget the selected project (asks first; deletes no files), `r` rescan, `q` hide, `esc` back to the editor with the sidebar still shown, `space ...` the usual space keys |
| `space j` | jump to a project, worktree, agent or open buffer |
| `space t` / `:tree` | the file tree: opens and focuses it, focuses it, or hides it. In it: `j`/`k` move, `g`/`G` first and last, `ret` open a file (on a directory: fold or unfold), `l`/`h` unfold or fold, `c` only changed files, `d` the file's diff against the base, `r` read again, `q` hide, `esc` back to the editor with the tree still shown, `space ...` the usual space keys |
| in the tree's diff (`d`) | `j`/`k` lines, `C-d`/`C-u` half a page, `g`/`G` first and last, `s` side by side, `ret` open the file at that line, `q` or `esc` back to the tree |

The full model is in [docs/editor-model.md](editor-model.md).

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

[projects]
paths = ["~/src/maestro", "~/src/site"]   # listed in the sidebar and the jump palette

[sidebar]
width = 36

[tree]
width = 32                            # the file tree; the views keep at least 20 columns

[notify]
method = "bell"                       # "osc" for an OSC 9 desktop notification, "off"

[lsp]
stop-unused-after-secs = 300          # stop servers of worktrees you left; 0 never does
max-live-roots = 3                    # 0 means no limit

[github]
gh = "gh"                             # the GitHub CLI: a path or a name on PATH
refresh-secs = 0                      # reload the inbox this often; 0 (default) only on open and r
```

Unknown keys in the file are errors, so a typo does not silently do nothing. `--clean` starts without the config, and reads and writes neither the session, the recent-files list nor the agent records.
