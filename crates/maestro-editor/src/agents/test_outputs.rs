//! Files Maestro's own verification run created or changed in an agent's
//! worktree.
//!
//! `t` runs a command such as `cargo test` in the worktree, and that
//! command can leave new files behind (a `Cargo.lock` in a repository that
//! does not commit one, a coverage report) or rewrite tracked ones (a
//! committed `Cargo.lock`, a snapshot file a test updates). The agent never
//! wrote those changes, so the review lists them apart and integrate and
//! compose leave them out.
//!
//! Created files: just before a run starts, Maestro lists the worktree's
//! untracked files that are not ignored. When the run ends it lists them
//! again. A file in the second list and not in the first, that is a
//! regular file and that the agent's starting commit does not have, was
//! created by the run, and is recorded on the agent with its contents'
//! blob id ([`super::TestOutput`]).
//!
//! Changed files: just before a run starts, Maestro snapshots the
//! worktree's files ([`maestro_git::Repository::work_snapshot_leaving_out`],
//! the tree the run's record keeps), which writes their contents to the
//! object store. When the run ends it snapshots them again. A regular file
//! whose contents differ between the two and that is tracked (in the
//! index) was changed by the run, and is recorded with the blob ids of
//! both and of its index entry: the contents from before the run are the
//! agent's (its own edit, or the commit's version) and are what integrate
//! commits.
//!
//! A record stays the run's only while the file is exactly as the run left
//! it: a created file still untracked with the same contents; a changed
//! file still tracked, with the run's contents on disk and its index
//! entry as the run left it (or holding the version from before the run,
//! which integrate puts there). Once the agent (or anyone) changes, stages or commits it, the
//! whole file as it is then counts as the agent's, the run's change
//! included, and so it does when the user presses `K` on it in the
//! review. Maestro cannot tell a later edit's lines from the run's, and an
//! edit made on top of the run's version keeps the parts of it the editor
//! kept. Files a run created or changed that are still as it left them
//! when the next run starts carry over, with the contents that run leaves
//! and, for a changed file, the agent's version from before the first of
//! those runs. Nothing here changes or deletes a file; `D` in the review
//! does, after asking.
//!
//! What it cannot tell: a file the agent writes while the run is going is
//! taken for the run's (the review says when the agent was running). A run
//! that changes an untracked file the agent wrote, deletes a file, or
//! changes only a file's mode changes the agent's work; a mode the run
//! gives a file it also changed is committed.

use std::collections::BTreeSet;
use std::path::PathBuf;

use maestro_git::{LeftOut, Repository};

use super::{AgentId, TestOutput};
use crate::editor::Editor;

/// At most this many files are recorded per agent; a run that creates
/// more has the rest counted as the agent's, and the history says so.
pub const MAX_TEST_OUTPUTS: usize = 1_000;

/// Which of `outputs` are still as their run left them: a created file
/// untracked, not ignored, a regular file with the recorded contents; a
/// changed file tracked, not in conflict, a regular file with the recorded
/// contents, its index entry untouched ([`index_untouched`]), and its
/// contents from before the run still in the object store.
pub fn still_made_by_tests(
    repo: &Repository,
    outputs: &[TestOutput],
) -> Result<Vec<PathBuf>, String> {
    if outputs.is_empty() {
        return Ok(Vec::new());
    }
    let (changed, created): (Vec<&TestOutput>, Vec<&TestOutput>) =
        outputs.iter().partition(|o| o.before.is_some());
    let paths: Vec<PathBuf> = created.iter().map(|o| o.path.clone()).collect();
    let untracked = repo.untracked_blobs(&paths).map_err(|e| e.to_string())?;
    let (tracked, present) = if changed.is_empty() {
        (Vec::new(), Vec::new())
    } else {
        let paths: Vec<PathBuf> = changed.iter().map(|o| o.path.clone()).collect();
        let befores: Vec<String> = changed.iter().filter_map(|o| o.before.clone()).collect();
        (
            repo.tracked_blobs(&paths).map_err(|e| e.to_string())?,
            repo.objects_present(&befores).map_err(|e| e.to_string())?,
        )
    };
    Ok(outputs
        .iter()
        .filter(|o| match &o.before {
            None => untracked.iter().any(|(p, b)| *p == o.path && *b == o.blob),
            Some(before) => {
                present.contains(before)
                    && tracked.iter().any(|t| {
                        t.path == o.path
                            && t.file.as_ref() == Some(&o.blob)
                            && index_untouched(o, before, &t.index)
                    })
            }
        })
        .map(|o| o.path.clone())
        .collect())
}

