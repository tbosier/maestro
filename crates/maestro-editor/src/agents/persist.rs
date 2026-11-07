//! Agent records that survive a restart.
//!
//! Records are saved per repository, keyed by the canonical path of its
//! main worktree, in `<state>/agents/<hash of that path>.json`, the state
//! directory editor sessions use, written with
//! [`write_private`](crate::navigation::session::write_private) (0600, a
//! temp file renamed over the target). Nothing is read or written unless
//! sessions are on: `--clean`, headless runs and `restore-session = false`
//! leave agent records alone, as they do editor sessions.
//!
//! The records for the editor's repository are read when the agent panel
//! first opens, on a background thread, because finding the main worktree
//! takes Git. A repository whose file cannot be used (unreadable, invalid,
//! a newer version, written for another repository) is reported, the file
//! is left in place, and nothing is saved for that repository in this
//! session, so the file is never overwritten.
//!
//! What is saved is what the review, tests, decisions and audit need: id,
//! name, group, task, provider name and kind, worktree, status, archived
//! flag, how the process ended, errors, history, comments, test run
//! summaries with the end of their output, and the ACP session id. Never the
//! provider's arguments or environment (the history names the program
//! started), the ACP transcript, or the terminal screen.
//!
//! The same scan lists the repository's worktrees. Worktrees on `maestro/`
//! branches under the agents' worktree directory that no record names are
//! offered as found worktrees ([`Origin::Found`]).
//!
//! Saving is coalesced: [`save`] serializes the records and writes only
//! when the text changed. It runs from [`super::tick`] (about once a
//! second) and when the editor quits. Before writing, it compares the file
//! with what this session last read or wrote ([`take_outside_changes`]).
//!
//! A record forgotten with `d` stays in the file as a [`Forgotten`] entry
//! (the latest [`MAX_FORGOTTEN`]). Every session unions the entries it
//! reads with its own and writes them all, and never loads or writes a
//! record one names, so a session holding a stale copy cannot bring it
//! back. Maestro 0.11.0 and earlier ignore the entries and drop them when
//! they save.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use super::{
    Agent, AgentId, Comment, HistoryEntry, Origin, ProviderKind, ProviderSpec, Status, TestFinish,
    TestRun, TestSummary, WorktreeInfo,
};
use crate::editor::{Editor, Severity, normalize_path};

/// The file format version this build reads and writes.
pub const VERSION: u32 = 1;
/// History entries kept per agent (the latest).
pub const MAX_HISTORY: usize = 500;
/// Test runs kept per agent (the latest).
pub const MAX_TEST_RUNS: usize = 10;
/// Output lines kept per saved test run (the last).
pub const MAX_TEST_TAIL: usize = 100;
/// Errors kept per agent (the latest).
pub const MAX_ERRORS: usize = 100;
/// Ids and groups above this are refused.
const MAX_ID: u64 = 1 << 40;
/// A records file larger than this is refused rather than read.
const MAX_FILE_BYTES: u64 = 64 << 20;
/// Forgotten records remembered per file (the latest). Another session can
/// only write a forgotten record back while its entry is kept, so this is
/// how many later forgets an open session's stale copy survives.
pub const MAX_FORGOTTEN: usize = 1000;

/// `<state>/agents/<fnv1a of the canonical main worktree path>.json`.
pub fn records_path(state_dir: &Path, main_root: &Path) -> PathBuf {
    let root = normalize_path(main_root);
    let hash = crate::recovery::fnv1a(root.to_string_lossy().as_bytes());
    state_dir.join("agents").join(format!("{hash:016x}.json"))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordsFile {
    pub version: u32,
    /// The canonical path of the main worktree, to catch hash collisions.
    pub repository: String,
    pub agents: Vec<AgentRecord>,
    /// Records forgotten with `d`. A record that matches one is never
    /// loaded or written again, by any session of this version.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub forgotten: Vec<Forgotten>,
}

/// A record forgotten with `d`. It names the record by its worktree and
/// creation time, which every session holds the same; ids are not used,
/// because a session renumbers a record whose id it already uses.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Forgotten {
    /// The agent's id in the session that forgot it. New agents never take
    /// an id at or below it.
    pub id: u64,
    /// The worktree's path, normalized.
    pub worktree: String,
    /// The record's `created`, milliseconds since the Unix epoch.
    pub created: u64,
    /// When it was forgotten, milliseconds since the Unix epoch.
    pub at: u64,
}

/// What a [`Forgotten`] entry and a record are matched on: the normalized
/// worktree path and the creation time.
type RecordKey = (PathBuf, u64);

fn forgotten_keys<'a>(list: impl IntoIterator<Item = &'a Forgotten>) -> HashSet<RecordKey> {
    list.into_iter()
        .map(|f| (PathBuf::from(&f.worktree), f.created))
        .collect()
}

fn record_key(r: &AgentRecord) -> RecordKey {
    (normalize_path(Path::new(&r.worktree.path)), r.created)
}

