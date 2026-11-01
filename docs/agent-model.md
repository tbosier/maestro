# Agent model

This describes what is built as of November 1, 2026. Anything marked partial or not built is not there.

## Version-checked edits

`Document::apply_at_version(tx, expected_version, selection)` applies a transaction only if the document is still at the version the edit was computed against. Otherwise it returns `EditError::Stale` and changes nothing. Separately, every `ChangeSet` records the length of the document it was built for and refuses to apply to any other length.

Edits produced away from the keyboard and computed against a known version must go through `apply_at_version`. The automation API's `document.apply_edit` does, and so does approving an edit proposal. Its `command.run` and `editor.execute` do not: they run editor commands on the current text, as a key press would, with no version check and no entry in the edit history (see `docs/automation.md`).

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
- for ACP agents, the session id and whether the agent advertised `loadSession`;
- the task contract, when one was attached (below).

## Task contracts

A task can carry a contract: the paths it may change, paths it must not change, a verification command and acceptance criteria for the reviewer. It is attached when the task is created and does not change afterward. `N` starts the competing agent under the same contract.

### Writing one

In the task input (`n`), a line whose first word is one of these is a contract line and is taken out of the task text; every other line stays in the task (a line starting `@decorator` is task text):

| Line | Meaning |
|---|---|
| `@allow <pattern> ...` | paths the task may change, separated by spaces; several lines add up |
| `@forbid <pattern> ...` | paths it must not change, whatever `@allow` says |
| `@verify <command>` | the verification command; spaces separate words, single or double quotes group them; one per task |
| `@accept <text>` | one acceptance criterion, plain text for the reviewer |
| `@contract <path>` | read the contract from a TOML file instead; the path is relative to the editor's working directory |

`A-ret` starts a new line in the input, and pasting multi-line text works. A file and typed lines cannot be combined.

A contract file is read only when the user names it with `@contract`; Maestro never looks for one in the repository. It must be a regular file of at most 64 KiB, and unknown keys are refused:

```toml
allowed = ["calc.py", "tests/**"]
forbidden = ["secrets/**"]
verify = ["python3", "test_calc.py"]   # or one string, split like @verify
acceptance = ["add(2, 3) returns 5"]   # or one string
```

Patterns are relative to the repository root and anchored there: `calc.py` is only the top-level file, `*` matches within one directory, `?` one character, `**` any number of directories, and a pattern that names a directory covers everything in it (`src` and `src/` cover `src/a/b.rs`). An empty `allowed` list allows every path that is not forbidden. Matching is case-sensitive, on any file system. Absolute patterns, `..`, `.`, empty segments (one trailing `/` is allowed), `!` negation, backslashes, `**` twice in a row, more than three `**` or eight `*` in one segment, and patterns over 512 bytes are refused; the limits keep matching cheap, since the panel checks every changed file on each redraw. With any pattern, a file name that is not valid UTF-8 counts as outside the contract.

After `ret`, a dialog shows the whole contract (the verification command quoted word by word, with control and direction-changing characters escaped) and says what Maestro enforces; `y` goes on to the provider list, whose title says whether the task has a contract.

### What Maestro does with it

The contract is a request to the agent, plus a few checks Maestro can make itself. It does not confine the agent.

