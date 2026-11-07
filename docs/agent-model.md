# Agent model

This describes what is built as of November 7, 2026. Anything marked partial or not built is not there.

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
- the files Maestro's verification runs created in the worktree, each with its contents' blob id (below);
- errors, and the files changed according to the last review diff;
- review comments, each on one line of one file, marked once sent;
- for ACP agents, the session id and whether the agent advertised `loadSession`;
- the task contract, when one was attached (below);
- hunks marked in the review, and the parts of its work integrated on their own (below);
- for a composed result, which hunk came from which agent (below).

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
- Checked at integrate: the dialog names every file outside the contract, computed from Git when the dialog opens: files in the net diff from the agent's starting commit, and files any commit on the agent's branch touched (a file changed and changed back does not show in the net diff, but a cherry-pick applies each commit). `y` is then refused and only `O` integrates; `D` in the review drops a file instead (a file only commits on the branch touched stays listed, because the commits stay). A dialog too long for the window cannot be accepted. After the agent's work is committed, Maestro checks that the commit to integrate is the one the dialog was computed on (see Integrate below), that it holds exactly those files, and that no file outside the contract is in it or its history that the dialog did not name; otherwise nothing is integrated (the commit stays on the agent's branch) and `I` shows the new state. Only that commit is integrated, so a commit that reaches the agent's branch after the check is refused or left out, never merged. After the merge, Maestro compares the target branch before and after against the contract and reports, as an error, any file outside it that the dialog did not name; given the checks before, this should not find anything. When Maestro made the commit, its message names the files `O` let in; the agent's history names them either way.
- Verification: `t` runs the contract's command instead of `[agents] test-command`. Before the command starts, Maestro takes a snapshot of the worktree (see Overlaps and conflicts) on the run's background thread, so it records the files the run started on. The integrate dialog warns, without blocking, when no verification has run (only when some verification command is set), when the last one is still running or did not pass, when the files changed since it started (it names them; files the tests wrote themselves count too), and when the agent's process was running during it.
- Acceptance criteria are shown in the contract dialog and sent to the agent. Maestro does not check them.

## Files the verification run creates

`t` runs a command in the agent's worktree, and the command can leave files the agent never wrote: `cargo test` writes a `Cargo.lock` in a repository that does not commit one. Before this, the review counted such a file as the agent's and integrate committed it.

Before the command starts, on the run's background thread, Maestro lists the worktree's untracked files that are not ignored (`git ls-files --others --exclude-standard`). When the command and the processes it left have ended, it lists them again. A file in the second list and not the first is the run's when it is a regular file (not a symlink or a directory) and the agent's starting commit has no file at that path. It is recorded on the agent with its blob id (`git hash-object`, the id `git add` would store). At most 1,000 are recorded; the history says how many more counted as the agent's. A file the agent had before the run stays the agent's even if the run rewrites it.

A recorded file is the run's only while it is exactly as the run left it: untracked, not ignored, with the recorded blob id. Then:

