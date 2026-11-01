# Agent model

This describes what is built as of October 31, 2026. Anything marked partial or not built is not there.

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
- review comments, each on one line of one file, marked once sent;
- for ACP agents, the session id and whether the agent advertised `loadSession`.

## Records across restarts

When sessions are on (the default; not with `--clean`, headless runs or `restore-session = false`), agent records are saved per repository in the state directory, `$XDG_STATE_HOME/maestro/agents/<hash of the main worktree's canonical path>.json`, next to the editor sessions. A record holds the id, name, group, task, provider name and kind, worktree (path, branch, starting commit, main worktree), status, the archived flag, how the process ended, errors (last 100), history (last 500 events), comments, test runs (last 10, each with its exit code, duration, `cargo test` totals and last 100 output lines) and the ACP session id. It does not hold the provider's arguments or environment, the ACP transcript, the agent's stderr or the terminal screen; the history names the program that was started. The file has a version number.

Saving is coalesced. About once a second Maestro checks whether anything a record holds changed (an agent created, a status change, a decision, a test run started or finished, a comment, an archive, a session id) and, if so, writes the file once; a test run's output is saved when the run ends. Quitting saves again with the file synced to disk.

The records of the editor's repository are read when the agent panel first opens, not at startup, on a background thread: finding the main worktree takes Git, and a session that never opens the panel runs no Git for it. Starting an agent waits until they are read, so ids continue after the highest saved one and a new branch never reuses a saved name. A records file that cannot be used (not JSON, another version, written for another repository, a record with an unknown kind, a relative path, a branch outside `maestro/`, a base that is not a commit id, an id out of range) is reported in the status line and left as it is, and nothing is saved for that repository in that session, so it is never overwritten. Before each save the file is compared with what this session last read or wrote. If another Maestro saved in between, its records for worktrees this session does not know are added (as loaded agents) so the save keeps them; records both sessions have are saved as this session has them. If the file became unusable, saving stops and the file is left alone. A record whose id an agent of this session already has gets a new id and name, and a group already in use gets a new group; the history says so.

A loaded agent has no process. A status that needs one (starting, working, planning, editing, awaiting approval, running, running tests, waiting for a prompt) loads as **stopped**, and its history says what it was. Every loaded agent is marked "not running (from an earlier session)" next to its status in the panel, and its live view says the transcript and screen were not saved. Review, tests, comments, drop file, reject, archive and integrate work on it as on any agent. Feedback is refused with a message that says to relaunch it first.

When records load, each one's worktree is checked against `git worktree list`. If Git no longer lists it, its directory is gone, or it is on another branch than the record names, the history says so and relaunch and tests refuse; archive refuses while the directory still exists (when it is gone, archiving only marks the record), and the review shows no diff for a worktree on another branch. The check is repeated at the moment of use for every agent, not only at load: tests and relaunch read the worktree's `.git` file and `HEAD`, and drop file, archive and integrate ask Git, so a worktree switched to other work later is refused too.

### Relaunch

`R` starts an agent with no process again in its worktree, with the provider of the same name and kind from the current config (the saved record holds no command). It refuses for a found worktree, a running, archived, rejected or integrated agent, one whose worktree changed or is gone, and when no such provider is configured.