- Sent to the agent: an ACP agent gets the contract after the task in its first prompt; a terminal agent gets it after the task in the `{task}` argument or in the text typed in. A relaunch (`R`) sends neither the task nor the contract.
- Enforced for ACP file writes through Maestro: `fs/write_text_file` requests outside the allowed paths, or inside a forbidden one, are refused, judged on the file that would actually be written (after symlinks). The agent gets an error, and the refusal goes to the agent's errors and history. The agent's own tools (its shell, its own file writes) and every terminal agent are not stopped.
- Marked in the review: every changed file outside the contract has `!` before its change letter in the review's file list, a line above its diff that says which rule it breaks, a count in the review summary, and `! N outside contract` in the agent list.
- Checked at integrate: the dialog names every file outside the contract, computed from Git when the dialog opens: files in the net diff from the agent's starting commit, and files any commit on the agent's branch touched (a file changed and changed back does not show in the net diff, but a cherry-pick applies each commit). `y` is then refused and only `O` integrates; `D` in the review drops a file instead (a file only commits on the branch touched stays listed, because the commits stay). A dialog too long for the window cannot be accepted. After the agent's work is committed, Maestro checks that the branch holds exactly the files the dialog was computed on and that no file outside the contract is there that the dialog did not name; otherwise nothing is integrated (the commit stays on the agent's branch) and `I` shows the new state. A commit that reaches the agent's branch in the moment between that check and the merge is not stopped. After the merge, Maestro compares the target branch before and after against the contract and reports, as an error, any file outside it that the dialog did not name. When Maestro made the commit, its message names the files `O` let in; the agent's history names them either way.
- Verification: `t` runs the contract's command instead of `[agents] test-command`. Before the command starts, Maestro takes a snapshot of the worktree (see Overlaps and conflicts) on the run's background thread, so it records the files the run started on. The integrate dialog warns, without blocking, when no verification has run (only when some verification command is set), when the last one is still running or did not pass, when the files changed since it started (it names them; files the tests wrote themselves count too), and when the agent's process was running during it.
- Acceptance criteria are shown in the contract dialog and sent to the agent. Maestro does not check them.

## Overlaps and conflicts between agents

Two agents of the same repository overlap when their last review diffs name the same file (a rename counts for both its paths), or when one names a path that is a directory of a path the other names (one adds the file `assets`, the other `assets/icon.png`). Agents that are found worktrees, archived, rejected or integrated, or whose worktree changed branch, are left out. The panel shows each agent's overlaps in the list (`overlaps beta-2`, or `CONFLICTS beta-2`) and in full on the sixth summary row (`overlaps beta-2 in README.md: CONFLICT in README.md`, `would merge cleanly`, `checking whether they conflict`, or why the dry run failed).

For each overlapping pair, a background thread takes a snapshot of both worktrees and runs a merge dry run between them (`git merge-tree`). A snapshot is the worktree's files as they are on disk (`HEAD` plus staged, unstaged and untracked files, ignored files left out) written as a commit that no branch or ref points at, built in a temporary copy of the index. The snapshot is refused if `HEAD` moves while it is taken. Its id depends only on the files and `HEAD`, so the dry run's result is cached by the repository and the pair of snapshot ids. A change to the repository's merge configuration (merge drivers, `merge.*` settings) does not clear the cache.

What it costs:

- Overlap is found from the review diffs the editor already has, with no Git. Agents whose files overlap nobody else's cost nothing more.
- The pairs are recomputed only when some agent's review diff or its `HEAD` commit changed, when a review diff holds a binary file (its diff does not show the contents, so every refresh counts), or when an agent was rejected, integrated or archived. Review diffs refresh every three seconds for running agents while the panel is open, and on the events listed under The panel and review screen. One recomputation runs at a time; requests that arrive meanwhile are folded into one rerun.
- Each recomputation takes one snapshot per agent in an overlapping pair (a `git add` into the index copy, which rehashes only files whose stats changed, then `write-tree` and `commit-tree`), and runs `merge-tree` only for pairs whose snapshot ids are not cached. At most 256 results are kept; when new ones would not fit, the old ones are dropped.

The integrate dialog's check below uses the other agents' last review diffs to pick which of them overlap; an agent whose new file has not reached its review diff yet is not checked.

The integrate dialog for one agent first says, on its own line, when this agent's work conflicts with the target branch now (Git would refuse the merge after committing the work on the agent's branch). It then checks the other overlapping agents against the target branch as it would be afterward: the target's `HEAD` merged in memory with this agent's snapshot, written as an unreferenced commit, then a dry run of that with each other agent's snapshot. The dialog names the agents that would then conflict and the files, the overlapping ones that would still merge cleanly, and any it could not check (all of them when this agent's own work conflicts with the target). The dry runs model a merge, whichever method the dialog is set to.

## Records across restarts