- The review lists it after the agent's files, with `T` before its change letter and "test run" where the line counts go. Its summary rows say "T added, created by Maestro's test run" and how many such files there are, and the lines above its diff name the command and say it is not counted, integrated or composed. When the agent's process was running during the run, they say so, since the agent may have written the file.
- It is left out of the file, line and contract counts in the panel and the review, out of overlaps and their dry runs, out of the "finished, no changes" check, and out of the compose view. Its hunks cannot be marked.
- Integrating the whole work leaves it out. The dialog has a "LEFT BEHIND" line naming each such file and the command, and step 1 commits everything else. The dialog's snapshot and Maestro's commit both stage every path but these, each excluded by its exact name (`:(top,exclude,literal)<path>`), and `maestro-git` lets exactly these untracked files stay in the agent's worktree when it checks that the worktree is clean. On `y`, Maestro refuses when the agent's records of these files changed since the dialog was checked (a run ended meanwhile), and checks the files again before committing and after: if one changed since the dialog, nothing is integrated (a commit already made stays on the agent's branch). A change in the moment between that last check and the merge is not seen; the file is then left out though it is the agent's, and stays in the worktree. Parts and composed results never include it, because it cannot be marked.
- The verification snapshot of a later run leaves out the earlier runs' files that are still as they were, as integrate does, so a run's own `Cargo.lock` never shows up as "the files changed after the last verification".

Once the file's contents change, or it is staged or committed, it is the agent's: counted, and committed by integrate. `K` on it in the review does the same at once, for a file the agent did write while the run was going; it is refused while a test run is going, since that run would record the file again. A later run that finds the file still as the last run left it keeps it as the run's, with the contents that run leaves. Nothing deletes the file; `D` in the review does, after asking.

Not handled: when the agent deleted a tracked file and the run creates a similar one, Git can show the pair as a rename; the review then shows the rename as the agent's, while integrate still leaves the new file untracked and commits only the deletion. A record whose path is not valid UTF-8 is not saved, so after a restart that file counts as the agent's. A tracked file the run modifies (a lock file the repository does commit) is still counted as the agent's change, and the integrate dialog's verification warning names it. A file the agent writes during the run is taken for the run's until `K`.

## Overlaps and conflicts between agents

Two agents of the same repository overlap when their last review diffs name the same file (a rename counts for both its paths), or when one names a path that is a directory of a path the other names (one adds the file `assets`, the other `assets/icon.png`). Agents that are found worktrees, archived, rejected or integrated, or whose worktree changed branch, are left out. The panel shows each agent's overlaps in the list (`overlaps beta-2`, or `CONFLICTS beta-2`) and in full on the sixth summary row (`overlaps beta-2 in README.md: CONFLICT in README.md`, `would merge cleanly`, `checking whether they conflict`, or why the dry run failed).

For each overlapping pair, a background thread takes a snapshot of both worktrees and runs a merge dry run between them (`git merge-tree`). A snapshot is the worktree's files as they are on disk (`HEAD` plus staged, unstaged and untracked files, ignored files left out) written as a commit that no branch or ref points at, built in a temporary copy of the index. The snapshot is refused if `HEAD` moves while it is taken. Its id depends only on the files and `HEAD`, so the dry run's result is cached by the repository and the pair of snapshot ids. A change to the repository's merge configuration (merge drivers, `merge.*` settings) does not clear the cache.

What it costs:

- Overlap is found from the review diffs the editor already has, with no Git. Agents whose files overlap nobody else's cost nothing more.
- The pairs are recomputed only when some agent's review diff or its `HEAD` commit changed, when a review diff holds a binary file (its diff does not show the contents, so every refresh counts), or when an agent was rejected, integrated or archived. Review diffs refresh every three seconds for running agents while the panel is open, and on the events listed under The panel and review screen. One recomputation runs at a time; requests that arrive meanwhile are folded into one rerun.
- Each recomputation takes one snapshot per agent in an overlapping pair (a `git add` into the index copy, which rehashes only files whose stats changed, then `write-tree` and `commit-tree`), and runs `merge-tree` only for pairs whose snapshot ids are not cached. At most 256 results are kept; when new ones would not fit, the old ones are dropped.

The integrate dialog's check below uses the other agents' last review diffs to pick which of them overlap; an agent whose new file has not reached its review diff yet is not checked.

The integrate dialog for one agent first says, on its own line, when this agent's work conflicts with the target branch now (Git would refuse the merge after committing the work on the agent's branch). It then checks the other overlapping agents against the target branch as it would be afterward: the target's `HEAD` merged in memory with this agent's snapshot, written as an unreferenced commit, then a dry run of that with each other agent's snapshot. The dialog names the agents that would then conflict and the files, the overlapping ones that would still merge cleanly, and any it could not check (all of them when this agent's own work conflicts with the target). The dry runs model a merge, whichever method the dialog is set to.

## Integrating part of the work

