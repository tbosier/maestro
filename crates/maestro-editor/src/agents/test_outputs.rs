//! Files Maestro's own verification run created in an agent's worktree.
//!
//! `t` runs a command such as `cargo test` in the worktree, and that
//! command can leave new files behind (a `Cargo.lock` in a repository that
//! does not commit one, a coverage report). The agent never wrote them, so
//! the review lists them apart and integrate and compose leave them out.
//!
//! What counts: just before a run starts, Maestro lists the worktree's
//! untracked files that are not ignored. When the run ends it lists them
//! again. A file in the second list and not in the first, that is a
//! regular file and that the agent's starting commit does not have, was
//! created by the run, and is recorded on the agent with its contents'
//! blob id ([`super::TestOutput`]).
//!
//! It stays the run's only while it is exactly as the run left it: still
//! untracked, with the same contents. Once the agent (or anyone) changes,
//! stages or commits it, it counts as the agent's again, and so it does
//! when the user presses `K` on it in the review. Files a run created
//! that are still as it left them when the next run starts carry over,
//! with the contents that run leaves. Nothing here deletes a file; `D` in
//! the review does, after asking.
//!
//! What it cannot tell: a file the agent writes while the run is going is
//! taken for the run's (the review says when the agent was running), and a
//! run that changes a file that already existed changes the agent's file.

use std::collections::BTreeSet;
use std::path::PathBuf;

use maestro_git::Repository;

use super::{AgentId, TestOutput};
use crate::editor::Editor;

/// At most this many files are recorded per agent; a run that creates
/// more has the rest counted as the agent's, and the history says so.
pub const MAX_TEST_OUTPUTS: usize = 1_000;

/// Which of `outputs` are still as their run left them: untracked, not
/// ignored, regular files with the recorded contents.
pub fn still_made_by_tests(
    repo: &Repository,
    outputs: &[TestOutput],
) -> Result<Vec<PathBuf>, String> {
    if outputs.is_empty() {
        return Ok(Vec::new());
    }
    let paths: Vec<PathBuf> = outputs.iter().map(|o| o.path.clone()).collect();
    let now = repo.untracked_blobs(&paths).map_err(|e| e.to_string())?;
    Ok(outputs
        .iter()
        .filter(|o| now.iter().any(|(p, b)| *p == o.path && *b == o.blob))
        .map(|o| o.path.clone())
        .collect())
}

/// What Maestro saw just before a run started.
#[derive(Debug, Clone)]
pub struct RunStart {
    /// Untracked, not ignored files then.
    pub untracked: BTreeSet<PathBuf>,
    /// Earlier runs' files that were still as those runs left them.
    pub still_made: Vec<PathBuf>,
    /// The agent's starting commit: a file it has is never the run's.
    pub base: String,
}

/// Reads the worktree before a run. `outputs` are the agent's records.
pub fn before_run(
    repo: &Repository,
    outputs: &[TestOutput],
    base: &str,
) -> Result<RunStart, String> {
    let still_made = still_made_by_tests(repo, outputs)?;
    let untracked = repo
        .untracked_files()
        .map_err(|e| e.to_string())?
        .into_iter()
        .collect();
    Ok(RunStart {
        untracked,
        still_made,
        base: base.to_string(),
    })
}

/// What the run left: the files it created, with their contents, and
/// how many more it created than are recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunEnd {
    /// `(path, blob)`, in path order.
    pub files: Vec<(PathBuf, String)>,
    pub not_recorded: usize,
}

