//! Which agents' changes overlap, and whether they would conflict.
//!
//! Two agents of one repository overlap when their last review diffs name
//! the same file. Overlap is found from the review diffs the editor
//! already holds, with no Git. For each overlapping pair a background
//! thread takes a [`work_snapshot`](maestro_git::Repository::work_snapshot)
//! of both worktrees (their committed and uncommitted files as an
//! unreferenced commit) and runs a merge dry run between the two
//! (`git merge-tree`).
//!
//! The cost is bounded three ways: agents whose files overlap no other
//! agent's cost no Git at all; dry runs are cached by the pair of snapshot
//! commit ids, which are the same for the same files, so an unchanged pair
//! never runs `merge-tree` again; and one recomputation runs at a time,
//! started only when some agent's review diff changed (or an agent was
//! decided or archived), with any further request folded into one rerun.
//! Each recomputation takes one snapshot per agent that is in an
//! overlapping pair.

use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;

use maestro_git::MergeCheck;

use super::{Agent, AgentId, Origin, WorktreeInfo};
use crate::editor::Editor;

/// Cached dry runs kept; the cache is cleared when it grows past this.
const MAX_CACHE: usize = 256;

/// What a merge dry run between two agents' work said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Merge {
    /// Not known yet: the dry run is running or queued.
    Checking,
    /// The two would merge without conflicts.
    Clean,
    /// The two would conflict in these files.
    Conflicts(Vec<PathBuf>),
    /// The dry run could not run (a worktree changed branch, git failed).
    Failed(String),
}

/// What a cached dry run is valid for: the repository it ran in (its
/// merge configuration counts, not only the commits) and the two snapshot
/// commits, ordered.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct CacheKey {
    repo: PathBuf,
    ours: String,
    theirs: String,
}

/// Two agents whose changes name the same files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pair {
    pub a: AgentId,
    pub b: AgentId,
    /// The files both changed, relative to the repository root.
    pub files: Vec<PathBuf>,
    pub merge: Merge,
}

impl Pair {
    /// The other agent of the pair, if `id` is in it.
    pub fn other(&self, id: AgentId) -> Option<AgentId> {
        if self.a == id {
            Some(self.b)
        } else if self.b == id {
            Some(self.a)
        } else {
            None
        }
    }
}

#[derive(Debug, Default)]
pub struct ConflictState {
    /// Overlapping pairs from the last recomputation.
    pub pairs: Vec<Pair>,
    /// Dry-run results by repository and ordered pair of snapshot commits.
    cache: HashMap<CacheKey, Merge>,
    running: bool,
    again: bool,
    /// `merge-tree` dry runs and worktree snapshots taken so far, for the
    /// tests that check the cost bound.
    pub dry_runs: u64,
    pub snapshots: u64,
}

/// The agent takes part: it has a record, a worktree that is still its
/// own, a review diff, and no decision yet.
pub(crate) fn eligible(a: &Agent) -> bool {
    a.origin != Origin::Found
        && !a.archived
        && !a.base_status().is_decided()
        && a.worktree.is_some()
        && a.worktree_note.is_none()
        && a.review.computed.is_some()
        && a.review.error.is_none()
}

/// Every path the agent's last review diff names.
pub fn changed_paths(a: &Agent) -> BTreeSet<PathBuf> {
    a.review
        .files
        .iter()
        .flat_map(super::file_paths)
        .cloned()
        .collect()
}

/// Pairs of eligible agents of the same repository whose changed paths
/// overlap, with the shared paths. No Git.
pub fn overlaps(agents: &[Agent]) -> Vec<(AgentId, AgentId, Vec<PathBuf>)> {
    let live: Vec<(&Agent, BTreeSet<PathBuf>)> = agents
        .iter()
        .filter(|a| eligible(a))
        .map(|a| (a, changed_paths(a)))
        .filter(|(_, p)| !p.is_empty())
        .collect();
    let mut out = Vec::new();
    for (i, (a, pa)) in live.iter().enumerate() {
        for (b, pb) in &live[i + 1..] {
            let same_repo = match (&a.worktree, &b.worktree) {
                (Some(x), Some(y)) => x.main_root == y.main_root,
                _ => false,
            };
            if !same_repo {
                continue;
            }
            let shared = shared_paths(pa, pb);
            if !shared.is_empty() {
                out.push((a.id, b.id, shared));
            }
        }
    }
    out
}