fn agent_key(a: &Agent) -> Option<RecordKey> {
    let w = a.worktree.as_ref()?;
    Some((normalize_path(&w.path), millis(a.created)))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentRecord {
    pub id: u64,
    pub name: String,
    pub group: u64,
    pub task: String,
    /// The provider's name in the config. Its command, arguments and
    /// environment are not saved; relaunching looks the name up again.
    pub provider: String,
    /// `acp` or `terminal`.
    pub kind: String,
    pub worktree: SavedWorktree,
    /// [`Status::key`] of the agent's own status when saved.
    pub status: String,
    #[serde(default)]
    pub archived: bool,
    /// How the process ended, when Maestro saw it end.
    #[serde(default)]
    pub exited: Option<String>,
    #[serde(default)]
    pub errors: Vec<String>,
    #[serde(default)]
    pub history: Vec<SavedEvent>,
    #[serde(default)]
    pub comments: Vec<SavedComment>,
    #[serde(default)]
    pub tests: Vec<SavedTestRun>,
    /// The ACP session, for `session/load` on relaunch.
    #[serde(default)]
    pub session_id: Option<String>,
    /// The ACP agent advertised `loadSession` when it last started.
    #[serde(default)]
    pub load_session: bool,
    /// Milliseconds since the Unix epoch.
    pub created: u64,
    /// The task contract, checked like a typed one when loaded.
    #[serde(default)]
    pub contract: Option<super::contract::Contract>,
    /// `composed` for a result composed from several agents' hunks;
    /// absent for an agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    /// For a composed result: which hunk came from which agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub composition: Option<SavedComposition>,
    /// Parts of the work integrated on their own.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parts: Vec<SavedPart>,
    /// Files Maestro's verification runs created in the worktree.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub test_outputs: Vec<SavedTestOutput>,
}

/// A [`super::TestOutput`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedTestOutput {
    /// Relative to the worktree root.
    pub path: String,
    pub blob: String,
    pub command: String,
    #[serde(default)]
    pub agent_was_running: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedComposition {
    /// The composed commit; empty if it was never made.
    pub commit: String,
    pub sources: Vec<SavedComposedHunk>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedComposedHunk {
    pub agent: String,
    pub label: String,
    pub test: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedPart {
    pub n: usize,
    pub branch: String,
    pub commit: String,
    pub hunks: Vec<SavedPartHunk>,
}

/// A hunk of a part, as [`super::marks::HunkKey`] holds it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedPartHunk {
    pub old_path: Option<String>,
    pub new_path: Option<String>,
    #[serde(default)]
    pub new_mode: Option<String>,
    pub old_start: usize,
    pub old_len: usize,
    pub body: String,
    /// The changed lines taken; absent for the whole hunk.
    pub lines: Option<Vec<usize>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedWorktree {
    pub path: String,
    pub branch: String,
    pub base: String,
    pub main_root: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedEvent {
    /// Milliseconds since the Unix epoch.
    pub at: u64,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedComment {
    pub path: String,
    pub line: usize,
    pub old: bool,
    pub text: String,
    pub sent: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedTestRun {
    pub command: String,
    /// False when the run was still going when the record was saved.
    pub finished: bool,
    pub code: Option<i32>,
    pub duration_ms: u64,
    pub summary: Option<SavedSummary>,
    /// The last output lines, at most [`MAX_TEST_TAIL`].
    pub tail: Vec<String>,
    /// Output lines not kept.
    pub dropped: usize,
    /// The tree of the worktree's files when the run started.
    #[serde(default)]
    pub tree: Option<String>,
    /// The agent's process ran during the run.
    #[serde(default)]
    pub agent_was_running: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedSummary {
    pub passed: u64,
    pub failed: u64,
    pub ignored: u64,
    pub failures: Vec<String>,
}

/// What reading a records file found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadOutcome {
    /// No file: nothing saved yet for this repository.
    Missing,
    Records(RecordsFile),
    /// The file is there but cannot be used; it is left in place.
    Unusable(String),
}

/// Reads and checks the records file at `path` for `main_root`.
pub fn read(path: &Path, main_root: &Path) -> ReadOutcome {
    read_seen(path, main_root).0
}

/// A hash of the file's bytes as this session last read or wrote them;
/// `None` when there was no file.
type Seen = Option<u64>;

fn hash_bytes(bytes: &[u8]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut h);
    h.finish()
}

/// [`read`], with a hash of exactly the bytes that were parsed, so a later
/// save can tell whether someone else wrote the file since.
fn read_seen(path: &Path, main_root: &Path) -> (ReadOutcome, Seen) {
    let shown = path.display();
    let unusable = |m: String| (ReadOutcome::Unusable(m), Some(0));
    match std::fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return (ReadOutcome::Missing, None);
        }
        Err(e) => return unusable(format!("could not read {shown}: {e}")),
        // A FIFO here could block the reader forever.
        Ok(m) if !m.is_file() => return unusable(format!("{shown} is not a regular file")),
        Ok(m) if m.len() > MAX_FILE_BYTES => {
            return unusable(format!(
                "{shown} is larger than {} MiB",
                MAX_FILE_BYTES >> 20
            ));
        }
        Ok(_) => {}
    }
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return (ReadOutcome::Missing, None);
        }
        Err(e) => return unusable(format!("could not read {shown}: {e}")),
    };
    let seen = Some(hash_bytes(&bytes));
    (parse(&bytes, path, main_root), seen)
}

fn parse(bytes: &[u8], path: &Path, main_root: &Path) -> ReadOutcome {
    let shown = path.display();
    let value: serde_json::Value = match serde_json::from_slice(bytes) {
        Ok(v) => v,
        Err(e) => return ReadOutcome::Unusable(format!("{shown} is not valid JSON: {e}")),
    };
    match value.get("version").and_then(serde_json::Value::as_u64) {
        Some(v) if v == u64::from(VERSION) => {}
        Some(v) => {
            return ReadOutcome::Unusable(format!(
                "{shown} has version {v}; this Maestro reads version {VERSION}"
            ));
        }
        None => return ReadOutcome::Unusable(format!("{shown} has no version")),
    }
    let mut file: RecordsFile = match serde_json::from_value(value) {
        Ok(f) => f,
        Err(e) => return ReadOutcome::Unusable(format!("{shown} is not a records file: {e}")),
    };
    let want = normalize_path(main_root);
    if Path::new(&file.repository) != want {
        return ReadOutcome::Unusable(format!(
            "{shown} belongs to {}, not {}",
            file.repository,
            want.display()
        ));
    }
    for f in &file.forgotten {
        // Its id raises the counter new agents take theirs from.
        if f.id == 0 || f.id > MAX_ID || !Path::new(&f.worktree).is_absolute() {
            return ReadOutcome::Unusable(format!(
                "{shown}: forgotten record {} at {:?} has an id out of range or a relative path",
                f.id, f.worktree
            ));
        }
    }
    // A record written back by a Maestro that ignores the forgotten list
    // (0.11.0 and earlier) stays forgotten for every reader of this file.
    let keys = forgotten_keys(&file.forgotten);
    file.agents.retain(|r| !keys.contains(&record_key(r)));
    for r in &file.agents {
        if let Err(e) = check_record(r, &want) {
            return ReadOutcome::Unusable(format!("{shown}: agent {}: {e}", r.id));
        }
    }
    ReadOutcome::Records(file)
}

/// A record Maestro could act on safely: known kind, absolute paths in
/// this repository, a `maestro/` branch. Anything else makes the whole
/// file unusable, so a save never drops a record it could not represent.
fn check_record(r: &AgentRecord, main_root: &Path) -> Result<(), String> {
    kind_from(&r.kind).ok_or_else(|| format!("unknown provider kind {:?}", r.kind))?;
    // Ids and groups count up from 1; a huge one would overflow the
    // counters new agents take theirs from.
    if r.id == 0 || r.id > MAX_ID || r.group > MAX_ID {
        return Err(format!("id {} or group {} is out of range", r.id, r.group));
    }
    let w = &r.worktree;
    if !Path::new(&w.path).is_absolute() {
        return Err(format!("worktree path {:?} is not absolute", w.path));
    }
    if normalize_path(Path::new(&w.main_root)) != main_root {
        return Err(format!(
            "main worktree {:?} is another repository",
            w.main_root
        ));
    }
    if !w.branch.starts_with("maestro/") || w.branch.len() <= "maestro/".len() {
        return Err(format!("branch {:?} is not a maestro/ branch", w.branch));
    }
    // A full commit id: hexadecimal, so it is never taken for an option
    // and can be cut anywhere for display.
    if !is_object_id(&w.base) {
        return Err(format!("base {:?} is not a commit id", w.base));
    }
    if let Some(c) = &r.contract {
        c.validate().map_err(|e| format!("task contract: {e}"))?;
    }
    for t in &r.tests {
        if let Some(tree) = t.tree.as_deref().filter(|t| !is_object_id(t)) {
            return Err(format!("test tree {tree:?} is not an object id"));
        }
    }
    match r.origin.as_deref() {
        None | Some("composed") => {}
        Some(o) => return Err(format!("unknown origin {o:?}")),
    }
    if let Some(c) = &r.composition
        && !c.commit.is_empty()
        && !is_object_id(&c.commit)
    {
        return Err(format!("composed commit {:?} is not a commit id", c.commit));
    }
    // Integrate leaves these paths out of its commit: each must name a file
    // inside the worktree, and the list stays bounded.
    if r.test_outputs.len() > super::test_outputs::MAX_TEST_OUTPUTS {
        return Err(format!(
            "{} files created by test runs; at most {} are kept",
            r.test_outputs.len(),
            super::test_outputs::MAX_TEST_OUTPUTS
        ));
    }
    for o in &r.test_outputs {
        let p = Path::new(&o.path);
        let plain = p
            .components()
            .all(|c| matches!(c, std::path::Component::Normal(_)));
        if o.path.is_empty() || !plain {
            return Err(format!(
                "test run file {:?} is not a path inside the worktree",
                o.path
            ));
        }
        if !is_object_id(&o.blob) {
            return Err(format!(
                "test run file blob {:?} is not an object id",
                o.blob
            ));
        }
    }
    for p in &r.parts {
        if !p.branch.starts_with("maestro/") || !is_object_id(&p.commit) {
            return Err(format!(
                "part {} ({:?} at {:?}) is not a maestro/ branch and a commit id",
                p.n, p.branch, p.commit
            ));
        }
    }
    Ok(())
}

/// Hexadecimal, 4 to 64 characters: never taken for an option.
fn is_object_id(s: &str) -> bool {
    (4..=64).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn kind_from(s: &str) -> Option<ProviderKind> {
    match s {
        "acp" => Some(ProviderKind::Acp),
        "terminal" => Some(ProviderKind::Terminal),
        _ => None,
    }
}

fn millis(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn from_millis(ms: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_millis(ms)
}

/// The record saved for `a`, or `None` when it has nothing on disk to
/// track (no worktree yet, or a found worktree) or a path that is not
/// valid UTF-8.
pub fn to_record(a: &Agent) -> Option<AgentRecord> {
    if a.origin == Origin::Found {
        return None;
    }
    let w = a.worktree.as_ref()?;
    let worktree = SavedWorktree {
        path: w.path.to_str()?.to_string(),
        branch: w.branch.clone(),
        base: w.base.clone(),
        main_root: w.main_root.to_str()?.to_string(),
    };
    let skip = |n: usize, max: usize| n.saturating_sub(max);
    let tests = a.tests[skip(a.tests.len(), MAX_TEST_RUNS)..]
        .iter()
        .map(|t| {
            let keep = t.output.len().min(MAX_TEST_TAIL);
            let f = t.finished.as_ref();
            SavedTestRun {
                command: t.command.clone(),
                finished: f.is_some(),
                code: f.and_then(|f| f.code),
                duration_ms: f.map_or(0, |f| f.duration.as_millis() as u64),
                summary: f.and_then(|f| f.summary.as_ref()).map(|s| SavedSummary {
                    passed: s.passed,
                    failed: s.failed,
                    ignored: s.ignored,
                    failures: s.failures.clone(),
                }),
                tail: t
                    .output
                    .iter()
                    .skip(t.output.len() - keep)
                    .cloned()
                    .collect(),
                dropped: t.dropped + (t.output.len() - keep),
                tree: t.tree.clone(),
                agent_was_running: t.agent_was_running,
            }
        })
        .collect();
    let comments = a
        .comments
        .iter()
        .filter_map(|c| {
            Some(SavedComment {
                path: c.path.to_str()?.to_string(),
                line: c.line,
                old: c.old,
                text: c.text.clone(),
                sent: c.sent,
            })
        })
        .collect();
    let parts = a
        .parts
        .iter()
        .map(|p| SavedPart {
            n: p.n,
            branch: p.branch.clone(),
            commit: p.commit.clone(),
            hunks: p
                .hunks
                .iter()
                .filter_map(|(k, lines)| {
                    let path = |x: &Option<PathBuf>| match x {
                        Some(p) => p.to_str().map(|s| Some(s.to_string())),
                        None => Some(None),
                    };
                    Some(SavedPartHunk {
                        old_path: path(&k.old_path)?,
                        new_path: path(&k.new_path)?,
                        new_mode: k.new_mode.clone(),
                        old_start: k.old_start,
                        old_len: k.old_len,
                        body: k.body.clone(),
                        lines: lines.as_ref().map(|s| s.iter().copied().collect()),
                    })
                })
                .collect(),
        })
        .collect();
    Some(AgentRecord {
        id: a.id.0,
        name: a.name.clone(),
        group: a.group,
        task: a.task.clone(),
        provider: a.provider.name.clone(),
        kind: super::kind_label(a.kind()).to_string(),
        worktree,
        status: a.base_status().key().to_string(),
        archived: a.archived,
        exited: a.exited.clone(),
        errors: a.errors[skip(a.errors.len(), MAX_ERRORS)..].to_vec(),
        history: a.history[skip(a.history.len(), MAX_HISTORY)..]
            .iter()
            .map(|h| SavedEvent {
                at: millis(h.at),
                text: h.text.clone(),
            })
            .collect(),
        comments,
        tests,
        session_id: a.session_id.clone(),
        load_session: a.load_session,
        created: millis(a.created),
        contract: a.contract.clone(),
        origin: (a.origin == Origin::Composed).then(|| "composed".to_string()),
        composition: a.composition.as_ref().map(|c| SavedComposition {
            commit: c.commit.clone(),
            sources: c
                .sources
                .iter()
                .map(|s| SavedComposedHunk {
                    agent: s.agent.clone(),
                    label: s.label.clone(),
                    test: s.test.clone(),
                })
                .collect(),
        }),
        parts,
        test_outputs: a
            .test_outputs
            .iter()
            .filter_map(|o| {
                Some(SavedTestOutput {
                    path: o.path.to_str()?.to_string(),
                    blob: o.blob.clone(),
                    command: o.command.clone(),
                    agent_was_running: o.agent_was_running,
                })
            })
            .collect(),
    })
}

/// An agent rebuilt from its record. It has no process: a status that
/// needs one becomes [`Status::Stopped`], and the history says what it was.
/// The provider is looked up by name and kind in `providers`; when it is no
/// longer configured, the spec has no program and relaunching refuses.
pub fn from_record(r: &AgentRecord, providers: &[ProviderSpec]) -> Agent {
    let kind = kind_from(&r.kind).unwrap_or(ProviderKind::Terminal);
    let provider = providers
        .iter()
        .find(|p| p.name == r.provider && p.kind == kind)
        .cloned()
        .unwrap_or_else(|| ProviderSpec {
            name: r.provider.clone(),
            kind,
            program: String::new(),
            args: Vec::new(),
            env: Vec::new(),
        });
    let mut a = Agent::new(AgentId(r.id), r.task.clone(), r.group, provider);
    a.name = r.name.clone();
    a.origin = Origin::Loaded;
    a.created = from_millis(r.created);
    a.worktree = Some(WorktreeInfo {
        path: PathBuf::from(&r.worktree.path),
        branch: r.worktree.branch.clone(),
        base: r.worktree.base.clone(),
        main_root: PathBuf::from(&r.worktree.main_root),
    });
    a.archived = r.archived;
    a.exited = r.exited.clone();
    a.errors = r.errors.clone();
    a.session_id = r.session_id.clone();
    a.load_session = r.load_session;
    a.contract = r.contract.clone();
    if r.origin.as_deref() == Some("composed") {
        a.origin = Origin::Composed;
    }
    a.composition = r.composition.as_ref().map(|c| super::compose::Composition {
        commit: c.commit.clone(),
        sources: c
            .sources
            .iter()
            .map(|s| super::compose::ComposedHunk {
                agent: s.agent.clone(),
                label: s.label.clone(),
                test: s.test.clone(),
            })
            .collect(),
    });
    a.test_outputs = r
        .test_outputs
        .iter()
        .map(|o| super::TestOutput {
            path: PathBuf::from(&o.path),
            blob: o.blob.clone(),
            command: o.command.clone(),
            agent_was_running: o.agent_was_running,
        })
        .collect();
    a.parts = r
        .parts
        .iter()
        .map(|p| super::marks::Part {
            n: p.n,
            branch: p.branch.clone(),
            commit: p.commit.clone(),
            hunks: p
                .hunks
                .iter()
                .map(|h| {
                    (
                        super::marks::HunkKey {
                            old_path: h.old_path.as_ref().map(PathBuf::from),
                            new_path: h.new_path.as_ref().map(PathBuf::from),
                            new_mode: h.new_mode.clone(),
                            old_start: h.old_start,
                            old_len: h.old_len,
                            body: h.body.clone(),
                        },
                        h.lines.as_ref().map(|l| l.iter().copied().collect()),
                    )
                })
                .collect(),
        })
        .collect();
    a.history = r
        .history
        .iter()
        .map(|h| HistoryEntry {
            at: from_millis(h.at),
            text: h.text.clone(),
        })
        .collect();
    a.comments = r
        .comments
        .iter()
        .map(|c| Comment {
            path: PathBuf::from(&c.path),
            line: c.line,
            old: c.old,
            text: c.text.clone(),
            sent: c.sent,
        })
        .collect();
    let mut unfinished = false;
    a.tests = r
        .tests
        .iter()
        .map(|t| {
            unfinished |= !t.finished;
            TestRun {
                command: t.command.clone(),
                started: Instant::now(),
                output: t.tail.iter().cloned().collect(),
                dropped: t.dropped,
                // A run that had not finished has no files to vouch for.
                tree: t.tree.clone().filter(|_| t.finished),
                agent_was_running: t.agent_was_running,
                // A run that was going when the record was saved did not
                // finish as far as Maestro knows: it shows as killed.
                finished: Some(TestFinish {
                    code: if t.finished { t.code } else { None },
                    duration: Duration::from_millis(t.duration_ms),
                    summary: t.summary.as_ref().map(|s| TestSummary {
                        passed: s.passed,
                        failed: s.failed,
                        ignored: s.ignored,
                        failures: s.failures.clone(),
                    }),
                }),
            }
        })
        .collect();
    let saved = Status::from_key(&r.status);
    let status = match saved {
        Some(s) if !s.needs_process() => s,
        _ => Status::Stopped,
    };
    a.decide(status);
    let was = match saved {
        Some(s) if s.needs_process() => {
            Some(format!("it was {} when Maestro last saved it", s.label()))
        }
        Some(_) => None,
        None => Some(format!("its saved status {:?} is unknown", r.status)),
    };
    let mut note = "loaded from an earlier session; this Maestro has no process for it".to_string();
    if let Some(w) = was {
        note.push_str(&format!("; {w}"));
    }
    if unfinished {
        note.push_str("; a test run had not finished");
    }
    a.log(note);
    a
}

/// Where a repository's records stand in this session.
#[derive(Debug)]
enum RepoState {
    /// Read (or found missing); saves go to this repository's file.
    /// `written` is the last text written and whether it was synced;
    /// `seen` the file as this session last read or wrote it.
    Ready {
        written: Option<(String, bool)>,
        seen: Seen,
    },
    /// The file could not be used; it is left alone and never written.
    Blocked(String),
}

/// Persistence bookkeeping, kept on [`super::AgentState`].
#[derive(Debug, Default)]
pub struct PersistState {
    repos: HashMap<PathBuf, RepoState>,
    /// Working directories whose repository was looked up already.
    looked_up: HashSet<PathBuf>,
    /// A background scan for the editor's repository runs. Starting an
    /// agent waits for it, so new ids never collide with loaded ones.
    pub loading: bool,
    /// The last save error reported, so a failing save does not repeat
    /// its message every second.
    last_error: Option<String>,
    /// [`fingerprint`] at the last save, so an unchanged state is not
    /// even serialized.
    last_fingerprint: Option<u64>,
    /// Records forgotten (`d`), per repository: in this session, or read
    /// from the file another session wrote. Matching records are never
    /// loaded or saved, the sidebar does not list them, and every save
    /// writes the list, so no session of this version brings them back.
    forgotten: HashMap<PathBuf, Vec<Forgotten>>,
    /// Repositories whose file must be written even when no agent of
    /// theirs is left, because a record was forgotten. An entry stays
    /// until a write synced to disk (on quit) holds the change.
    rewrite: HashSet<PathBuf>,
}

/// A cheap summary of everything a record holds that changes on a
/// meaningful event: creation, status, decisions, a test run starting or
/// finishing, comments, archive, the session id. Output streaming into a
/// running test does not change it; the tail is saved when the run ends.
fn fingerprint(ed: &Editor) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    ed.agents
        .persist
        .repos
        .values()
        .filter(|r| matches!(r, RepoState::Ready { .. }))
        .count()
        .hash(&mut h);
    ed.agents
        .persist
        .forgotten
        .values()
        .map(Vec::len)
        .sum::<usize>()
        .hash(&mut h);
    for a in &ed.agents.agents {
        (a.id.0, &a.name, &a.task, a.group, &a.provider.name).hash(&mut h);
        (a.origin as u8, a.base_status().key(), a.archived, &a.exited).hash(&mut h);
        (
            a.errors.len(),
            a.history.len(),
            &a.session_id,
            a.load_session,
        )
            .hash(&mut h);
        if let Some(w) = &a.worktree {
            (&w.path, &w.branch, &w.base).hash(&mut h);
        }
        for c in &a.comments {
            (&c.path, c.line, c.old, &c.text, c.sent).hash(&mut h);
        }
        a.tests.len().hash(&mut h);
        a.tests.last().map(|t| t.finished.is_some()).hash(&mut h);
        a.tests.last().map(|t| &t.tree).hash(&mut h);
        a.contract.as_ref().map(|c| c.brief()).hash(&mut h);
        a.composition.as_ref().map(|c| &c.commit).hash(&mut h);
        a.parts.len().hash(&mut h);
        for o in &a.test_outputs {
            (&o.path, &o.blob).hash(&mut h);
        }
    }
    h.finish()
}

impl PersistState {
    /// Why the records of `main_root` are not saved, if they are not.
    pub fn blocked(&self, main_root: &Path) -> Option<&str> {
        match self.repos.get(&normalize_path(main_root)) {
            Some(RepoState::Blocked(why)) => Some(why),
            _ => None,
        }
    }

    /// A record of the agent in `worktree` was forgotten. For the
    /// sidebar, which lists records by worktree: the creation time is not
    /// compared.
    pub fn is_forgotten(&self, worktree: &Path) -> bool {
        if self.forgotten.is_empty() {
            return false;
        }
        let path = normalize_path(worktree);
        self.forgotten
            .values()
            .flatten()
            .any(|f| Path::new(&f.worktree) == path)
    }

    /// The keys of every record forgotten, in any repository.
    fn forgotten_keys(&self) -> HashSet<RecordKey> {
        forgotten_keys(self.forgotten.values().flatten())
    }
}

/// `ours` with the entries of `theirs` it lacks, one entry per record (the
/// earliest forget), and at most [`MAX_FORGOTTEN`] (the latest). Sorted,
/// so two sessions that know the same forgets write the same text.
fn union_forgotten(ours: &[Forgotten], theirs: &[Forgotten]) -> Vec<Forgotten> {
    let mut by_key: HashMap<RecordKey, Forgotten> = HashMap::new();
    for f in ours.iter().chain(theirs) {
        let key = (PathBuf::from(&f.worktree), f.created);
        match by_key.get(&key) {
            Some(had) if (had.at, had.id) <= (f.at, f.id) => {}
            _ => {
                by_key.insert(key, f.clone());
            }
        }
    }
    let mut all: Vec<Forgotten> = by_key.into_values().collect();
    all.sort_by(|a, b| {
        (a.at, &a.worktree, a.created, a.id).cmp(&(b.at, &b.worktree, b.created, b.id))
    });
    let extra = all.len().saturating_sub(MAX_FORGOTTEN);
    all.drain(..extra);
    all
}

/// Adds the forgotten entries read from `root`'s file to this session's.
/// New agents take ids above theirs. An agent of this session that one
/// names leaves the panel when it is archived and idle, as if forgotten
/// here; otherwise it stays until the editor quits but is no longer saved.
fn take_forgotten(ed: &mut Editor, root: &Path, theirs: &[Forgotten]) {
    if theirs.is_empty() {
        return;
    }
    let had = ed.agents.persist.forgotten_keys();
    let ours = ed
        .agents
        .persist
        .forgotten
        .get(root)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let all = union_forgotten(ours, theirs);
    let state = &mut ed.agents;
    state.next_id = state
        .next_id
        .max(theirs.iter().map(|f| f.id).max().unwrap_or(0));
    let new: HashSet<RecordKey> = forgotten_keys(&all)
        .into_iter()
        .filter(|k| !had.contains(k))
        .collect();
    state.persist.forgotten.insert(root.to_path_buf(), all);
    if new.is_empty() {
        return;
    }
    let mut i = 0;
    while i < ed.agents.agents.len() {
        let a = &ed.agents.agents[i];
        if a.origin == Origin::Found || !agent_key(a).is_some_and(|k| new.contains(&k)) {
            i += 1;
            continue;
        }
        if super::actions::forget_blocker(a).is_none() {
            let a = super::actions::remove_from_panel(ed, i);
            ed.add_status(
                Severity::Info,
                format!("{}'s record was forgotten in another Maestro", a.name),
            );
            continue;
        }
        let a = &mut ed.agents.agents[i];
        a.log(
            "its record was forgotten in another Maestro: it stays in this panel until the \
             editor quits, and is no longer saved",
        );
        let name = a.name.clone();
        ed.add_status(
            Severity::Warning,
            format!(
                "{name}'s record was forgotten in another Maestro; it is no longer saved and \
                 leaves the panel when the editor quits"
            ),
        );
        i += 1;
    }
}

/// Notes that the record of `a` was forgotten: the next save writes its
/// repository's file without it, even when no agent of that repository is
/// left, and with an entry that keeps every session of this version from
/// loading or saving that record again.
pub(crate) fn forget(ed: &mut Editor, a: &Agent) {
    let Some(w) = &a.worktree else {
        return;
    };
    let root = normalize_path(&w.main_root);
    let p = &mut ed.agents.persist;
    // A path that is not UTF-8 was never saved, so the file has nothing
    // to forget.
    if let Some(path) = normalize_path(&w.path).to_str() {
        let entry = Forgotten {
            id: a.id.0,
            worktree: path.to_string(),
            created: millis(a.created),
            at: millis(SystemTime::now()),
        };
        let ours = p
            .forgotten
            .get(&root)
            .map(Vec::as_slice)
            .unwrap_or_default();
        let all = union_forgotten(ours, &[entry]);
        p.forgotten.insert(root.clone(), all);
    }
    p.rewrite.insert(root);
    p.last_fingerprint = None;
}

/// The state directory, when agent records may be read and written: the
/// same condition as editor sessions (a state directory and sessions on).
pub fn enabled(ed: &Editor) -> Option<PathBuf> {
    let nav = &ed.navigation;
    if nav.save_session {
        nav.state_dir.clone()
    } else {
        None
    }
}

/// What the background scan found for a repository.
struct Scan {
    main_root: PathBuf,
    file: ReadOutcome,
    seen: Seen,
    worktrees: Vec<maestro_git::Worktree>,
    /// The main worktree's `HEAD` commit and branch, the base found
    /// worktrees are compared against.
    main_head: Option<(String, String)>,
    /// The agents' worktree directory, canonical when it exists.
    agents_dir: PathBuf,
}

/// Reads the records of the editor's repository and looks for found
/// worktrees, once per working directory, on a background thread. Called
/// when the panel opens.
pub fn load_for_cwd(ed: &mut Editor) {
    let Some(state) = enabled(ed) else {
        return;
    };
    let cwd = normalize_path(&ed.cwd);
    if ed.agents.persist.loading || ed.agents.persist.looked_up.contains(&cwd) {
        return;
    }
    ed.agents.persist.looked_up.insert(cwd.clone());
    ed.agents.persist.loading = true;
    let git = ed.git.git().clone();
    let setting = ed.config.agents.worktree_dir.clone();
    let spawned = ed.spawn_background("agent-records", move |tx| {
        let scan = scan(&git, &cwd, &state, setting.as_deref());
        tx.send(move |ed| {
            ed.agents.persist.loading = false;
            match scan {
                Ok(Some(s)) => apply(ed, s),
                Ok(None) => {}
                Err(e) => ed.add_status(
                    Severity::Warning,
                    format!("agent records were not loaded: {e}"),
                ),
            }
        });
    });
    if let Err(e) = spawned {
        ed.agents.persist.loading = false;
        ed.add_status(
            Severity::Warning,
            format!("agent records were not loaded: {e}"),
        );
    }
}

/// Runs on a background thread. `Ok(None)` outside a repository.
fn scan(
    git: &maestro_git::Git,
    cwd: &Path,
    state: &Path,
    setting: Option<&str>,
) -> Result<Option<Scan>, String> {
    let Some(repo) = git.discover(cwd).map_err(|e| e.to_string())? else {
        return Ok(None);
    };
    let worktrees = repo.worktrees().map_err(|e| e.to_string())?;
    let main_root = worktrees
        .iter()
        .find(|w| w.is_main)
        .map(|w| w.path.clone())
        .unwrap_or_else(|| repo.root().to_path_buf());
    let main_root = normalize_path(&main_root);
    let main_head = git.discover(&main_root).ok().flatten().and_then(|main| {
        let commit = main.resolve("HEAD").ok()?;
        Some((commit, main.head().ok()?.label()))
    });
    let (file, seen) = read_seen(&records_path(state, &main_root), &main_root);
    let agents_dir = normalize_path(&super::create::worktree_root(&main_root, setting));
    Ok(Some(Scan {
        main_root,
        file,
        seen,
        worktrees,
        main_head,
        agents_dir,
    }))
}

/// Adds the scanned records and found worktrees to the editor's agents.
fn apply(ed: &mut Editor, mut scan: Scan) {
    let root = scan.main_root.clone();
    if ed.agents.persist.repos.contains_key(&root) {
        return;
    }
    let loaded = take_file(
        ed,
        &root,
        std::mem::replace(&mut scan.file, ReadOutcome::Missing),
        scan.seen,
    );
    let checked = verify(ed, &root, &scan.worktrees);
    let found = add_found(ed, &scan);
    let mut parts = Vec::new();
    if loaded > 0 {
        parts.push(format!(
            "{loaded} agent record(s) from an earlier session, not running"
        ));
    }
    if found > 0 {
        parts.push(format!(
            "{found} found worktree(s) with no record (a adopts, A archives)"
        ));
    }
    if checked > 0 {
        parts.push(format!(
            "{checked} record(s) whose worktree changed; see their history"
        ));
    }
    if !parts.is_empty() {
        ed.add_status(Severity::Info, format!("loaded {}", parts.join("; ")));
    }
    let ids: Vec<AgentId> = ed
        .agents
        .agents
        .iter()
        .filter(|a| a.origin != Origin::New && a.worktree.is_some() && !a.archived)
        .map(|a| a.id)
        .collect();
    for id in ids {
        super::review::request(ed, id);
    }
}

/// Records the file's outcome for `root` and merges its records. Returns
/// how many were added.
fn take_file(ed: &mut Editor, root: &Path, file: ReadOutcome, seen: Seen) -> usize {
    match file {
        ReadOutcome::Unusable(why) => {
            ed.add_status(
                Severity::Warning,
                format!(
                    "agent records not loaded: {why}. The file is left as it is, and agents \
                     in this repository are not saved in this session"
                ),
            );
            ed.agents
                .persist
                .repos
                .insert(root.to_path_buf(), RepoState::Blocked(why));
            0
        }
        ReadOutcome::Missing => {
            ed.agents.persist.repos.insert(
                root.to_path_buf(),
                RepoState::Ready {
                    written: None,
                    seen,
                },
            );
            0
        }
        ReadOutcome::Records(f) => {
            take_forgotten(ed, root, &f.forgotten);
            let n = merge(ed, &f.agents);
            ed.agents.persist.repos.insert(
                root.to_path_buf(),
                RepoState::Ready {
                    written: None,
                    seen,
                },
            );
            n
        }
    }
}

/// Adds records as loaded agents. Ids continue after the highest one, so
/// new agents get new ids and branch names. A record whose id an agent of
/// this session already has gets a new id, and its history says so.
fn merge(ed: &mut Editor, records: &[AgentRecord]) -> usize {
    let providers = super::providers(ed);
    let forgotten = ed.agents.persist.forgotten_keys();
    let mut records: Vec<&AgentRecord> = records
        .iter()
        .filter(|r| !forgotten.contains(&record_key(r)))
        .collect();
    records.sort_by_key(|r| r.id);
    let taken: HashSet<u64> = ed.agents.agents.iter().map(|a| a.id.0).collect();
    let state = &mut ed.agents;
    state.next_id = state
        .next_id
        .max(records.iter().map(|r| r.id).max().unwrap_or(0));
    state.next_group = state
        .next_group
        .max(records.iter().map(|r| r.group).max().unwrap_or(0));
    let taken_groups: HashSet<u64> = state.agents.iter().map(|a| a.group).collect();
    let mut groups: HashMap<u64, u64> = HashMap::new();
    let mut seen = HashSet::new();
    for r in &records {
        let mut a = from_record(r, &providers);
        if taken.contains(&r.id) || !seen.insert(r.id) {
            let new = state.fresh_id();
            let name = format!("{}-{}", r.provider, new.0);
            a.log(format!(
                "renumbered from {} ({}) to {} ({name}): another agent already had its id",
                r.id, r.name, new.0
            ));
            a.id = new;
            a.name = name;
        }
        // A group already used by agents of this session belongs to
        // another task; these get a group of their own.
        if taken_groups.contains(&r.group) {
            a.group = *groups.entry(r.group).or_insert_with(|| state.fresh_group());
        }
        state.agents.push(a);
    }
    records.len()
}

/// Checks each loaded record's worktree against Git's list and notes what
/// changed. Returns how many changed.
fn verify(ed: &mut Editor, root: &Path, list: &[maestro_git::Worktree]) -> usize {
    let mut changed = 0;
    for a in &mut ed.agents.agents {
        if a.origin != Origin::Loaded || a.archived || a.worktree_note.is_some() {
            continue;
        }
        let Some(w) = &a.worktree else {
            continue;
        };
        if normalize_path(&w.main_root) != root {
            continue;
        }
        let note = match list
            .iter()
            .find(|x| normalize_path(&x.path) == normalize_path(&w.path))
        {
            None => Some(format!(
                "Git no longer lists {} as a worktree",
                w.path.display()
            )),
            Some(x) if x.prunable.is_some() => Some(format!(
                "the worktree directory {} is gone",
                w.path.display()
            )),
            Some(x) if x.branch.as_deref() != Some(w.branch.as_str()) => Some(format!(
                "the worktree {} is now on {}, not {}",
                w.path.display(),
                x.branch
                    .as_deref()
                    .map_or("a detached HEAD".to_string(), |b| format!("branch {b}")),
                w.branch
            )),
            Some(_) => None,
        };
        if let Some(n) = note {
            a.log(format!("{n}; relaunch, tests and archive refuse it"));
            a.worktree_note = Some(n);
            changed += 1;
        }
    }
    changed
}

/// Adds a found entry for each worktree on a `maestro/` branch under the
/// agents' directory that no agent names. Returns how many.
fn add_found(ed: &mut Editor, scan: &Scan) -> usize {
    let Some((head, main_branch)) = &scan.main_head else {
        return 0;
    };
    let tracked: Vec<PathBuf> = ed
        .agents
        .agents
        .iter()
        .filter(|a| !a.archived)
        .filter_map(|a| a.worktree.as_ref().map(|w| normalize_path(&w.path)))
        .collect();
    let mut n = 0;
    for x in &scan.worktrees {
        let Some(branch) = &x.branch else {
            continue;
        };
        let path = normalize_path(&x.path);
        if x.is_main
            || x.bare
            || x.prunable.is_some()
            || !branch.starts_with("maestro/")
            || !path.starts_with(&scan.agents_dir)
            || tracked.contains(&path)
        {
            continue;
        }
        let id = ed.agents.fresh_id();
        let group = ed.agents.fresh_group();
        let provider = ProviderSpec {
            name: "unknown".into(),
            kind: ProviderKind::Terminal,
            program: String::new(),
            args: Vec::new(),
            env: Vec::new(),
        };
        let mut a = Agent::new(id, String::new(), group, provider);
        let dir = path
            .file_name()
            .map(|d| d.to_string_lossy().into_owned())
            .unwrap_or_else(|| branch.clone());
        a.name = format!("found-{dir}");
        a.origin = Origin::Found;
        a.decide(Status::Stopped);
        a.worktree = Some(WorktreeInfo {
            path,
            branch: branch.clone(),
            base: head.clone(),
            main_root: scan.main_root.clone(),
        });
        a.log(format!(
            "found a worktree on branch {branch} that no agent record names; its task and \
             provider are unknown. The review shows what it changed since it forked from \
             {main_branch} ({})",
            super::create::short(head)
        ));
        ed.agents.agents.push(a);
        n += 1;
    }
    n
}

/// Reads the records of `main_root` on the editor thread if this session
/// has not yet. Used when an agent is created in a repository whose
/// records were never read (after `:cd` into another repository), so the
/// next save cannot replace them. Found worktrees are not looked for here.
pub fn ensure_loaded(ed: &mut Editor, main_root: &Path) {
    let Some(state) = enabled(ed) else {
        return;
    };
    let root = normalize_path(main_root);
    if ed.agents.persist.repos.contains_key(&root) {
        return;
    }
    let (file, seen) = read_seen(&records_path(&state, &root), &root);
    let n = take_file(ed, &root, file, seen);
    if n > 0 {
        ed.add_status(
            Severity::Info,
            format!("loaded {n} agent record(s) from an earlier session, not running"),
        );
    }
}

/// Before a save: for each repository saved to, looks at its file. If it
/// is not what this session last read or wrote (another Maestro saved, or
/// someone edited it), its records for worktrees this session does not
/// know are added as loaded agents, so the save keeps them; a file that can
/// no longer be used blocks saving instead of being replaced. Records both
/// sessions have are saved as this session has them.
///
/// Another program can still write between this check and the rename.
fn take_outside_changes(ed: &mut Editor, state: &Path, errors: &mut Vec<String>) {
    let roots: Vec<(PathBuf, Seen)> = ed
        .agents
        .persist
        .repos
        .iter()
        .filter_map(|(root, r)| match r {
            RepoState::Ready { seen, .. } => Some((root.clone(), *seen)),
            RepoState::Blocked(_) => None,
        })
        .collect();
    for (root, seen) in roots {
        let path = records_path(state, &root);
        let (now, now_seen) = read_seen(&path, &root);
        if now_seen == seen {
            continue;
        }
        match now {
            // Removed by someone: this session's records are written anew.
            ReadOutcome::Missing => {}
            ReadOutcome::Unusable(why) => {
                errors.push(format!(
                    "the agent records file changed and can no longer be used ({why}); it is \
                     left as it is, and agents in this repository are no longer saved in this \
                     session"
                ));
                ed.agents
                    .persist
                    .repos
                    .insert(root.clone(), RepoState::Blocked(why));
                continue;
            }
            ReadOutcome::Records(f) => {
                take_forgotten(ed, &root, &f.forgotten);
                let known: HashSet<PathBuf> = ed
                    .agents
                    .agents
                    .iter()
                    .filter(|a| a.origin != Origin::Found)
                    .filter_map(|a| a.worktree.as_ref().map(|w| normalize_path(&w.path)))
                    .collect();
                // A record forgotten here or by the writer stays forgotten
                // (`merge` leaves it out).
                let new: Vec<AgentRecord> = f
                    .agents
                    .into_iter()
                    .filter(|r| !known.contains(&normalize_path(Path::new(&r.worktree.path))))
                    .collect();
                let n = merge(ed, &new);
                if n > 0 {
                    ed.add_status(
                        Severity::Info,
                        format!(
                            "{n} agent record(s) saved by another Maestro were added, not running"
                        ),
                    );
                }
            }
        }
        if let Some(RepoState::Ready { seen, .. }) = ed.agents.persist.repos.get_mut(&root) {
            *seen = now_seen;
        }
    }
}

/// Writes the records of every repository whose file was read, when they
/// changed since the last write. `durable` syncs the file (on quit).
pub fn save(ed: &mut Editor, durable: bool) {
    let Some(state) = enabled(ed) else {
        return;
    };
    let p = &ed.agents.persist;
    if ed.agents.agents.is_empty() && p.rewrite.is_empty() && p.forgotten.is_empty() {
        return;
    }
    let print = fingerprint(ed);
    if !durable && ed.agents.persist.last_fingerprint == Some(print) {
        return;
    }
    let mut errors = Vec::new();
    take_outside_changes(ed, &state, &mut errors);
    let mut groups: BTreeMap<PathBuf, Vec<AgentRecord>> = BTreeMap::new();
    let mut unsaved = Vec::new();
    let forgotten = ed.agents.persist.forgotten_keys();
    for a in &ed.agents.agents {
        let Some(w) = &a.worktree else {
            continue;
        };
        // Forgotten in another Maestro while it is still listed here.
        if agent_key(a).is_some_and(|k| forgotten.contains(&k)) {
            continue;
        }
        match to_record(a) {
            Some(r) => {
                if r.comments.len() < a.comments.len() {
                    unsaved.push(format!("comments of {} on files", a.name));
                }
                let kept: usize = r.parts.iter().map(|p| p.hunks.len()).sum();
                let all: usize = a.parts.iter().map(|p| p.hunks.len()).sum();
                if kept < all {
                    unsaved.push(format!("integrated hunks of {} in files", a.name));
                }
                groups
                    .entry(normalize_path(&w.main_root))
                    .or_default()
                    .push(r);
            }
            None if a.origin != Origin::Found => unsaved.push(a.name.clone()),
            None => {}
        }
    }
    if !unsaved.is_empty() {
        errors.push(format!(
            "not saved, because a path is not valid UTF-8: {}",
            unsaved.join(", ")
        ));
    }
    // A repository whose last agent was forgotten is written with none,
    // and one with forgotten records keeps the list in its file even when
    // no agent of this session is in it: a Maestro that ignores the list
    // may have dropped it.
    let p = &ed.agents.persist;
    for root in p.rewrite.iter().chain(p.forgotten.keys()) {
        groups.entry(root.clone()).or_default();
    }
    for (root, mut records) in groups {
        let Some(RepoState::Ready { written, seen }) = ed.agents.persist.repos.get_mut(&root)
        else {
            // Blocked, or never read: never written.
            ed.agents.persist.rewrite.remove(&root);
            continue;
        };
        let Some(repository) = root.to_str() else {
            continue;
        };
        records.sort_by_key(|r| r.id);
        let file = RecordsFile {
            version: VERSION,
            repository: repository.to_string(),
            agents: records,
            forgotten: ed
                .agents
                .persist
                .forgotten
                .get(&root)
                .cloned()
                .unwrap_or_default(),
        };
        let text = match serde_json::to_string_pretty(&file) {
            Ok(t) => t,
            Err(e) => {
                errors.push(format!("could not encode agent records: {e}"));
                continue;
            }
        };
        if let Some((last, synced)) = written
            && *last == text
            && (*synced || !durable)
        {
            if *synced {
                ed.agents.persist.rewrite.remove(&root);
            }
            continue;
        }
        let path = records_path(&state, &root);
        match crate::navigation::session::write_private(&path, text.as_bytes(), durable) {
            Ok(()) => {
                *seen = Some(hash_bytes(text.as_bytes()));
                *written = Some((text, durable));
                if durable {
                    ed.agents.persist.rewrite.remove(&root);
                }
            }
            Err(e) => errors.push(format!(
                "could not save agent records to {}: {e}",
                path.display()
            )),
        }
    }
    let error = (!errors.is_empty()).then(|| errors.join("; "));
    if error.is_some() && error != ed.agents.persist.last_error {
        ed.add_status(Severity::Warning, error.clone().unwrap_or_default());
    }
    // After a failure the next tick tries again.
    ed.agents.persist.last_fingerprint = error.is_none().then_some(print);
    ed.agents.persist.last_error = error;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(id: u64) -> AgentRecord {
        AgentRecord {
            id,
            name: format!("sh-{id}"),
            group: 1,
            task: "do it".into(),
            provider: "sh".into(),
            kind: "terminal".into(),
            worktree: SavedWorktree {
                path: format!("/r-worktrees/do-it-{id}"),
                branch: format!("maestro/do-it-{id}"),
                base: "abc123".into(),
                main_root: "/r".into(),
            },
            status: "ready-for-review".into(),
            archived: false,
            exited: Some("exited with code 0".into()),
            errors: vec![],
            history: vec![SavedEvent {
                at: 1_000,
                text: "created".into(),
            }],
            comments: vec![],
            tests: vec![],
            session_id: None,
            load_session: false,
            created: 1_000,
            contract: None,
            origin: None,
            composition: None,
            parts: vec![],
            test_outputs: vec![],
        }
    }

    fn write(dir: &Path, name: &str, text: &str) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let p = dir.join(name);
        std::fs::write(&p, text).unwrap();
        p
    }

    #[test]
    fn records_path_is_per_repository() {
        let a = records_path(Path::new("/s"), Path::new("/nonexistent/a"));
        let b = records_path(Path::new("/s"), Path::new("/nonexistent/b"));
        assert_ne!(a, b);
        assert!(a.starts_with("/s/agents"));
        assert_eq!(a.extension().unwrap(), "json");
    }

    #[test]
    fn a_file_round_trips_and_is_checked() {
        let dir = std::env::temp_dir().join(format!("maestro-records-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let file = RecordsFile {
            version: VERSION,
            repository: "/r".into(),
            agents: vec![record(1), record(2)],
            forgotten: vec![],
        };
        let p = write(&dir, "ok.json", &serde_json::to_string(&file).unwrap());
        assert_eq!(
            read(&p, Path::new("/r")),
            ReadOutcome::Records(file.clone())
        );
        assert_eq!(
            read(&dir.join("none.json"), Path::new("/r")),
            ReadOutcome::Missing
        );
        // Another repository, a newer version, garbage, a bad record.
        assert!(
            matches!(read(&p, Path::new("/other")), ReadOutcome::Unusable(m) if m.contains("belongs to /r"))
        );
        let newer = write(
            &dir,
            "v2.json",
            r#"{"version": 2, "repository": "/r", "agents": []}"#,
        );
        assert!(
            matches!(read(&newer, Path::new("/r")), ReadOutcome::Unusable(m) if m.contains("version 2"))
        );
        let bad = write(&dir, "bad.json", "{not json");
        assert!(
            matches!(read(&bad, Path::new("/r")), ReadOutcome::Unusable(m) if m.contains("not valid JSON"))
        );
        let mut f = file.clone();
        f.agents[1].worktree.path = "relative/path".into();
        let rel = write(&dir, "rel.json", &serde_json::to_string(&f).unwrap());
        assert!(
            matches!(read(&rel, Path::new("/r")), ReadOutcome::Unusable(m) if m.contains("not absolute"))
        );
        let mut f = file.clone();
        f.agents[0].worktree.branch = "main".into();
        let main = write(&dir, "main.json", &serde_json::to_string(&f).unwrap());
        assert!(
            matches!(read(&main, Path::new("/r")), ReadOutcome::Unusable(m) if m.contains("not a maestro/ branch"))
        );
        // A base that is not a commit id (it would be cut mid-character
        // for display), and an id that would overflow the counters.
        let mut f = file.clone();
        f.agents[0].worktree.base = "1234567é".into();
        let base = write(&dir, "base.json", &serde_json::to_string(&f).unwrap());
        assert!(
            matches!(read(&base, Path::new("/r")), ReadOutcome::Unusable(m) if m.contains("not a commit id"))
        );
        let mut f = file.clone();
        f.agents[0].id = u64::MAX;
        let id = write(&dir, "id.json", &serde_json::to_string(&f).unwrap());
        assert!(
            matches!(read(&id, Path::new("/r")), ReadOutcome::Unusable(m) if m.contains("out of range"))
        );
        // A saved contract is checked like a typed one.
        let mut f = file.clone();
        f.agents[0].contract = Some(super::super::contract::Contract {
            allowed: vec!["../outside".into()],
            ..Default::default()
        });
        let bad = write(&dir, "contract.json", &serde_json::to_string(&f).unwrap());
        assert!(
            matches!(read(&bad, Path::new("/r")), ReadOutcome::Unusable(m) if m.contains("task contract"))
        );
        // A test run's file is a path inside the worktree with a blob id;
        // integrate leaves exactly these paths out of its commit.
        for (path, blob, why) in [
            ("../escape", "abc123", "not a path inside the worktree"),
            ("/etc/passwd", "abc123", "not a path inside the worktree"),
            ("", "abc123", "not a path inside the worktree"),
            ("Cargo.lock", "--not-hex", "not an object id"),
        ] {
            let mut f = file.clone();
            f.agents[0].test_outputs = vec![SavedTestOutput {
                path: path.into(),
                blob: blob.into(),
                command: "cargo test".into(),
                agent_was_running: false,
            }];
            let bad = write(&dir, "outputs.json", &serde_json::to_string(&f).unwrap());
            assert!(
                matches!(read(&bad, Path::new("/r")), ReadOutcome::Unusable(ref m) if m.contains(why)),
                "{path:?} {blob:?}: {:?}",
                read(&bad, Path::new("/r"))
            );
        }
        let mut f = file;
        f.agents[0].kind = "robot".into();
        let kind = write(&dir, "kind.json", &serde_json::to_string(&f).unwrap());
        assert!(
            matches!(read(&kind, Path::new("/r")), ReadOutcome::Unusable(m) if m.contains("unknown provider kind"))
        );
        // A directory where the file should be is not read.
        std::fs::create_dir_all(dir.join("dir.json")).unwrap();
        assert!(
            matches!(read(&dir.join("dir.json"), Path::new("/r")), ReadOutcome::Unusable(m) if m.contains("not a regular file"))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn gone(id: u64, at: u64) -> Forgotten {
        Forgotten {
            id,
            worktree: format!("/r-worktrees/do-it-{id}"),
            created: 1_000,
            at,
        }
    }

    #[test]
    fn a_forgotten_record_is_not_read_and_bad_entries_are_refused() {
        let dir = std::env::temp_dir().join(format!("maestro-forgotten-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // Record 2 is named by an entry; record 1 has the path of another
        // entry but a different creation time: another agent, kept.
        let mut other = gone(1, 5);
        other.created = 999;
        let file = RecordsFile {
            version: VERSION,
            repository: "/r".into(),
            agents: vec![record(1), record(2)],
            forgotten: vec![other.clone(), gone(2, 6)],
        };
        let p = write(&dir, "f.json", &serde_json::to_string(&file).unwrap());
        let ReadOutcome::Records(read_back) = read(&p, Path::new("/r")) else {
            panic!("{:?}", read(&p, Path::new("/r")));
        };
        assert_eq!(read_back.agents, vec![record(1)]);
        assert_eq!(read_back.forgotten, vec![other, gone(2, 6)]);
        // A file without the list (written by 0.11.0) reads as before.
        let old = r#"{"version": 1, "repository": "/r", "agents": []}"#;
        let p = write(&dir, "old.json", old);
        assert!(
            matches!(read(&p, Path::new("/r")), ReadOutcome::Records(f) if f.forgotten.is_empty())
        );
        // An id that would overflow the counter, or a relative path.
        for bad in [
            Forgotten {
                id: u64::MAX,
                ..gone(1, 1)
            },
            Forgotten {
                id: 0,
                ..gone(1, 1)
            },
            Forgotten {
                worktree: "relative".into(),
                ..gone(1, 1)
            },
        ] {
            let f = RecordsFile {
                forgotten: vec![bad],
                ..file.clone()
            };
            let p = write(&dir, "bad.json", &serde_json::to_string(&f).unwrap());
            assert!(
                matches!(read(&p, Path::new("/r")), ReadOutcome::Unusable(ref m) if m.contains("forgotten record")),
                "{:?}",
                read(&p, Path::new("/r"))
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn forgotten_entries_merge_once_each_sorted_and_bounded() {
        // The same record forgotten in two sessions is kept once, with the
        // earlier forget; either order gives the same list.
        let mut late = gone(1, 50);
        late.id = 7;
        let ab = union_forgotten(&[gone(2, 20), late.clone()], &[gone(1, 10)]);
        let ba = union_forgotten(&[gone(1, 10)], &[late, gone(2, 20)]);
        assert_eq!(ab, vec![gone(1, 10), gone(2, 20)]);
        assert_eq!(ab, ba);
        // Past the bound the oldest forgets go.
        let many: Vec<Forgotten> = (1..=MAX_FORGOTTEN as u64 + 5).map(|i| gone(i, i)).collect();
        let kept = union_forgotten(&many, &[]);
        assert_eq!(kept.len(), MAX_FORGOTTEN);
        assert_eq!(kept[0], gone(6, 6));
        assert_eq!(kept.last(), many.last());
    }

    #[test]
    fn a_running_status_loads_as_stopped() {
        let mut r = record(3);
        r.status = "running".into();
        r.tests.push(SavedTestRun {
            command: "cargo test".into(),
            finished: false,
            code: None,
            duration_ms: 0,
            summary: None,
            tail: vec!["compiling".into()],
            dropped: 0,
            tree: Some("abcdef".into()),
            agent_was_running: false,
        });
        let a = from_record(&r, &[]);
        assert_eq!(a.status(), Status::Stopped);
        assert!(!a.is_running());
        assert_eq!(a.origin, Origin::Loaded);
        assert!(a.provider.program.is_empty(), "no provider is configured");
        let last = &a.history.last().unwrap().text;
        assert!(
            last.contains("it was running when Maestro last saved it"),
            "{last}"
        );
        assert!(last.contains("a test run had not finished"), "{last}");
        assert_eq!(a.tests[0].describe(), "killed");
        // A finished status is kept.
        let a = from_record(&record(4), &[]);
        assert_eq!(a.status(), Status::ReadyForReview);
        // An unknown one is not trusted.
        let mut r = record(5);
        r.status = "flying".into();
        assert_eq!(from_record(&r, &[]).status(), Status::Stopped);
    }

    #[test]
    fn records_keep_bounded_history_and_test_tails() {
        let mut r = record(6);
        r.history = (0..(MAX_HISTORY as u64 + 50))
            .map(|i| SavedEvent {
                at: i,
                text: format!("event {i}"),
            })
            .collect();
        let mut a = from_record(&r, &[]);
        let mut run = TestRun {
            command: "t".into(),
            started: Instant::now(),
            output: (0..500).map(|i| format!("line {i}")).collect(),
            dropped: 7,
            finished: None,
            tree: None,
            agent_was_running: false,
        };
        run.finished = Some(TestFinish {
            code: Some(0),
            duration: Duration::from_millis(1500),
            summary: None,
        });
        a.tests.push(run);
        let saved = to_record(&a).unwrap();
        assert_eq!(saved.history.len(), MAX_HISTORY);
        assert_eq!(
            saved.history.last().unwrap().text,
            a.history.last().unwrap().text
        );
        let t = &saved.tests[0];
        assert_eq!(t.tail.len(), MAX_TEST_TAIL);
        assert_eq!(t.tail.last().unwrap(), "line 499");
        assert_eq!(t.dropped, 7 + 500 - MAX_TEST_TAIL);
        assert_eq!((t.finished, t.code, t.duration_ms), (true, Some(0), 1500));
    }
}
