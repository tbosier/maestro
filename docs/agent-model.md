# Agent model

This describes what is built as of October 30, 2026. Anything marked partial or not built is not there.

## Version-checked edits

`Document::apply_at_version(tx, expected_version, selection)` applies a transaction only if the document is still at the version the edit was computed against. Otherwise it returns `EditError::Stale` and changes nothing. Separately, every `ChangeSet` records the length of the document it was built for and refuses to apply to any other length.

Edits produced away from the keyboard and computed against a known version must go through `apply_at_version`. The automation API's `document.apply_edit` does. Its `command.run` and `editor.execute` do not: they run editor commands on the current text, as a key press would, with no version check and no entry in the edit history (see `docs/automation.md`).

Coding agents started from the agent panel do not edit buffers. They change files in their own worktree, and the user reviews and integrates those files through Git (below).

## Agents

An agent is a record in the editor (`maestro_editor::agents::Agent`) with:

- an id and a name (`<provider>-<id>`), the task text, and a group number shared by agents started on the same task;
- the provider it runs (see Providers);
- its worktree: a path, a branch named `maestro/<slug>-<id>`, the commit it started from, and the repository's main worktree, where its work is integrated;
- a status (below);
- the runtime: an ACP client, or a terminal (a PTY) for terminal providers;
- for ACP agents, the transcript (messages, thoughts, tool calls, plans), the pending permission request and the agent's stderr (last 500 lines);
- a history of timestamped events;
- verification runs with their output (last 20,000 lines each), exit code, duration and parsed `cargo test` totals;
- errors, and the files changed according to the last review diff;
- review comments, each on one line of one file, marked once sent.

Records live only in memory. Quitting the editor forgets them; the worktrees and branches stay on disk.

## Providers

The provider list is the enabled `[agents.provider.<name>]` entries from the config, then the built-in providers found on `PATH` that the config does not name: `gemini --acp` and `opencode acp` (ACP), `claude {task}` and `codex {task}` (terminal). A config entry with `enabled = false` hides the built-in of the same name.

- An ACP agent is started with its worktree as the working directory, gets `session/new` there, and receives the task as its first prompt.
- A terminal agent runs in a PTY in its worktree. Each argument containing `{task}` has it replaced by the task text, as one argument with no shell quoting. If no argument contains `{task}`, the task is typed into the terminal once the program first draws something, or after one second.

## Status

Status comes only from these signals:

| Status | Set when |
|---|---|
| starting | the worktree is being created or the ACP session is starting |
| working | an ACP prompt was sent and the agent has not sent a plan or edit in this turn |
| planning | the ACP agent sent a plan during the turn |
| editing | the ACP agent reported an edit, delete or move tool, or wrote a file through the client |
| awaiting approval | the ACP agent asked for permission |
| running | a terminal agent's program is running; Maestro cannot see what it is doing |
| running tests | Maestro's verification command runs in the worktree (shown over the agent's own status) |
| ready for review | the ACP turn ended with `end_turn`, or the terminal program exited with code 0, and the review diff has changes (or has not been computed yet) |
| finished, no changes | as above, but the review diff of the worktree against its starting commit is empty |
| blocked | the ACP turn ended any other way (`refusal`, `cancelled`, `max_tokens`, `max_turn_requests`) |
| failed | the worktree or process could not start, the turn ended with an error, the ACP process exited mid-turn, or the terminal program exited with a nonzero code |
| rejected | the user rejected the agent |
| integrated | the user integrated its work |

Once rejected or integrated, agent events no longer change the status. `maestro_agent::AgentStatus` has no state for "working" or for a running terminal program, so the editor has its own status type with those two added. Each status is drawn with its name next to its color.

## The panel and review screen

`space a` or `:agents` opens the panel over the editor area. The keys are listed on its last line. The review screen (`Enter` on an agent) shows the diff of the worktree against the commit it started from: commits on its branch, staged, unstaged and untracked files. Diffs are computed on a background thread when the panel opens, when an ACP turn ends or the agent writes a file, when a process exits, after integration, every three seconds for running agents while the panel is open, and on `r`.

Terminal agents have no structured status. The panel says so ("terminal only") next to every terminal agent.

## Actions

- Tests (`t`): runs `[agents] test-command`, or `cargo test` when the worktree has a `Cargo.toml`, with `maestro-tasks`. Output streams into the tests view; the totals are parsed from `cargo test` output.
- Drop a file (`D` in the review): discards the agent's change to the file under the cursor in its worktree, after a dialog that says what happens to it (restored to the starting commit's version, or deleted if the agent added it). Refused while that file has unsaved edits in a buffer, while other Git work for the agent runs, when the worktree no longer has the agent's branch checked out or no longer descends from its starting commit, and when the path (or a parent of it) is a directory or a symlink. A file the agent turned into a symlink, or the reverse, is restored whichever half of the change is dropped. This is how to keep stray files (build output, caches) out of an integration; there is no partial staging of hunks.
- Feedback (`f`): unsent comments plus typed text. An ACP agent gets them as a new prompt (refused while a turn is running). A terminal agent gets them typed into its terminal, and the input box says that is all Maestro can do.
- Permission (`p`): lists the choices of the oldest unanswered request (requests queue; the transcript says how many more wait). Choices that allow something ask for confirmation first; rejecting is sent at once.
- Reject (`X`): stops the agent and its tests and marks it rejected. `a` in the dialog also archives the worktree. Rejecting while the worktree is still being made means the agent is never launched.
- Archive (`A`): removes the worktree with `remove_worktree`, keeping the branch, after the agent's program has exited. Ignored files in it are deleted without a second question, and the dialog says so. If the worktree has uncommitted or untracked files, or commits on no branch, the removal is refused and a second dialog lists what forcing would delete; only `F` there forces it.
- Integrate (`I`): checks first and refuses, changing nothing, if either worktree is gone or on another branch, the main worktree is detached or has uncommitted changes, the agent's starting commit is not on the main worktree's branch, or an open buffer in either worktree has unsaved edits. The dialog says what will be committed and how the branch will be integrated (`tab` cycles merge, fast-forward and cherry-pick). On `y`, the same checks run again; then uncommitted work in the agent's worktree is committed with the task's first line as the subject, and `integrate` runs in the main worktree. `maestro-git` checks again and refuses on its own.
- A dialog that does not fit the window cannot be accepted.
- `:q` and `:qa` refuse while Git work for an agent runs (a worktree being made, an integration, an archive). With agents or tests running they ask first, and on `y` stop them and wait up to a second for terminal programs to exit. The automation API's `editor.quit` refuses the same way while Git work runs, and refuses while agents or tests run unless `force` is passed, which stops them first.

## Not built

- Agent records do not survive a restart, so the "Sessions" item of M2 is not done.
- Agents cannot propose edits to open buffers; there is no edit proposal or review status per buffer edit.
- No OS sandbox around agent processes (see `docs/security.md`).
- Terminals show only in the panel, not in splits.