/// The paths two change sets share: the same path, or a path of one that
/// is a directory holding a path of the other (one agent adds the file
/// `assets`, the other `assets/icon.png`, which Git cannot merge).
pub fn shared_paths(a: &BTreeSet<PathBuf>, b: &BTreeSet<PathBuf>) -> Vec<PathBuf> {
    let mut out: BTreeSet<PathBuf> = a.intersection(b).cloned().collect();
    for (x, y) in [(a, b), (b, a)] {
        for p in x {
            if y.iter().any(|q| q != p && q.starts_with(p)) {
                out.insert(p.clone());
            }
        }
    }
    out.into_iter().collect()
}

/// [`shared_paths`] is not empty.
pub fn paths_overlap(a: &BTreeSet<PathBuf>, b: &BTreeSet<PathBuf>) -> bool {
    !shared_paths(a, b).is_empty()
}

/// Recomputes the overlapping pairs now, and their dry runs on a
/// background thread. Called when an agent's review diff changed and
/// after a decision or an archive.
pub fn refresh(ed: &mut Editor) {
    if ed.agents.conflicts.running {
        ed.agents.conflicts.again = true;
        return;
    }
    let found = overlaps(&ed.agents.agents);
    let state = &mut ed.agents.conflicts;
    // A result is only shown for the files it was computed on: every pair
    // says "checking" until its new dry run is back.
    state.pairs = found
        .into_iter()
        .map(|(a, b, files)| Pair {
            a,
            b,
            files,
            merge: Merge::Checking,
        })
        .collect();
    if state.pairs.is_empty() {
        return;
    }
    let mut involved: Vec<(AgentId, WorktreeInfo)> = Vec::new();
    for p in &ed.agents.conflicts.pairs {
        for id in [p.a, p.b] {
            if !involved.iter().any(|(x, _)| *x == id)
                && let Some(w) = ed.agents.get(id).and_then(|a| a.worktree.clone())
            {
                involved.push((id, w));
            }
        }
    }
    let pairs: Vec<(AgentId, AgentId)> = ed
        .agents
        .conflicts
        .pairs
        .iter()
        .map(|p| (p.a, p.b))
        .collect();
    let cache = ed.agents.conflicts.cache.clone();
    let git = ed.git.git().clone();
    ed.agents.conflicts.running = true;
    let spawned = ed.spawn_background("agent-conflicts", move |tx| {
        let result = dry_runs(&git, &involved, &pairs, &cache);
        tx.send(move |ed| applied(ed, result));
    });
    if let Err(e) = spawned {
        ed.agents.conflicts.running = false;
        for p in &mut ed.agents.conflicts.pairs {
            p.merge = Merge::Failed(format!("could not start the dry run: {e}"));
        }
    }
}

struct DryRuns {
    results: Vec<((AgentId, AgentId), Merge)>,
    new_cache: Vec<(CacheKey, Merge)>,
    dry_runs: u64,
    snapshots: u64,
}

/// Runs on a background thread: one snapshot per involved agent, then a
/// dry run per pair whose snapshot ids are not cached.
fn dry_runs(
    git: &maestro_git::Git,
    involved: &[(AgentId, WorktreeInfo)],
    pairs: &[(AgentId, AgentId)],
    cache: &HashMap<CacheKey, Merge>,
) -> DryRuns {
    let mut out = DryRuns {
        results: Vec::new(),
        new_cache: Vec::new(),
        dry_runs: 0,
        snapshots: 0,
    };
    let mut snaps: HashMap<AgentId, Result<(String, PathBuf), String>> = HashMap::new();
    for (id, w) in involved {
        out.snapshots += 1;
        let snap = super::actions::open_agent_worktree(git, w).and_then(|repo| {
            repo.work_snapshot()
                .map(|s| (s.commit, w.main_root.clone()))
                .map_err(|e| e.to_string())
        });
        snaps.insert(*id, snap);
    }
    for &(a, b) in pairs {
        let merge = match (snaps.get(&a), snaps.get(&b)) {
            (Some(Ok((ca, root))), Some(Ok((cb, _)))) => {
                let (x, y) = if ca <= cb { (ca, cb) } else { (cb, ca) };
                let key = CacheKey {
                    repo: root.clone(),
                    ours: x.clone(),
                    theirs: y.clone(),
                };
                let known = cache.get(&key).or_else(|| {
                    out.new_cache
                        .iter()
                        .find(|(k, _)| *k == key)
                        .map(|(_, m)| m)
                });
                match known {
                    Some(m) => m.clone(),
                    None => {
                        out.dry_runs += 1;
                        let m = check(git, root, &key.ours, &key.theirs);
                        if !matches!(m, Merge::Failed(_)) {
                            out.new_cache.push((key, m.clone()));
                        }
                        m
                    }
                }
            }
            (Some(Err(e)), _) | (_, Some(Err(e))) => Merge::Failed(e.clone()),
            _ => Merge::Failed("no worktree".into()),
        };
        out.results.push(((a, b), merge));
    }
    out
}