- An ACP agent whose saved session id exists, and whose agent advertised `loadSession` when it last ran, is started with `session/load`: the agent replays the conversation into the transcript (not into the history), and the agent then waits for a prompt. If the agent no longer advertises it, or answers `session/load` with an error, a new session starts and the transcript says it has no earlier conversation. Without a saved session id or the capability, a new session starts and the transcript says why. No prompt is sent either way; `f` sends one.
- A terminal agent runs its provider command with every argument that held `{task}` left out, and nothing is typed in. The status line says so; `C-\` gives it the keys.

### Found worktrees

The same scan lists the repository's worktrees. A worktree on a `maestro/` branch under the agents' worktree directory that no record names (left by an earlier version or a crash) shows in the panel as a found worktree, named `found-<directory>`, with "found worktree" where the status goes. Its review compares it with where it forked from the main worktree's current branch. It can be reviewed (`Enter`), archived (`A`, with the same confirmations as any archive) or adopted (`a`, which asks for a provider and makes it a saved record with status stopped, ready for `R`). Tests, feedback, comments, drop file, compete, reject and integrate refuse until it is adopted. Found worktrees are not saved; an archived one is gone the next time.

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
| waiting for a prompt | a relaunched ACP agent's session is up and no turn runs |
| stopped | no process runs and Maestro did not see the agent finish: a record that was still working when it was saved, an adopted found worktree, or a relaunched ACP agent that exited while waiting for a prompt |

Once rejected or integrated, agent events no longer change the status. `maestro_agent::AgentStatus` has no state for "working" or for a running terminal program, so the editor has its own status type with those two added. Each status is drawn with its name next to its color.

## The panel and review screen

`space a` or `:agents` opens the panel over the editor area. The keys are listed on its last line. The review screen (`Enter` on an agent) shows the diff of the worktree against the commit it started from: commits on its branch, staged, unstaged and untracked files. Diffs are computed on a background thread when the panel opens, when an ACP turn ends or the agent writes a file, when a process exits, after integration, every three seconds for running agents while the panel is open, and on `r`.

Terminal agents have no structured status. The panel says so ("terminal only") next to every terminal agent.

## Actions

- Tests (`t`): runs `[agents] test-command`, or `cargo test` when the worktree has a `Cargo.toml`, with `maestro-tasks`. Output streams into the tests view; the totals are parsed from `cargo test` output.
- Drop a file (`D` in the review): discards the agent's change to the file under the cursor in its worktree, after a dialog that says what happens to it (restored to the starting commit's version, or deleted if the agent added it). Refused while that file has unsaved edits in a buffer. This is how to keep stray files (build output, caches) out of an integration; there is no partial staging of hunks.
- Feedback (`f`): unsent comments plus typed text. An ACP agent gets them as a new prompt (refused while a turn is running). A terminal agent gets them typed into its terminal, and the input box says that is all Maestro can do.
- Permission (`p`): lists the choices of the oldest unanswered request (requests queue; the transcript says how many more wait). Choices that allow something ask for confirmation first; rejecting is sent at once.
- Reject (`X`): stops the agent and its tests and marks it rejected. `a` in the dialog also archives the worktree. Rejecting while the worktree is still being made means the agent is never launched.
- Relaunch (`R`) and adopt (`a`): see Records across restarts.
- Archive (`A`): removes the worktree with `remove_worktree`, keeping the branch, after the agent's program has exited. It refuses when Git lists the worktree on another branch than the record names. Ignored files in it are deleted without a second question, and the dialog says so. If the worktree has uncommitted or untracked files, or commits on no branch, the removal is refused and a second dialog lists what forcing would delete; only `F` there forces it.
- Integrate (`I`): checks first and refuses, changing nothing, if either worktree is gone or on another branch, the main worktree is detached or has uncommitted changes, the agent's starting commit is not on the main worktree's branch, or an open buffer in either worktree has unsaved edits. The dialog says what will be committed and how the branch will be integrated (`tab` cycles merge, fast-forward and cherry-pick). On `y`, the same checks run again; then uncommitted work in the agent's worktree is committed with the task's first line as the subject, and `integrate` runs in the main worktree. `maestro-git` checks again and refuses on its own.
- A dialog that does not fit the window cannot be accepted.
- `:q` and `:qa` refuse while Git work for an agent runs (a worktree being made, an integration, an archive). With agents or tests running they ask first, and on `y` stop them and wait up to a second for terminal programs to exit. The automation API's `editor.quit` refuses the same way while Git work runs, and refuses while agents or tests run unless `force` is passed, which stops them first.

## Not built

- Two Maestro instances in one repository: each keeps the other's new records when it saves, but a change both make to one record (an archive in one, a comment in the other) keeps only the last save's. A record whose agent still runs in the other editor loads here as not running, and Maestro cannot tell.
- Relaunch does not send the task again, and a relaunched terminal agent gets no task at all.
- Agents cannot propose edits to open buffers; there is no edit proposal or review status per buffer edit.
- No OS sandbox around agent processes (see `docs/security.md`).
- Terminals show only in the panel, not in splits.