/// A changed file's index entry, `now`, is as the run left it, or holds
/// the version from before the run (Maestro's commit of the agent's work
/// puts it there). Anything else means someone staged or committed the
/// file since. A record without the run's index blob (none are written so)
/// only asks that the entry not hold the run's contents.
fn index_untouched(o: &TestOutput, before: &str, now: &str) -> bool {
    match &o.index {
        Some(i) => now == i || now == before,
        None => now != o.blob,
    }
}

/// What committing the agent's work leaves out for the records of
/// `outputs` whose paths are in `made` (from [`still_made_by_tests`]): a
/// created file stays untracked; a changed file is committed with its
/// contents from before the run.
pub fn left_out(outputs: &[TestOutput], made: &[PathBuf]) -> Vec<LeftOut> {
    outputs
        .iter()
        .filter(|o| made.contains(&o.path))
        .map(|o| LeftOut {
            path: o.path.clone(),
            instead: o.before.clone(),
        })
        .collect()
}

/// Which records of `outputs` are still the run's, and what committing
/// the agent's work leaves out for them.
pub fn still_left_out(repo: &Repository, outputs: &[TestOutput]) -> Result<Vec<LeftOut>, String> {
    let made = still_made_by_tests(repo, outputs)?;
    Ok(left_out(outputs, &made))
}

/// What Maestro saw just before a run started.
#[derive(Debug, Clone)]
pub struct RunStart {
    /// Untracked, not ignored files then.
    pub untracked: BTreeSet<PathBuf>,
    /// Earlier runs' files that were still as those runs left them, as the
    /// snapshot of the files the run starts on leaves them out.
    pub still_made: Vec<LeftOut>,
    /// The agent's starting commit: a file it has is never the run's.
    pub base: String,
}

/// Reads the worktree before a run. `outputs` are the agent's records.
pub fn before_run(
    repo: &Repository,
    outputs: &[TestOutput],
    base: &str,
) -> Result<RunStart, String> {
    let still_made = still_left_out(repo, outputs)?;
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

/// A file the run created or changed, as [`TestOutput`] records it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct RunFile {
    pub path: PathBuf,
    pub blob: String,
    pub before: Option<String>,
    pub index: Option<String>,
}

/// What the run left: the files it created and the tracked files it
/// changed, with their contents, and how many more than are recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunEnd {
    /// In path order.
    pub files: Vec<RunFile>,
    pub not_recorded: usize,
}