In an agent's review, `space` marks the hunk under the cursor or unmarks it, and `v` starts a line range: `j`/`k` extend it (the gutter shows `~`), and `space` or `v` marks the added and removed lines in it, or unmarks them when all are marked already. A range covers one hunk; lines of other hunks in it are left out, and the status line says so. A marked hunk shows `*` in the gutter and `[marked]` or `[2 of 5 lines marked]` after its header; marked lines show `*`; the header counts the marked hunks and lines. Hunks of binary, renamed or copied files, and files in conflict, cannot be marked.

A mark holds the hunk as the review showed it. When a refreshed review no longer has that hunk (the agent changed its lines or the file's mode), the mark is dropped, in the review and in the compose view, and the status line says how many were.

With marks, `I` integrates only them. On a background thread Maestro checks both worktrees, the main worktree (clean, on a branch) and that the agent's starting commit is on its branch, as for a whole agent, and that the review the marks come from descends from that commit. It then applies the marked hunks to the starting commit's files in memory (`maestro_git::Repository::tree_from_hunks`): a taken line must be what the base has there, and a removed line left out stays. A marked hunk that does not apply alone is refused by name, with why: lines that are not what the base has, a file added where the base has a file or a directory, or a file added under a path the base has as a file that no marked hunk removes (the agent turned a file into a directory, or into a symlink, and only half of that is marked), or lines that would follow a last line with no newline. The dialog lists every marked hunk (file and line ranges), says the rest stays on the agent's branch and in its worktree, and names the new branch `maestro/<agent>-part-<n>`. It warns that Maestro has not tested these hunks alone. `tab` changes the method, and `O` is needed for files outside the task contract, as for a whole agent.

On `y` Maestro first refuses if the marks are no longer the ones the dialog listed (a review refresh while it was up unmarked a hunk the agent changed). The checks then run again. Maestro builds the same tree again through a temporary index (the real index and both working trees are not touched), refuses if it differs, commits it with the starting commit as its only parent and the user's Git identity, creates the new branch at that commit (never moving an existing one), and integrates exactly that commit with `integrate_reviewed`: the same contract checks before and after, the same check of the target branch and base, and the same refusal of a dirty main worktree. The commit message lists the hunks. If integrating fails after the commit was made, the branch stays and the status line names it.

Afterward the agent keeps its worktree and status; the review shows the hunks that went in with `=` and `[integrated in part 1]`, and they cannot be marked again. The rest can be integrated with more marks, all at once with `I` and no marks, or rejected. Lines left in a hunk that went in only in part cannot be marked: each part is built on the starting commit, so they would change the same lines as the part already there. They go in only with the whole work, where Git reports a conflict if they clash with the part. Not carried: mode changes of a modified file (it keeps the base's mode), and added lines whose text was not valid UTF-8 (refused, since the review holds them with U+FFFD).

## Composing one result from several agents

`C` on an agent opens the compose view for its task group: the agents started on the task with `N` that are not rejected, integrated or archived, and not composed results themselves. It needs two of them, started from the same commit. The left side lists every file any of them changed; for the chosen file, each agent has a column with its name and last test result, and its hunks under it. Hunks that cover the same lines of the starting commit sit in one band across the columns, so the alternatives line up. `j`/`k` move between hunks, `h`/`l` between agents, `tab` between files, and `space` marks or unmarks a hunk (`[x]`).

`ret` checks, on a background thread, that every mark is still in its agent's review and that the marked hunks apply together on the common starting commit. Two hunks that change the same or neighboring lines of a file conflict; each is named with its agent (`calc.py -1,5 +1,5 from alpha-1: it conflicts with calc.py -1,6 +1,6 from beta-2`) in the status line and marked `[!]` in the view. Otherwise a dialog lists every hunk with its agent and that agent's test result, and `y` makes the result: a commit of exactly those hunks on the starting commit (with the user's identity; its message lists which hunk came from which agent and its test result), on a new branch `maestro/compose-<id>`, checked out in a new worktree next to the agents'. The agents' worktrees and branches are not touched.

The result is an entry in the panel named `compose-<id>`, of origin composed, with the group's task and contract and status ready for review. No agent runs in it: messages and relaunch refuse, and the panel says so. `ret` reviews it, `t` runs the verification command in its worktree, `D`, `X` and `A` work as for an agent, and `I` integrates it through the same path as any agent's work. Its record, with where each hunk came from, is saved with the agent records.

## Records across restarts

When sessions are on (the default; not with `--clean`, headless runs or `restore-session = false`), agent records are saved per repository in the state directory, `$XDG_STATE_HOME/maestro/agents/<hash of the main worktree's canonical path>.json`, next to the editor sessions. A record holds the id, name, group, task, provider name and kind, worktree (path, branch, starting commit, main worktree), status, the archived flag, how the process ended, errors (last 100), history (last 500 events), comments, test runs (last 10, each with its exit code, duration, `cargo test` totals, last 100 output lines and the snapshot tree of the files it ran on), the ACP session id, the task contract, the parts integrated on their own (branch, commit and hunks), the files its verification runs created (path, blob id, command, and whether the agent was running; a path that is not a plain relative path inside the worktree makes the file unusable), and for a composed result its origin and sources. Marks are not saved. A saved contract is checked like a typed one when the file is read. It does not hold the provider's arguments or environment, the ACP transcript, the agent's stderr or the terminal screen; the history names the program that was started. The file has a version number.

Saving is coalesced. About once a second Maestro checks whether anything a record holds changed (an agent created, a status change, a decision, a test run started or finished, a comment, an archive, a session id) and, if so, writes the file once; a test run's output is saved when the run ends. Quitting saves again with the file synced to disk.

The records of the editor's repository are read when the agent panel first opens, not at startup, on a background thread: finding the main worktree takes Git, and a session that never opens the panel runs no Git for it. Starting an agent waits until they are read, so ids continue after the highest saved one and a new branch never reuses a saved name. A records file that cannot be used (not JSON, another version, written for another repository, a record with an unknown kind, a relative path, a branch outside `maestro/`, a base that is not a commit id, an id out of range) is reported in the status line and left as it is, and nothing is saved for that repository in that session, so it is never overwritten. Before each save the file is compared with what this session last read or wrote. If another Maestro saved in between, its records for worktrees this session does not know are added (as loaded agents) so the save keeps them; records both sessions have are saved as this session has them. If the file became unusable, saving stops and the file is left alone. A record whose id an agent of this session already has gets a new id and name, and a group already in use gets a new group; the history says so.

A loaded agent has no process. A status that needs one (starting, working, planning, editing, awaiting approval, running, running tests, waiting for a prompt) loads as **stopped**, and its history says what it was. Every loaded agent is marked "not running (from an earlier session)" next to its status in the panel, and its live view says the transcript and screen were not saved. Review, tests, comments, drop file, reject, archive and integrate work on it as on any agent. A message (`m`) is refused with a note that says to relaunch it first.

When records load, each one's worktree is checked against `git worktree list`. If Git no longer lists it, its directory is gone, or it is on another branch than the record names, the history says so and relaunch and tests refuse; archive refuses while the directory still exists (when it is gone, archiving only marks the record), and the review shows no diff for a worktree on another branch. The check is repeated at the moment of use for every agent, not only at load: tests and relaunch read the worktree's `.git` file and `HEAD`, and drop file, archive and integrate ask Git, so a worktree switched to other work later is refused too.

### Relaunch

`R` starts an agent with no process again in its worktree, with the provider of the same name and kind from the current config (the saved record holds no command). It refuses for a found worktree, a running, archived, rejected or integrated agent, one whose worktree changed or is gone, and when no such provider is configured.

- An ACP agent whose saved session id exists, and whose agent advertised `loadSession` when it last ran, is started with `session/load`: the agent replays the conversation into the transcript (not into the history), and the agent then waits for a prompt. If the agent no longer advertises it, or answers `session/load` with an error, a new session starts and the transcript says it has no earlier conversation. Without a saved session id or the capability, a new session starts and the transcript says why. No prompt is sent either way; `m` sends one.
- A terminal agent runs its provider command with every argument that held `{task}` left out, and nothing is typed in. The status line says so; `i` or a click in its screen gives it the keys.

### Found worktrees

The same scan lists the repository's worktrees. A worktree on a `maestro/` branch under the agents' worktree directory that no record names (left by an earlier version or a crash) shows in the panel as a found worktree, named `found-<directory>`, with "found worktree" where the status goes. Its review compares it with where it forked from the main worktree's current branch. It can be reviewed (`Enter`), archived (`A`, with the same confirmations as any archive) or adopted (`a`, which asks for a provider and makes it a saved record with status stopped, ready for `R`). Tests, messages, comments, drop file, compete, reject and integrate refuse until it is adopted. Found worktrees are not saved; an archived one is gone the next time.

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

Once rejected or integrated, agent events no longer change the status. `maestro_agent::AgentStatus` has no state for "working" or for a running terminal program, so the editor has its own status type with those two added. A permission request is shown over running tests: the agent waits for you either way.

### State words

One word names each state, the same in the agent panel's list and summary, the review screen, the sidebar's agent rows and worktree rows, the jump palette and notifications (`agents::state_word`). Before the word every view draws a mark (`!` needs you, `~` working, `-` idle, `=` done), and both are drawn in the state's color, so color is never the only signal. The sidebar's group headings (needs you, working, idle, done) are groups of these states, not words for an agent.

| State | Word | Mark | Sidebar group |
|---|---|---|---|
| starting | starting | `~` | working |
| working | working | `~` | working |
| planning | planning | `~` | working |
| editing | editing | `~` | working |
| running | running | `~` | working |
| running tests | running tests | `~` | working |
| awaiting approval | awaiting approval | `!` | needs you |
| ready for review | ready for review | `!` | needs you |
| blocked | blocked | `!` | needs you |
| failed | failed | `!` | needs you |
| waiting for a prompt | waiting for a prompt | `-` | idle |
| stopped | stopped | `-` | idle |
| finished, no changes | finished, no changes | `=` | done |
| rejected | rejected | `=` | done |
| integrated | integrated | `=` | done |
| archived (the worktree was removed, whatever the status was) | archived | `=` | done |
| found worktree (no record) | found worktree | `-` | idle |

When the last verification run did not pass (a nonzero exit, killed, or it could not start) and the agent is not working, decided or archived, it goes to needs you with the mark `!` and keeps its word; the sidebar and the palette add `, tests failed` after it (`! stopped, tests failed`), the notification says `tests failed`, and the panel shows the result on the list's second line (`FAIL 3/40`, `exit 2`). Notifications use the word of the state that needs you: `alpha-1 needs you: awaiting approval`. An archived agent's list line says what its status was (`was ready for review`). Saved records keep the old status keys (`ready-for-review`, `idle`, ...), so records written before these words load unchanged.

## The panel and review screen

`space a` or `:agents` opens the panel over the editor area. The keys are listed on its last line. A key is listed for the selected agent only when its action would not refuse, judged by the same function the action checks first (`review::blocker`, `actions::test_blocker`, `message_blocker`, `reject_blocker`, `archive_blocker`, `integrate_blocker`, `adopt_blocker`, `forget_blocker`, `compose::blocker`, `Agent::relaunch_refusal`), so the two cannot disagree. The rebase `I` offers is gated the same way by `rebase::blocker`: the dialog is shown only when it allows the rebase, and `y` checks it first. Some checks run only when the key is pressed, because they ask Git, read the worktree or search `PATH`: the worktree's branch on disk (tests, relaunch), unsaved buffers and the main worktree (integrate), whether the worktree directory still exists and its provider is still configured (relaunch), and whether a provider not yet on the task is left (`N`). Those can still refuse a listed key. The review screen's `m`, `t`, `I`, `X`, `D` and `c` are filtered the same way. An archived agent lists `v`, `N`, `d` and the panel's own keys, plus `C` while two other agents on its task can still be composed; an agent whose program exited in this session does not list `m` or `i`. `n` asks for a task (and shows its contract, if it has one), then for a provider.

A new task starts from the commit at the `HEAD` of the branch checked out in the repository's main worktree, the branch `I` integrates into, whichever worktree the editor is in. Git reports that commit (`git worktree list`) when the worktree is made, and the agent's history names it (`from main at 1a2b3c4d`). When the main worktree has a detached HEAD, its branch has no commits yet, or it is bare, the task starts from the `HEAD` of the worktree the editor is in instead, and the provider list says why; `I` refuses to integrate it until the main worktree is on a branch that contains that commit, or until the agent's work is rebased onto that branch (see Rebase onto the target). `N` starts from the commit the selected agent started from, so competing agents work on the same code.

While the provider list is up, a background thread checks where the agent will start, and the list shows it above the providers: `starts from main at 1a2b3c4d (the branch I integrates into)`, and, when the editor is in another worktree, `you are in maestro/add-mul-2; the new agent starts from main, not from here`. When the worktree it starts from has staged, unstaged or untracked files (ignored ones do not count), a line marked `!` gives their count and up to five names and says the agent will not see them unless they are committed first (stashing them does not give them to the agent either). It is a warning; the provider can still be chosen. When the window is too short for every note, the warnings are shown first and the last row says notes were left out. The list shows what Git reported when it opened. The worktree is made from the commit Git reports for the branch when the worktree is made, on a background thread just after the provider is chosen, so a commit, reset or checkout in the main worktree in between changes it; the agent's history names the commit it got. With `N`, the list names the commit and the agent it comes from, and runs no check (before that agent's worktree exists, `N` checks and starts as a new task does).

The review screen (`Enter` on an agent) shows the diff of the worktree against the commit it started from: commits on its branch, staged, unstaged and untracked files. Diffs are computed on a background thread when the panel opens, when an ACP turn ends or the agent writes a file, when a process exits, after integration, every three seconds for running agents while the panel is open, and on `r`.

Terminal agents have no structured status. The panel says so ("terminal only") next to every terminal agent.

## Actions

- Tests (`t`): runs the task contract's verification command if it has one, else `[agents] test-command`, or `cargo test` when the worktree has a `Cargo.toml`, with `maestro-tasks`. Files the run creates are recorded and kept out of the agent's work (see Files the verification run creates). Output streams into the tests view; the totals are parsed from `cargo test` output. When the command exits, processes it left running in its process group (a server started with `&`, say) get SIGHUP, then SIGKILL once nothing else is left in the group or after 200 ms, before the run counts as finished; a process that moved to another process group or session is not reached. This goes through the `kill` program (each call waits for it at most a second); if `kill` is missing or fails, the leftovers keep running. On Linux, when Maestro was started with SIGCHLD ignored (the kernel then reaps children by itself, so no child's pid is safe to signal), no process group is signaled at all and leftovers keep running; elsewhere that case cannot be detected.
- Drop a file (`D` in the review): discards the agent's change to the file under the cursor in its worktree, after a dialog that says what happens to it (restored to the starting commit's version, or deleted if the agent added it). Refused while that file has unsaved edits in a buffer, while other Git work for the agent runs, when the worktree no longer has the agent's branch checked out or no longer descends from its starting commit, and when the path (or a parent of it) is a directory or a symlink. A file the agent turned into a symlink, or the reverse, is restored whichever half of the change is dropped. This is how to keep stray files (build output, caches) out of an integration, though files Maestro's own test run created are left out without it; to integrate only some hunks, mark them instead (see Integrating part of the work).
- Message (`m`, formerly `f` for feedback, which still works): unsent comments plus typed text. An ACP agent gets them as a new prompt (refused while a turn is running). A terminal agent gets them typed into its terminal followed by Enter. To type into a running terminal agent directly, `i` or a click in its screen gives it the keys, and `C-\` or a click outside takes them back.
- Permission (`p`): lists the choices of the oldest unanswered request (requests queue; the transcript says how many more wait). Choices that allow something ask for confirmation first; rejecting is sent at once.
- Reject (`X`): stops the agent and its tests and marks it rejected. `a` in the dialog also archives the worktree. Rejecting while the worktree is still being made means the agent is never launched. It refuses for an agent already rejected, integrated or archived (nothing is left to reject once the worktree is gone).
- Relaunch (`R`) and adopt (`a`): see Records across restarts.
- Archive (`A`): removes the worktree with `remove_worktree`, keeping the branch, after the agent's program has exited. It refuses when Git lists the worktree on another branch than the record names. Ignored files in it are deleted without a second question, and the dialog says so. If the worktree has uncommitted or untracked files, or commits on no branch, the removal is refused and a second dialog lists what forcing would delete; only `F` there forces it.
- Forget a record (`d`): removes an archived agent's record from the panel and, when records are saved, from the records file at the next save (within about a second, or on quit), so it does not come back after a restart. It asks y/n first; the dialog says nothing on disk is deleted (the worktree was removed when it was archived) and that the branch stays with its commits. It refuses for an agent that is not archived, since its worktree would be left with no record (it would come back as a found worktree), and while Git work for it runs. Records of that worktree that another Maestro saves into the file are not added back in this session. With sessions off, or a records file that cannot be used, it only removes the agent from the panel, and the dialog says so.
- Integrate (`I`): checks first and refuses, changing nothing, if either worktree is gone or on another branch, the main worktree is detached or has uncommitted changes, the agent's starting commit is not on the main worktree's branch (see Rebase onto the target below), or an open buffer in either worktree has unsaved edits. The dialog says what will be committed and how the branch will be integrated (`tab` cycles merge, fast-forward and cherry-pick). The dialog is computed from one snapshot of the agent's worktree: the commit its branch points at plus its uncommitted and untracked files. On `y`, the same checks run again; then uncommitted work in the agent's worktree is committed with the task's first line as the subject. What may be integrated is exactly the snapshot: the branch's commit from the dialog, or Maestro's commit of the same files whose only parent is that commit. If the agent committed, reset or rebased after the dialog was checked, or changed the files, nothing is integrated ("the agent's branch moved after the dialog was checked" or "the agent's files changed after the dialog was checked"; a commit Maestro made stays on the agent's branch), and `I` shows the new state. Then `maestro-git` integrates that commit only: it refuses if the branch no longer points at it, both when its checks start and again just before git runs, and gives git the commit id rather than the branch name, so a commit that lands on the branch after that last check is not taken in. It also refuses when the main worktree no longer has the dialog's target branch checked out, or when that branch no longer contains the agent's starting commit (a switch or reset while Maestro committed). Maestro's Git commands ignore replacement refs (`git replace`). `maestro-git` checks the rest again and refuses on its own. With a task contract, files outside it need `O` instead of `y`, and the dialog warns about the last verification and names agents that would conflict afterward (see Task contracts and Overlaps).
- Rebase onto the target (offered by `I`): when the agent's starting commit is not on the main worktree's branch (it was started from another agent's worktree, or that branch was reset), `I` refuses and names the commits the base has and the branch lacks, newest first: up to three as short id and subject, then "and N more". The refusal goes to the status line (and so to `:messages`) and the agent's history. It then offers, in a y/n dialog, to move the agent's own work onto the branch's `HEAD`: the equivalent of `git rebase --onto <target> <base> <agent-branch>` in the agent's worktree. The dialog repeats the refusal and says what `y` does: which commits move (up to three named, then how many more), onto which commit, that the agent's branch is rewritten, and that its tip before the rebase is kept as the branch `<agent-branch>-before-rebase` (`-2`, `-3` and on when that name is taken), which `git reset --hard <that branch>` in the worktree puts back. The offer is made only when `rebase::blocker` allows it, and `y` checks the same function first: it refuses while the agent's process or a test run is going, while other Git work for it runs, for a found, archived, rejected or integrated agent, when its worktree changed or is gone, and when parts of its work were already integrated on their own. The background check before the dialog refuses, with the reason in the status line, when the worktree is missing, not on its branch (detached included) or in the middle of a merge or rebase, when the agent has nothing of its own to move, when its branch has merge commits, and when a dry run (each commit, then the uncommitted work, applied in memory with `merge-tree`, needing git 2.45) finds a conflict, which it names by file. When the dry run cannot run, the dialog says so and `y` still works.
  - Uncommitted work: Maestro does not refuse it. Like integrate, `y` first commits the worktree's uncommitted and untracked files (not Maestro's test-run files, which stay untracked) with the task's first line as the subject, and the dialog names those files. It does this because terminal agents often leave their work uncommitted, and refusing would leave no way forward inside Maestro. The commit is made only on the files and target the dialog's dry run passed on, and must hold exactly those files.
  - On `y` the checks run again: the main worktree is still on the branch at the same `HEAD`, the agent's branch and files are what the dialog was checked on, and the test run's files are unchanged. Then the old-tip branch is made and git rebases with `--merge --no-autostash --no-update-refs --no-rebase-merges --no-autosquash --no-fork-point --empty=drop`, so the user's rebase settings cannot stash, move other branches or reorder commits. Hooks run. Commits whose change the target already has are dropped, as `git rebase` does.
  - If git stops on a conflict (or fails another way), the rebase is aborted, the branch is checked to be back at the commit it was on with no tracked file changed, the old-tip branch is deleted, and the status line names the conflicting files. When Maestro committed uncommitted work first, that commit is undone with `reset --soft`: the branch is where it was, the files are unchanged, and the work stays staged rather than unstaged or untracked. If putting things back fails, the status line says what was found and that the old tip is kept as the branch.
  - After a rebase the agent's recorded starting commit is the target's `HEAD`, its marks are cleared, the review and overlaps refresh, and the status line says how many commits moved and names the old-tip branch. `I` then integrates only the agent's work. When the agent was tested before, that run was on the old files, so the integrate dialog says the files changed since it.
  - `I` with marked hunks names the missing commits the same way but does not offer the rebase.
- A dialog that does not fit the window cannot be accepted.
- `:q` and `:qa` refuse while Git work for an agent runs (a worktree being made, an integration, a rebase, an archive). With agents or tests running they ask first, and on `y` stop them and wait up to a second for terminal programs to exit. The automation API's `editor.quit` refuses the same way while Git work runs, and refuses while agents or tests run unless `force` is passed, which stops them first.

## Not built

- Two Maestro instances in one repository: each keeps the other's new records when it saves, but a change both make to one record (an archive in one, a comment in the other) keeps only the last save's. A record whose agent still runs in the other editor loads here as not running, and Maestro cannot tell. A record forgotten (`d`) in one stays in the other's memory, and that one's next save writes it back to the file; the session that forgot it does not load it again, but a later session does.
- Relaunch does not send the task again, and a relaunched terminal agent gets no task at all.
- Agents started from the panel cannot propose edits to open buffers. An automation client can (`proposal.create`, see `docs/automation.md`), and an agent that speaks the automation protocol could do so as a client, but the panel does not connect its agents to it.
- No OS sandbox around agent processes (see `docs/security.md`).
- Terminals show only in the panel, not in splits.
- A task contract cannot be edited after the task starts, and nothing checks the acceptance criteria. Contracts do not confine terminal agents or an ACP agent's own tools; only writes through Maestro are refused.
- Conflict detection covers agents of one repository against each other and, in the integrate dialog, against the target branch. It does not watch the target branch on its own, and the panel does not show which agents already conflict with it.