/// Reads the worktree after a run: files created by it, plus earlier
/// runs' files that were still theirs at the start, as they are now.
pub fn after_run(repo: &Repository, start: &RunStart) -> Result<RunEnd, String> {
    let now = repo.untracked_files().map_err(|e| e.to_string())?;
    let mut new: Vec<PathBuf> = now
        .into_iter()
        .filter(|p| !start.untracked.contains(p))
        .collect();
    new.sort();
    // Bounded before asking Git about each, so a run that writes many
    // thousands of files cannot make the argument lists too long.
    let mut not_recorded = new.len().saturating_sub(MAX_TEST_OUTPUTS);
    new.truncate(MAX_TEST_OUTPUTS);
    // A path the agent's starting commit has is a change to that file
    // (the agent deleted it, the run wrote it again), not a new file.
    let in_base: BTreeSet<PathBuf> = repo
        .paths_in(&start.base, &new)
        .map_err(|e| e.to_string())?
        .into_iter()
        .collect();
    new.retain(|p| !in_base.contains(p));
    let mut candidates: BTreeSet<PathBuf> = start.still_made.iter().cloned().collect();
    candidates.extend(new);
    let candidates: Vec<PathBuf> = candidates.into_iter().collect();
    let mut files = repo
        .untracked_blobs(&candidates)
        .map_err(|e| e.to_string())?;
    files.sort();
    not_recorded += files.len().saturating_sub(MAX_TEST_OUTPUTS);
    files.truncate(MAX_TEST_OUTPUTS);
    Ok(RunEnd {
        files,
        not_recorded,
    })
}

/// On the editor thread, after a run of `command` ended: the agent's
/// records become exactly what the run left. Earlier records the agent
/// changed since are dropped (those files are the agent's now). On an
/// error the records are kept as they were.
pub fn record(
    ed: &mut Editor,
    id: AgentId,
    command: &str,
    agent_was_running: bool,
    end: Result<RunEnd, String>,
) {
    let Some(a) = ed.agents.get_mut(id) else {
        return;
    };
    let end = match end {
        Ok(e) => e,
        Err(e) => {
            a.log(format!(
                "could not tell which files the test run created ({e}); the review counts \
                 every file as the agent's"
            ));
            return;
        }
    };
    let before: BTreeSet<PathBuf> = a.test_outputs.iter().map(|o| o.path.clone()).collect();
    let next: Vec<TestOutput> = end
        .files
        .into_iter()
        .map(|(path, blob)| {
            // A file an earlier run made and this one left alone keeps
            // the command that made it.
            let earlier = a
                .test_outputs
                .iter()
                .find(|o| o.path == path && o.blob == blob);
            match earlier {
                Some(o) => o.clone(),
                None => TestOutput {
                    path,
                    blob,
                    command: command.to_string(),
                    agent_was_running,
                },
            }
        })
        .collect();
    let added: Vec<PathBuf> = next
        .iter()
        .filter(|o| !before.contains(&o.path))
        .map(|o| o.path.clone())
        .collect();
    if !added.is_empty() {
        a.log(format!(
            "the test run created {} file(s) the agent did not write: {}; the review lists them \
             apart and integrate leaves them out",
            added.len(),
            super::actions::list_paths(&added)
        ));
    }
    if end.not_recorded > 0 {
        a.log(format!(
            "the test run created {} more file(s) than Maestro records ({MAX_TEST_OUTPUTS}); \
             those count as the agent's",
            end.not_recorded
        ));
    }
    let changed = next != a.test_outputs;
    a.test_outputs = next;
    if changed {
        super::review::request(ed, id);
    }
}

/// `K` in the review: the file under the cursor, which the verification
/// run created, counts as the agent's from now on.
pub fn keep_as_agents(ed: &mut Editor, id: AgentId, path: &std::path::Path) {
    let Some(a) = ed.agents.get_mut(id) else {
        return;
    };
    let Some(i) = a.test_outputs.iter().position(|o| o.path == path) else {
        ed.error(format!(
            "{} was not created by Maestro's test run; it already counts as the agent's",
            path.display()
        ));
        return;
    };
    a.test_outputs.remove(i);
    a.review.test_made.retain(|p| p != path);
    let msg = format!(
        "{} counts as {}'s work now (K); integrate commits it",
        path.display(),
        a.name
    );
    a.log(msg.clone());
    ed.info(msg);
    super::review::request(ed, id);
    super::conflicts::refresh(ed);
}