When sessions are on (the default; not with `--clean`, headless runs or `restore-session = false`), agent records are saved per repository in the state directory, `$XDG_STATE_HOME/maestro/agents/<hash of the main worktree's canonical path>.json`, next to the editor sessions. A record holds the id, name, group, task, provider name and kind, worktree (path, branch, starting commit, main worktree), status, the archived flag, how the process ended, errors (last 100), history (last 500 events), comments, test runs (last 10, each with its exit code, duration, `cargo test` totals, last 100 output lines and the snapshot tree of the files it ran on), the ACP session id and the task contract. A saved contract is checked like a typed one when the file is read. It does not hold the provider's arguments or environment, the ACP transcript, the agent's stderr or the terminal screen; the history names the program that was started. The file has a version number.

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

- Tests (`t`): runs the task contract's verification command if it has one, else `[agents] test-command`, or `cargo test` when the worktree has a `Cargo.toml`, with `maestro-tasks`. Output streams into the tests view; the totals are parsed from `cargo test` output.
- Drop a file (`D` in the review): discards the agent's change to the file under the cursor in its worktree, after a dialog that says what happens to it (restored to the starting commit's version, or deleted if the agent added it). Refused while that file has unsaved edits in a buffer, while other Git work for the agent runs, when the worktree no longer has the agent's branch checked out or no longer descends from its starting commit, and when the path (or a parent of it) is a directory or a symlink. A file the agent turned into a symlink, or the reverse, is restored whichever half of the change is dropped. This is how to keep stray files (build output, caches) out of an integration; there is no partial staging of hunks.
- Feedback (`f`): unsent comments plus typed text. An ACP agent gets them as a new prompt (refused while a turn is running). A terminal agent gets them typed into its terminal, and the input box says that is all Maestro can do.
- Permission (`p`): lists the choices of the oldest unanswered request (requests queue; the transcript says how many more wait). Choices that allow something ask for confirmation first; rejecting is sent at once.
- Reject (`X`): stops the agent and its tests and marks it rejected. `a` in the dialog also archives the worktree. Rejecting while the worktree is still being made means the agent is never launched.
- Relaunch (`R`) and adopt (`a`): see Records across restarts.
- Archive (`A`): removes the worktree with `remove_worktree`, keeping the branch, after the agent's program has exited. It refuses when Git lists the worktree on another branch than the record names. Ignored files in it are deleted without a second question, and the dialog says so. If the worktree has uncommitted or untracked files, or commits on no branch, the removal is refused and a second dialog lists what forcing would delete; only `F` there forces it.
- Integrate (`I`): checks first and refuses, changing nothing, if either worktree is gone or on another branch, the main worktree is detached or has uncommitted changes, the agent's starting commit is not on the main worktree's branch, or an open buffer in either worktree has unsaved edits. The dialog says what will be committed and how the branch will be integrated (`tab` cycles merge, fast-forward and cherry-pick). On `y`, the same checks run again; then uncommitted work in the agent's worktree is committed with the task's first line as the subject, and `integrate` runs in the main worktree. `maestro-git` checks again and refuses on its own. With a task contract, files outside it need `O` instead of `y`, and the dialog warns about the last verification and names agents that would conflict afterward (see Task contracts and Overlaps).
- A dialog that does not fit the window cannot be accepted.
- `:q` and `:qa` refuse while Git work for an agent runs (a worktree being made, an integration, an archive). With agents or tests running they ask first, and on `y` stop them and wait up to a second for terminal programs to exit. The automation API's `editor.quit` refuses the same way while Git work runs, and refuses while agents or tests run unless `force` is passed, which stops them first.

## Not built

- Two Maestro instances in one repository: each keeps the other's new records when it saves, but a change both make to one record (an archive in one, a comment in the other) keeps only the last save's. A record whose agent still runs in the other editor loads here as not running, and Maestro cannot tell.
- Relaunch does not send the task again, and a relaunched terminal agent gets no task at all.
- Agents started from the panel cannot propose edits to open buffers. An automation client can (`proposal.create`, see `docs/automation.md`), and an agent that speaks the automation protocol could do so as a client, but the panel does not connect its agents to it.
- No OS sandbox around agent processes (see `docs/security.md`).
- Terminals show only in the panel, not in splits.
- A task contract cannot be edited after the task starts, and nothing checks the acceptance criteria. Contracts do not confine terminal agents or an ACP agent's own tools; only writes through Maestro are refused.
- Conflict detection covers agents of one repository against each other and, in the integrate dialog, against the target branch. It does not watch the target branch on its own, and the panel does not show which agents already conflict with it.