/// A merge dry run between two commits, in the repository at `root`.
pub(crate) fn check(
    git: &maestro_git::Git,
    root: &std::path::Path,
    ours: &str,
    theirs: &str,
) -> Merge {
    let repo = match git.discover(root) {
        Ok(Some(r)) => r,
        Ok(None) => return Merge::Failed(format!("{} is not a repository", root.display())),
        Err(e) => return Merge::Failed(e.to_string()),
    };
    match repo.merge_check(ours, theirs) {
        Ok(MergeCheck::Clean { .. }) => Merge::Clean,
        Ok(MergeCheck::Conflicts { files, .. }) => Merge::Conflicts(files),
        Err(e) => Merge::Failed(e.to_string()),
    }
}

fn applied(ed: &mut Editor, r: DryRuns) {
    let state = &mut ed.agents.conflicts;
    state.running = false;
    state.dry_runs += r.dry_runs;
    state.snapshots += r.snapshots;
    // At most MAX_CACHE entries: when the new ones would not fit, the old
    // ones go, and of a batch larger than the cache only the first are kept.
    if state.cache.len() + r.new_cache.len() > MAX_CACHE {
        state.cache.clear();
    }
    state.cache.extend(r.new_cache.into_iter().take(MAX_CACHE));
    for ((a, b), merge) in r.results {
        if let Some(p) = state.pairs.iter_mut().find(|p| p.a == a && p.b == b) {
            p.merge = merge;
        }
    }
    if std::mem::take(&mut state.again) {
        refresh(ed);
    }
}

/// The pairs agent `id` is in, as (other agent's name, pair).
pub fn for_agent(ed: &Editor, id: AgentId) -> Vec<(String, &Pair)> {
    ed.agents
        .conflicts
        .pairs
        .iter()
        .filter_map(|p| {
            let other = p.other(id)?;
            let name = ed.agents.get(other)?.name.clone();
            Some((name, p))
        })
        .collect()
}

fn list(paths: &[PathBuf]) -> String {
    let mut v: Vec<String> = paths
        .iter()
        .take(3)
        .map(|p| p.display().to_string())
        .collect();
    if paths.len() > 3 {
        v.push(format!("{} more", paths.len() - 3));
    }
    v.join(", ")
}

/// One line per pair for agent `id`, for the panel: who, which files,
/// and what the dry run said.
pub fn describe(ed: &Editor, id: AgentId) -> Vec<String> {
    for_agent(ed, id)
        .into_iter()
        .map(|(name, p)| {
            let verdict = match &p.merge {
                Merge::Checking => "checking whether they conflict".to_string(),
                Merge::Clean => "would merge cleanly".to_string(),
                Merge::Conflicts(f) => format!("CONFLICT in {}", list(f)),
                Merge::Failed(e) => format!("dry run failed: {e}"),
            };
            format!("overlaps {name} in {}: {verdict}", list(&p.files))
        })
        .collect()
}

/// A short form for the agent list: `conflicts beta-2`, `overlaps beta-2`.
pub fn brief(ed: &Editor, id: AgentId) -> Option<String> {
    let pairs = for_agent(ed, id);
    if pairs.is_empty() {
        return None;
    }
    let conflicting: Vec<&str> = pairs
        .iter()
        .filter(|(_, p)| matches!(p.merge, Merge::Conflicts(_)))
        .map(|(n, _)| n.as_str())
        .collect();
    if !conflicting.is_empty() {
        return Some(format!("CONFLICTS {}", conflicting.join(" ")));
    }
    let names: Vec<&str> = pairs.iter().map(|(n, _)| n.as_str()).collect();
    Some(format!("overlaps {}", names.join(" ")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(paths: &[&str]) -> BTreeSet<PathBuf> {
        paths.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn a_file_and_a_directory_of_the_same_name_overlap() {
        assert_eq!(
            shared_paths(&set(&["assets", "x"]), &set(&["assets/icon.png"])),
            [PathBuf::from("assets")]
        );
        assert_eq!(
            shared_paths(&set(&["a/b.txt"]), &set(&["a"])),
            [PathBuf::from("a")]
        );
        assert_eq!(
            shared_paths(&set(&["same.txt"]), &set(&["same.txt", "other"])),
            [PathBuf::from("same.txt")]
        );
        // A shared prefix of names is not a directory.
        assert!(!paths_overlap(&set(&["asset"]), &set(&["assets/icon.png"])));
    }
}
