# 0007: Direction after M3

Status: accepted, November 1, 2026.

## Position

Cursor puts one agent inside the editor and is built to generate code fast. Orca and herdr manage many agents: Orca as a desktop app with a worktree per task, herdr as a terminal multiplexer that knows when an agent is blocked, working or done. Maestro's place is the step after generation: judging, steering and combining agent work at keyboard speed, with guarantees about what reaches your branch. Under that sits an editor that has to be good with every agent feature off.

An idea from another tool is taken when it speeds up editing or the review loop, fits the selection-first model, and can be built and checked to the same standard as the rest. Competing with Neovim's thousands of plugins, VS Code extension compatibility and a chat panel as the main surface are out.

## Taken from other tools

- Orca: a sidebar of projects and their worktrees, where choosing one switches the editor to it; a jump palette across them; GitHub pull requests, issues and checks next to the work.
- herdr: every agent's state at a glance (blocked, working, done, idle), ordered by what needs you, with a notification when one needs you; agents that keep running when the editor detaches; agents driving the tool through the socket (spawn, read output, wait on each other).
- Magit: a keyboard Git status screen with staging of files, hunks and selected lines, commit, log and blame.
- Helix and Kakoune: already the editing model.

## New

- Intent edits: an agent sends a structural pipeline instead of a text diff, and a stale proposal is rebased by running the pipeline again on the current text.
- Hunk-level integration, including one result composed from several agents' hunks.
- Review sorted by kind of change: mechanical (renames, moves, formatting) apart from logic.
- Provenance that lasts: who wrote each line (you, which agent, under which contract, verified by what), in Git notes, with an agent-aware blame.
- Contracts checked while the agent runs, not only at review.

## Workstreams

Wave 1, in parallel worktrees: workspaces sidebar and jump palette (`space s`, `space j`), Git status screen and hunk keys (`space g`, `space h`), GitHub inbox through `gh` (`space i`), intent edits, hunk-level and composed integration. Wave 2, after those merge: review by kind of change, lasting provenance, live contracts, detachable agents, agent orchestration over the socket.

Each workstream follows AGENTS.md: real checks through the release binary, the three gates, an independent review, docs that say only what was checked.