/// Reads the worktree after a run: files created by it, plus earlier
/// runs' files that were still theirs at the start, as they are now; and
/// tracked files whose contents differ from `tree`, the snapshot of the
/// files the run started on (with earlier runs' changes already replaced
/// by the agent's version, so those carry over).
pub fn after_run(repo: &Repository, start: &RunStart, tree: &str) -> Result<RunEnd, String> {
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
    let mut candidates: BTreeSet<PathBuf> = start
        .still_made
        .iter()
        .filter(|l| l.instead.is_none())
        .map(|l| l.path.clone())
        .collect();
    candidates.extend(new);
    let candidates: Vec<PathBuf> = candidates.into_iter().collect();
    let mut files: Vec<RunFile> = repo
        .untracked_blobs(&candidates)
        .map_err(|e| e.to_string())?
        .into_iter()
        .map(|(path, blob)| RunFile {
            path,
            blob,
            before: None,
            index: None,
        })
        .collect();

    // Tracked files the run changed: the files now against the snapshot
    // taken before it. A run that writes back the index's version still
    // changed the agent's file.
    let after = repo
        .work_snapshot_leaving_out(&[])
        .map_err(|e| e.to_string())?;
    let changed = repo
        .changed_blobs(tree, &after.tree)
        .map_err(|e| e.to_string())?;
    let paths: Vec<PathBuf> = changed.iter().map(|c| c.path.clone()).collect();
    let tracked = repo.tracked_blobs(&paths).map_err(|e| e.to_string())?;
    files.extend(changed.into_iter().filter_map(|c| {
        let t = tracked
            .iter()
            .find(|t| t.path == c.path && t.file.as_ref() == Some(&c.to))?;
        Some(RunFile {
            index: Some(t.index.clone()),
            path: c.path,
            blob: c.to,
            before: Some(c.from),
        })
    }));
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
                "could not tell which files the test run created ({e}); files it created count \
                 as the agent's, and earlier records are kept"
            ));
            return;
        }
    };
    let known: BTreeSet<PathBuf> = a.test_outputs.iter().map(|o| o.path.clone()).collect();
    let next: Vec<TestOutput> = end
        .files
        .into_iter()
        .map(|f| {
            // A file an earlier run made and this one left alone keeps
            // the command that made it.
            let earlier = a.test_outputs.iter().find(|o| {
                o.path == f.path && o.blob == f.blob && o.before == f.before && o.index == f.index
            });
            match earlier {
                Some(o) => o.clone(),
                None => TestOutput {
                    path: f.path,
                    blob: f.blob,
                    before: f.before,
                    index: f.index,
                    command: command.to_string(),
                    agent_was_running,
                },
            }
        })
        .collect();
    let (changed, created): (Vec<&TestOutput>, Vec<&TestOutput>) = next
        .iter()
        .filter(|o| !known.contains(&o.path))
        .partition(|o| o.before.is_some());
    let paths = |v: &[&TestOutput]| -> Vec<PathBuf> { v.iter().map(|o| o.path.clone()).collect() };
    if !created.is_empty() {
        a.log(format!(
            "the test run created {} file(s) the agent did not write: {}; the review lists them \
             apart and integrate leaves them out",
            created.len(),
            super::actions::list_paths(&paths(&created))
        ));
    }
    if !changed.is_empty() {
        a.log(format!(
            "the test run changed {} tracked file(s): {}; the review lists its changes apart and \
             integrate commits those files as they were before the run",
            changed.len(),
            super::actions::list_paths(&paths(&changed))
        ));
    }
    if end.not_recorded > 0 {
        a.log(format!(
            "the test run created or changed {} more file(s) than Maestro records \
             ({MAX_TEST_OUTPUTS}); those count as the agent's",
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
/// run created or changed, counts as the agent's from now on, as it is on
/// disk.
pub fn keep_as_agents(ed: &mut Editor, id: AgentId, path: &std::path::Path) {
    let Some(a) = ed.agents.get_mut(id) else {
        return;
    };
    // A run going now would record the file again when it ends.
    if a.tests_running() {
        ed.error("wait for the test run to finish; it would record the file again when it ends");
        return;
    }
    let Some(i) = a.test_outputs.iter().position(|o| o.path == path) else {
        ed.error(format!(
            "{} was not created or changed by Maestro's test run; it already counts as the agent's",
            path.display()
        ));
        return;
    };
    let o = a.test_outputs.remove(i);
    a.review.release_test_made(path);
    let how = if o.before.is_some() {
        "integrate commits it with the test run's change"
    } else {
        "integrate commits it"
    };
    let msg = format!(
        "{} counts as {}'s work now (K); {how}",
        path.display(),
        a.name
    );
    a.log(msg.clone());
    ed.info(msg);
    super::review::request(ed, id);
    super::conflicts::refresh(ed);
}
