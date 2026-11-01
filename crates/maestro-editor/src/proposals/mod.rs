//! Edit proposals: changes an automation client asks to make to an open
//! document, held until the person at the editor approves or rejects them.
//! See `docs/automation.md` (the `proposal.*` methods) and
//! `docs/security.md`.
//!
//! A proposal is a [`ChangeSet`] computed against one version of a
//! document. The automation server checks the base version and the edits
//! when the client creates it and queues it with [`submit`]; nothing
//! changes until the user approves it in the review overlay
//! ([`review`], `space e` or `:proposals`). On approval the change is
//! applied with [`Document::apply_at_version`] against the proposal's base
//! version as one undo step, so a document that moved on is refused as
//! stale.
//!
//! An intent edit ([`submit_intent`]) is a proposal that also carries the
//! structural pipeline that produced its change ([`Intent`]). When its
//! document moves on, the pipeline runs again on the current text
//! ([`refresh`]) and the review shows that new change with a note that it
//! was rebased; a rerun that fails makes the proposal stale. Approval
//! applies exactly the change the approval dialog described.
//!
//! A proposal with a verification command then
//! saves the document and runs the command through `maestro-tasks` in the
//! editor's working directory, on threads of its own; the result arrives as
//! a job. Everything the decision and the run produce stays on the
//! [`Proposal`], which the automation server reports through
//! `proposal.status`, `document.history` and `proposal.resolved`.
//!
//! [`Document::apply_at_version`]: maestro_core::Document::apply_at_version

pub mod review;

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use maestro_core::document::{EditError, SaveError};
use maestro_core::{ChangeSet, DocumentVersion, Rope, Selection, Transaction};
use maestro_git::DiffFile;
use maestro_tasks::{TaskEvent, TaskHandle, TaskSpec};

use crate::editor::{DocumentId, Editor, Severity};
use crate::structural::{self, IntentRun, StepCount};

crate::commands! {
    proposal_review => "Review edit proposals from automation clients",
}

/// Default key bindings, added to the `space` group.
pub const DEFAULT_KEYS: &[(&str, &str)] = &[("space e", "proposal_review")];

/// Proposals waiting for review at once; `proposal.create` refuses more.
pub const MAX_PENDING: usize = 100;
/// Decided proposals kept for `proposal.status` and `document.history`;
/// older ones are dropped first.
pub const MAX_RESOLVED: usize = 500;
/// Output lines of a verification run that are kept (the last ones).
pub const OUTPUT_TAIL_LINES: usize = 40;
/// Longest output line kept, in chars; longer lines are cut.
pub const OUTPUT_LINE_CHARS: usize = 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProposalId(pub u64);

/// Who proposed the change, as the client said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Actor {
    pub name: String,
    pub agent_id: Option<String>,
}

impl Actor {
    /// `name (agent id)` or `name`.
    pub fn label(&self) -> String {
        match &self.agent_id {
            Some(id) => format!("{} (agent {id})", self.name),
            None => self.name.clone(),
        }
    }
}

/// The command a proposal asks to run after it is applied and saved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifySpec {
    pub program: String,
    pub args: Vec<String>,
    /// The editor's working directory when the proposal was made.
    pub cwd: PathBuf,
}

impl VerifySpec {
    /// The command as a shell would need it typed, every word quoted where
    /// it has to be, so the dialog shows exactly what runs. The program is
    /// quoted more strictly than the arguments (see [`quote_program`]).
    /// Builtins are the one gap: Maestro runs `program` from `PATH`, while a
    /// shell given `exec` or `cd` would run its builtin.
    pub fn display(&self) -> String {
        std::iter::once(quote_program(&self.program))
            .chain(self.args.iter().map(|w| shell_quote(w)))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// Characters that would hide or rearrange what a dialog shows: control
/// characters, and invisible or direction-changing Unicode format
/// characters.
fn hides_text(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{00AD}'
                | '\u{061C}'
                | '\u{180E}'
                | '\u{200B}'..='\u{200F}'
                | '\u{2028}'..='\u{202E}'
                | '\u{2060}'..='\u{206F}'
                | '\u{FEFF}'
                | '\u{FFF9}'..='\u{FFFB}'
        )
}

/// Quotes `w` for a POSIX shell when it needs it. Only for showing a
/// command; nothing here goes through a shell. A word with characters
/// that would hide text (a newline in an `sh -c` script, a bidirectional
/// override) is shown in Bash's `$'...'` form with those characters
/// escaped, so every character of the command is visible.
pub fn shell_quote(w: &str) -> String {
    quote_word(w, hides_text)
}

/// Shell words that a shell reads as syntax, not as a program to run.
const RESERVED: &[&str] = &[
    "!", "{", "}", "[[", "]]", "case", "coproc", "do", "done", "elif", "else", "esac", "fi", "for",
    "function", "if", "in", "select", "then", "time", "until", "while",
];

/// Quotes the program for display. Beyond [`shell_quote`]: a word with `=`
/// is quoted (unquoted, a shell reads `A=b c` as an assignment and runs
/// `c`), so is a reserved word, and every non-ASCII character is escaped,
/// so a program name cannot pass for a different one that looks the same.
pub fn quote_program(w: &str) -> String {
    if w.contains('=') || RESERVED.contains(&w) {
        if w.is_ascii() && !w.chars().any(hides_text) {
            return format!("'{}'", w.replace('\'', r"'\''"));
        }
        return ansi_c(w, |c| hides_text(c) || !c.is_ascii());
    }
    quote_word(w, |c| hides_text(c) || !c.is_ascii())
}

fn quote_word(w: &str, escape: impl Fn(char) -> bool) -> String {
    let plain = !w.is_empty()
        && w.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./=:,+@%".contains(c));
    if plain {
        return w.to_string();
    }
    if !w.chars().any(&escape) {
        return format!("'{}'", w.replace('\'', r"'\''"));
    }
    ansi_c(w, escape)
}

/// Bash's `$'...'` form, with every character `escape` picks written as a
/// `\u` (or `\U`) escape.
fn ansi_c(w: &str, escape: impl Fn(char) -> bool) -> String {
    let mut out = String::from("$'");
    for c in w.chars() {
        match c {
            '\\' => out.push_str(r"\\"),
            '\'' => out.push_str(r"\'"),
            '\n' => out.push_str(r"\n"),
            '\t' => out.push_str(r"\t"),
            '\r' => out.push_str(r"\r"),
            c if escape(c) && (c as u32) > 0xFFFF => out.push_str(&format!("\\U{:08X}", c as u32)),
            c if escape(c) => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('\'');
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Pending,
    /// Approved and applied; the verification command runs.
    Verifying,
    Resolved,
}

impl State {
    pub fn label(self) -> &'static str {
        match self {
            State::Pending => "pending",
            State::Verifying => "verifying",
            State::Resolved => "resolved",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Approved,
    Rejected,
    Withdrawn,
    Stale,
    Failed,
}

impl Decision {
    pub fn label(self) -> &'static str {
        match self {
            Decision::Approved => "approved",
            Decision::Rejected => "rejected",
            Decision::Withdrawn => "withdrawn",
            Decision::Stale => "stale",
            Decision::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecidedBy {
    /// The person at the editor, with their login name when known.
    User(Option<String>),
    /// The client that made the proposal (withdraw).
    Client,
    /// The editor (the document was closed).
    Editor,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifyStatus {
    NotRun,
    Running,
    Passed,
    Failed,
    Error,
}

impl VerifyStatus {
    pub fn label(self) -> &'static str {
        match self {
            VerifyStatus::NotRun => "not run",
            VerifyStatus::Running => "running",
            VerifyStatus::Passed => "passed",
            VerifyStatus::Failed => "failed",
            VerifyStatus::Error => "error",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyResult {
    pub status: VerifyStatus,
    pub exit_code: Option<i32>,
    pub duration: Option<Duration>,
    pub output_tail: Vec<String>,
    pub dropped: u64,
    pub error: Option<String>,
}

impl VerifyResult {
    fn simple(status: VerifyStatus, error: Option<String>) -> Self {
        Self {
            status,
            exit_code: None,
            duration: None,
            output_tail: Vec::new(),
            dropped: 0,
            error,
        }
    }

    /// One line for the status line and the overlay.
    pub fn summary(&self) -> String {
        let mut s = self.status.label().to_string();
        match (self.status, self.exit_code) {
            (VerifyStatus::Passed | VerifyStatus::Failed, Some(c)) => {
                s.push_str(&format!(" (exit {c}"))
            }
            (VerifyStatus::Failed, None) => s.push_str(" (killed by a signal"),
            _ => {}
        }
        if matches!(self.status, VerifyStatus::Passed | VerifyStatus::Failed) {
            if let Some(d) = self.duration {
                s.push_str(&format!(", {:.2} s", d.as_secs_f64()));
            }
            s.push(')');
        }
        if let Some(e) = &self.error {
            s.push_str(&format!(": {e}"));
        }
        s
    }
}

/// What the server hands over to queue a proposal. The changes were
/// checked against the document at `base_version`.
#[derive(Debug, Clone)]
pub struct NewProposal {
    pub doc: DocumentId,
    pub base_version: DocumentVersion,
    pub changes: ChangeSet,
    pub edit_count: usize,
    pub actor: Actor,
    pub description: String,
    /// Program and arguments; the working directory is the editor's.
    pub verify: Option<(String, Vec<String>)>,
}

/// The pipeline behind an intent edit, and where its preview stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Intent {
    /// The pipeline line as the client sent it.
    pub pipeline: String,
    /// The selection count the client expects after the selection steps.
    pub expect: Option<usize>,
    /// Selection counts per step of the run that produced the proposal's
    /// current change.
    pub counts: Vec<StepCount>,
    /// The document version the proposal's change applies to: the base
    /// version, or a newer one after a rebase.
    pub preview_version: DocumentVersion,
    /// The latest rerun, against a newer version, failed.
    pub failed: Option<RerunFailure>,
    /// The preview version the approval dialog described when it was
    /// opened. Approval applies only that.
    pub shown: Option<DocumentVersion>,
    /// When approval was last refused because the change differed from
    /// the dialog's (milliseconds since the Unix epoch). An `a` right after
    /// that was typed ahead, before the new diff could be drawn, so it is
    /// ignored for [`RESHOW_MS`].
    pub refused_ms: Option<u64>,
}

/// After approval was refused because the change was recomputed, `a` on
/// that proposal is ignored for this long, so keys typed ahead in one
/// burst (`y a y`) cannot approve a diff that was never on screen.
pub const RESHOW_MS: u64 = 500;

/// A rerun of an intent's pipeline that failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RerunFailure {
    pub version: DocumentVersion,
    pub message: String,
    pub counts: Vec<StepCount>,
}

impl Intent {
    /// The intent for `run`, the first run of `pipeline`.
    pub fn new(pipeline: String, expect: Option<usize>, run: &IntentRun) -> Self {
        Self {
            pipeline,
            expect,
            counts: run.counts.clone(),
            preview_version: run.version,
            failed: None,
            shown: None,
            refused_ms: None,
        }
    }
}

/// `s` with every character that would hide text (a control character, a
/// direction override) written as a `\u{...}` escape, for showing a
/// pipeline the client sent.
pub fn visible(s: &str) -> String {
    s.chars()
        .map(|c| {
            if hides_text(c) {
                format!("\\u{{{:04X}}}", c as u32)
            } else {
                c.to_string()
            }
        })
        .collect()
}

pub struct Proposal {
    pub id: ProposalId,
    pub doc: DocumentId,
    /// The document's path when the proposal was made.
    pub path: Option<PathBuf>,
    pub actor: Actor,
    pub description: String,
    pub base_version: DocumentVersion,
    pub changes: ChangeSet,
    pub edit_count: usize,
    /// The document at `base_version`, and with the change applied.
    pub old_text: Rope,
    pub new_text: Rope,
    /// The diff of `old_text` to `new_text` as the review shows it.
    pub diff: DiffFile,
    pub verify: Option<VerifySpec>,
    pub state: State,
    pub decision: Option<Decision>,
    pub decided_by: Option<DecidedBy>,
    pub reason: Option<String>,
    pub applied_version: Option<DocumentVersion>,
    pub saved: bool,
    pub result: Option<VerifyResult>,
    pub created_ms: u64,
    pub decided_ms: Option<u64>,
    pub resolved_ms: Option<u64>,
    /// Order of resolution, for `proposal.resolved` notifications.
    pub resolved_seq: Option<u64>,
    /// The pipeline, for an intent edit.
    pub intent: Option<Intent>,
    handle: Option<TaskHandle>,
    canceled: bool,
}

impl Proposal {
    /// The version the change applies to: the base version, or for a
    /// rebased intent edit the version its pipeline ran on again.
    pub fn apply_version(&self) -> DocumentVersion {
        self.intent
            .as_ref()
            .map_or(self.base_version, |i| i.preview_version)
    }

    /// True for an intent edit whose change was computed against another
    /// version than the one the client looked at.
    pub fn rebased(&self) -> bool {
        self.intent
            .as_ref()
            .is_some_and(|i| i.preview_version != self.base_version)
    }

    /// What the review says about a rebase, in short lines with the
    /// version first: why it happened, against which version, and whether
    /// the rerun failed. Empty when there was none.
    pub fn rebase_notes(&self) -> Vec<String> {
        let Some(i) = &self.intent else {
            return Vec::new();
        };
        let why = format!(
            "changed since version {}, which {} looked at",
            self.base_version.0, self.actor.name
        );
        if let Some(f) = &i.failed {
            return vec![
                format!("no longer applies: {why}"),
                format!(
                    "the pipeline ran again on version {} and failed: {}",
                    f.version.0, f.message
                ),
                "approving resolves it as stale; nothing changes".into(),
            ];
        }
        if !self.rebased() {
            return Vec::new();
        }
        vec![
            format!("rebased onto version {}: {why}", i.preview_version.0),
            format!(
                "the pipeline ran again on version {}; this diff is from that run",
                i.preview_version.0
            ),
        ]
    }

    /// Lines added and removed.
    pub fn counts(&self) -> (usize, usize) {
        self.diff.counts()
    }

    /// The name shown for the file.
    pub fn file_label(&self) -> String {
        self.path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "[scratch]".into())
    }
}

/// Every proposal this editor has seen, and the review overlay.
#[derive(Default)]
pub struct Proposals {
    /// Oldest first.
    pub items: Vec<Proposal>,
    next_id: u64,
    seq: u64,
    /// The review overlay, when open.
    pub review: Option<review::ReviewView>,
}

impl Proposals {
    pub fn get(&self, id: ProposalId) -> Option<&Proposal> {
        self.items.iter().find(|p| p.id == id)
    }

    pub fn get_mut(&mut self, id: ProposalId) -> Option<&mut Proposal> {
        self.items.iter_mut().find(|p| p.id == id)
    }

    pub fn pending_count(&self) -> usize {
        self.items
            .iter()
            .filter(|p| p.state == State::Pending)
            .count()
    }

    pub fn review_open(&self) -> bool {
        self.review.is_some()
    }

    /// The resolution counter; a proposal resolved later has a larger
    /// `resolved_seq` than this.
    pub fn last_seq(&self) -> u64 {
        self.seq
    }

    /// Proposals resolved after `seq`, in the order they were resolved.
    pub fn resolved_since(&self, seq: u64) -> Vec<&Proposal> {
        let mut out: Vec<&Proposal> = self
            .items
            .iter()
            .filter(|p| p.resolved_seq.is_some_and(|s| s > seq))
            .collect();
        out.sort_by_key(|p| p.resolved_seq);
        out
    }

    /// True while a verification command runs.
    pub fn verifying(&self) -> bool {
        self.items.iter().any(|p| p.state == State::Verifying)
    }

    fn resolve(&mut self, id: ProposalId) {
        self.seq += 1;
        let seq = self.seq;
        if let Some(p) = self.get_mut(id) {
            p.state = State::Resolved;
            p.resolved_ms = Some(now_ms());
            p.resolved_seq = Some(seq);
            if p.result.is_none() && p.verify.is_some() {
                p.result = Some(VerifyResult::simple(VerifyStatus::NotRun, None));
            }
        }
        self.prune();
    }

    /// Drops the proposals resolved longest ago, by resolution order (not
    /// creation order), so the one just resolved is never the one dropped.
    fn prune(&mut self) {
        let mut seqs: Vec<u64> = self.items.iter().filter_map(|p| p.resolved_seq).collect();
        if seqs.len() <= MAX_RESOLVED {
            return;
        }
        seqs.sort_unstable();
        let cutoff = seqs[seqs.len() - MAX_RESOLVED];
        self.items
            .retain(|p| p.resolved_seq.is_none_or(|s| s >= cutoff));
    }
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// The local user's login name, from `$USER` or `$LOGNAME`.
pub fn local_user() -> Option<String> {
    ["USER", "LOGNAME"]
        .iter()
        .filter_map(|k| std::env::var(k).ok())
        .find(|v| !v.is_empty())
}

/// The selection that best describes "where the user is" in a document:
/// the focused view if it shows it, else any view that does, else the
/// selection remembered for when it is shown again.
pub fn reference_selection(ed: &Editor, id: DocumentId) -> Selection {
    if ed.view().doc == id {
        return ed.view().selection.clone();
    }
    ed.views
        .values()
        .find(|v| v.doc == id)
        .map(|v| v.selection.clone())
        .unwrap_or_else(|| ed.buffers[&id].last_selection.clone())
}

/// Applies `changes` to document `id` only if it is still at `base`, as one
/// undo step of its own: any undo group the user had open is closed first.
/// Selections in every view of the document move with the text, and the
/// syntax tree, language servers, marks and jumps follow, as for an edit
/// from the keyboard. Returns the new version. Used by
/// `document.apply_edit` and by approving a proposal.
pub fn apply_versioned(
    ed: &mut Editor,
    id: DocumentId,
    base: DocumentVersion,
    changes: &ChangeSet,
) -> Result<DocumentVersion, EditError> {
    let before = reference_selection(ed, id);
    let focused = ed.focus;
    let insert_mode = ed.mode == crate::editor::Mode::Insert;
    let buf = ed
        .buffers
        .get_mut(&id)
        .expect("caller checked the document");
    let old_text = buf.doc.text().clone();
    // Close whatever the user had open so this edit is its own undo step.
    buf.doc.commit(&before);
    buf.doc
        .apply_at_version(&Transaction::new(changes.clone()), base, &before)?;
    buf.last_edit = Some(std::time::Instant::now());
    let new_text = buf.doc.text().clone();
    let s = new_text.slice(..);
    let after = before.map(changes).normalize(s);
    buf.doc.commit(&after);
    let version = buf.doc.version();
    for v in ed.views.values_mut().filter(|v| v.doc == id) {
        let mapped = v.selection.map(changes).normalize(s);
        v.selection = if v.id == focused && insert_mode {
            mapped
        } else {
            mapped.clamp_normal(s)
        };
    }
    ed.on_text_changed(id, &old_text, std::slice::from_ref(changes));
    crate::lsp::document_changed(ed, id, std::slice::from_ref(changes));
    Ok(version)
}

/// Saves document `id` as `:w` does (no force): the recovery file goes,
/// language servers hear about it.
pub fn save_document(ed: &mut Editor, id: DocumentId) -> Result<(), SaveError> {
    let sel = reference_selection(ed, id);
    let dir = ed.recovery_dir.clone();
    let session = ed.session_id.clone();
    let buf = ed
        .buffers
        .get_mut(&id)
        .expect("caller checked the document");
    buf.doc.save(&sel, false)?;
    buf.disk_change_reported = false;
    buf.recovery_version = None;
    if let (Some(dir), Some(p)) = (dir, buf.doc.path()) {
        crate::recovery::remove(&dir, p, &session);
    }
    crate::lsp::document_saved(ed, id);
    Ok(())
}

/// Queues a proposal for review and announces it in the status line. The
/// caller has checked that the document exists, is at `base_version`, and
/// that the changes fit it.
pub fn submit(ed: &mut Editor, new: NewProposal) -> ProposalId {
    queue(ed, new, None)
}

/// Queues an intent edit: `new.changes` is the result of running
/// `intent.pipeline` on the document at `intent.preview_version` (its
/// current version), which may be newer than `new.base_version`.
pub fn submit_intent(ed: &mut Editor, new: NewProposal, intent: Intent) -> ProposalId {
    queue(ed, new, Some(intent))
}

/// The name a diff shows for a proposal's file.
fn diff_name(path: Option<&PathBuf>) -> PathBuf {
    path.and_then(|p| p.file_name())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("[scratch]"))
}

fn queue(ed: &mut Editor, new: NewProposal, intent: Option<Intent>) -> ProposalId {
    let buf = &ed.buffers[&new.doc];
    let path = buf.doc.path().map(PathBuf::from);
    let old_text = buf.doc.text().clone();
    let mut new_text = old_text.clone();
    // The caller built the change set for this text; a mismatch would be
    // a bug, and the diff then shows no change rather than a wrong one.
    let _ = new.changes.apply(&mut new_text);
    let name = diff_name(path.as_ref());
    let diff = review::text_diff(&old_text.to_string(), &new_text.to_string(), &name);
    let verify = new.verify.map(|(program, args)| VerifySpec {
        program,
        args,
        cwd: ed.cwd.clone(),
    });
    let p = &mut ed.proposals;
    p.next_id += 1;
    let id = ProposalId(p.next_id);
    p.items.push(Proposal {
        id,
        doc: new.doc,
        path,
        actor: new.actor,
        description: new.description,
        base_version: new.base_version,
        changes: new.changes,
        edit_count: new.edit_count,
        old_text,
        new_text,
        diff,
        verify,
        state: State::Pending,
        decision: None,
        decided_by: None,
        reason: None,
        applied_version: None,
        saved: false,
        result: None,
        created_ms: now_ms(),
        decided_ms: None,
        resolved_ms: None,
        resolved_seq: None,
        intent,
        handle: None,
        canceled: false,
    });
    let pr = ed.proposals.get(id).expect("just added");
    let msg = format!(
        "proposal {} from {} for {} waits for review: space e or :proposals",
        id.0,
        pr.actor.name,
        pr.path
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "[scratch]".into()),
    );
    // Not over an error the user has not seen yet.
    if !matches!(ed.status, Some((Severity::Error, _))) {
        ed.info(msg);
    }
    id
}

/// Why a proposal could not be withdrawn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WithdrawError {
    NotFound,
    NotPending(State),
}

/// The client withdraws a pending proposal. Nothing changes in the
/// document.
pub fn withdraw(
    ed: &mut Editor,
    id: ProposalId,
    reason: Option<String>,
) -> Result<(), WithdrawError> {
    let p = ed.proposals.get_mut(id).ok_or(WithdrawError::NotFound)?;
    if p.state != State::Pending {
        return Err(WithdrawError::NotPending(p.state));
    }
    decide(p, Decision::Withdrawn, DecidedBy::Client, reason);
    ed.proposals.resolve(id);
    if ed.proposals.review_open() {
        ed.info(format!("proposal {} was withdrawn by its client", id.0));
    }
    review::after_change(ed);
    Ok(())
}

fn decide(p: &mut Proposal, d: Decision, by: DecidedBy, reason: Option<String>) {
    p.decision = Some(d);
    p.decided_by = Some(by);
    p.reason = reason.filter(|r| !r.trim().is_empty());
    p.decided_ms = Some(now_ms());
}

/// Why approving would be refused before anything is applied, if it would.
/// A proposal against an older version is resolved as stale by
/// [`approve`]; this only covers what leaves it pending.
pub fn approval_blocker(ed: &Editor, id: ProposalId) -> Option<String> {
    let p = ed.proposals.get(id)?;
    if p.state != State::Pending {
        return Some(format!(
            "proposal {} is {}; only a pending proposal can be approved",
            id.0,
            p.decision.map(Decision::label).unwrap_or(p.state.label())
        ));
    }
    let buf = ed.buffers.get(&p.doc)?;
    if p.verify.is_some() && buf.doc.changed_on_disk() {
        return Some(format!(
            "not approved: {} changed on disk since it was read, so it cannot be saved \
             for the verification command; :reload or :w! first",
            buf.doc.display_name()
        ));
    }
    None
}

/// Why the proposal is stale now, if it is: its document was closed or has
/// moved on from the base version.
pub fn stale_reason(ed: &Editor, p: &Proposal) -> Option<String> {
    match ed.buffers.get(&p.doc) {
        None => Some("the document was closed".into()),
        // An intent edit is rebased instead (`refresh`).
        Some(b) if p.intent.is_none() && b.doc.version() != p.base_version => Some(format!(
            "the document is at version {}, the proposal was made against version {}",
            b.doc.version().0,
            p.base_version.0
        )),
        // `:w other` renames a buffer without changing its version. The
        // review showed the old path; saving would write the new one.
        Some(b)
            if b.doc.path().map(crate::editor::normalize_path)
                != p.path.as_deref().map(crate::editor::normalize_path) =>
        {
            Some(format!(
                "the document was saved as {} since the proposal was made for {}",
                b.doc.display_name(),
                p.file_label()
            ))
        }
        Some(b) => {
            let f = p.intent.as_ref()?.failed.as_ref()?;
            (f.version == b.doc.version()).then(|| {
                format!(
                    "the document moved on from version {} to {}, and the pipeline ran again \
                     on it and failed: {}",
                    p.base_version.0, f.version.0, f.message
                )
            })
        }
    }
}

/// Runs a pending intent edit's pipeline again when its document has
/// moved on since its preview was computed, and replaces the preview (the
/// change, the texts and the diff) with the new run, or records why the
/// rerun failed. Does nothing for other proposals, and nothing when the
/// preview is current or already failed at this version.
pub fn refresh(ed: &mut Editor, id: ProposalId) {
    let Some(p) = ed.proposals.get(id) else {
        return;
    };
    let Some(intent) = &p.intent else {
        return;
    };
    let Some(buf) = ed.buffers.get(&p.doc) else {
        return;
    };
    let current = buf.doc.version();
    if p.state != State::Pending
        || current == intent.preview_version
        || intent.failed.as_ref().is_some_and(|f| f.version == current)
    {
        return;
    }
    let result = structural::run_intent(ed, p.doc, &intent.pipeline, intent.expect);
    let old_text = buf.doc.text().clone();
    let name = diff_name(p.path.as_ref());
    let p = ed.proposals.get_mut(id).expect("checked");
    match result {
        Ok(run) => {
            let mut new_text = old_text.clone();
            let _ = run.changes.apply(&mut new_text);
            p.diff = review::text_diff(&old_text.to_string(), &new_text.to_string(), &name);
            p.old_text = old_text;
            p.new_text = new_text;
            p.edit_count = run.edit_count;
            p.changes = run.changes;
            let intent = p.intent.as_mut().expect("checked");
            intent.counts = run.counts;
            intent.preview_version = current;
            intent.failed = None;
        }
        Err(e) => {
            let counts = match &e {
                structural::IntentError::Failed { counts, .. } => counts.clone(),
                structural::IntentError::Invalid(_) => Vec::new(),
            };
            let intent = p.intent.as_mut().expect("checked");
            intent.failed = Some(RerunFailure {
                version: current,
                message: e.to_string(),
                counts,
            });
        }
    }
}

/// [`refresh`] for every pending intent edit. The front end calls this
/// before drawing while the review is open, so the diff shown is the one
/// approval would apply.
pub fn refresh_all(ed: &mut Editor) {
    let ids: Vec<ProposalId> = ed
        .proposals
        .items
        .iter()
        .filter(|p| p.intent.is_some() && p.state == State::Pending)
        .map(|p| p.id)
        .collect();
    for id in ids {
        refresh(ed, id);
    }
}

/// Remembers that the approval dialog for proposal `id` describes its
/// current preview, so approval may apply it.
pub fn mark_shown(ed: &mut Editor, id: ProposalId) {
    if let Some(i) = ed.proposals.get_mut(id).and_then(|p| p.intent.as_mut()) {
        i.shown = Some(i.preview_version);
    }
}

/// The user approved proposal `id` (after the confirmation). Applies it
/// against its base version as one undo step, or resolves it as stale if
/// the document moved on. With a verification command, then saves the
/// document and starts the command.
pub fn approve(ed: &mut Editor, id: ProposalId) {
    refresh(ed, id);
    if let Some(why) = approval_blocker(ed, id) {
        ed.error(why);
        return;
    }
    let Some(p) = ed.proposals.get(id) else {
        return;
    };
    let user = DecidedBy::User(local_user());
    if let Some(why) = stale_reason(ed, p) {
        let p = ed.proposals.get_mut(id).expect("checked");
        decide(p, Decision::Stale, user, Some(why.clone()));
        ed.proposals.resolve(id);
        ed.error(format!(
            "proposal {} is stale and was not applied: {why}",
            id.0
        ));
        review::after_change(ed);
        return;
    }
    // An intent edit applies only the change the dialog described: if the
    // pipeline ran again since (the document moved on while the dialog was
    // open), the reviewer has to look at the new change first.
    if let Some(i) = &p.intent
        && i.shown != Some(i.preview_version)
    {
        let msg = format!(
            "proposal {}: the document changed while the approval dialog was open, so the \
             pipeline ran again on version {}; nothing was applied. Review the new diff and \
             press a again",
            id.0, i.preview_version.0
        );
        if let Some(i) = ed.proposals.get_mut(id).and_then(|p| p.intent.as_mut()) {
            i.refused_ms = Some(now_ms());
        }
        ed.error(msg);
        review::after_change(ed);
        return;
    }
    let (doc, base, changes) = (p.doc, p.apply_version(), p.changes.clone());
    let applied = apply_versioned(ed, doc, base, &changes);
    let p = ed.proposals.get_mut(id).expect("checked");
    match applied {
        Ok(v) => {
            decide(p, Decision::Approved, user, None);
            p.applied_version = Some(v);
        }
        Err(e) => {
            let d = match e {
                EditError::Stale { .. } => Decision::Stale,
                _ => Decision::Failed,
            };
            decide(p, d, user, Some(e.to_string()));
            ed.proposals.resolve(id);
            ed.error(format!("proposal {} was not applied: {e}", id.0));
            review::after_change(ed);
            return;
        }
    }
    if p.verify.is_none() {
        let rebased = if p.rebased() {
            format!(" (rebased onto version {})", base.0)
        } else {
            String::new()
        };
        ed.proposals.resolve(id);
        ed.info(format!(
            "proposal {} applied{rebased} as one undo step; not saved",
            id.0
        ));
        review::after_change(ed);
        return;
    }
    // The verification command reads the file, so the file must hold the
    // change.
    if let Err(e) = save_document(ed, doc) {
        let p = ed.proposals.get_mut(id).expect("checked");
        p.result = Some(VerifyResult::simple(
            VerifyStatus::Error,
            Some(format!("not run: saving failed: {e}")),
        ));
        ed.proposals.resolve(id);
        ed.error(format!(
            "proposal {} applied, but saving failed ({e}); the verification command did not run",
            id.0
        ));
        review::after_change(ed);
        return;
    }
    let p = ed.proposals.get_mut(id).expect("checked");
    p.saved = true;
    start_verification(ed, id);
    review::after_change(ed);
}

/// The user rejected proposal `id`, with an optional reason for the client.
pub fn reject(ed: &mut Editor, id: ProposalId, reason: Option<String>) {
    let Some(p) = ed.proposals.get_mut(id) else {
        return;
    };
    if p.state != State::Pending {
        let state = p.state.label();
        ed.error(format!(
            "proposal {} is {state}; only a pending proposal can be rejected",
            id.0
        ));
        return;
    }
    decide(p, Decision::Rejected, DecidedBy::User(local_user()), reason);
    ed.proposals.resolve(id);
    ed.info(format!("proposal {} rejected; nothing changed", id.0));
    review::after_change(ed);
}

#[derive(Default)]
struct Tail {
    lines: VecDeque<String>,
    dropped: u64,
}

fn start_verification(ed: &mut Editor, id: ProposalId) {
    let tx = ed.idle_job_sender();
    let Some(p) = ed.proposals.get_mut(id) else {
        return;
    };
    let Some(v) = p.verify.clone() else {
        return;
    };
    let spec = TaskSpec {
        program: v.program.clone(),
        args: v.args.clone(),
        cwd: v.cwd.clone(),
    };
    // Only the tail is kept, and one job is sent when the run ends, so a
    // chatty command cannot queue unbounded work for the editor.
    let tail: Arc<Mutex<Tail>> = Default::default();
    let started = maestro_tasks::start(&spec, move |event| match event {
        TaskEvent::Output(line) => {
            let mut t = tail.lock().unwrap_or_else(|e| e.into_inner());
            let line: String = line.chars().take(OUTPUT_LINE_CHARS).collect();
            t.lines.push_back(line);
            if t.lines.len() > OUTPUT_TAIL_LINES {
                t.lines.pop_front();
                t.dropped += 1;
            }
        }
        TaskEvent::Finished { code, duration } => {
            let t = std::mem::take(&mut *tail.lock().unwrap_or_else(|e| e.into_inner()));
            tx.send(move |ed| finished(ed, id, code, duration, t));
        }
    });
    match started {
        Ok(handle) => {
            p.handle = Some(handle);
            p.state = State::Verifying;
            p.result = Some(VerifyResult::simple(VerifyStatus::Running, None));
            ed.info(format!(
                "proposal {} applied and saved; running {}",
                id.0,
                v.display()
            ));
        }
        Err(e) => {
            p.result = Some(VerifyResult::simple(
                VerifyStatus::Error,
                Some(format!("could not start {}: {e}", v.program)),
            ));
            ed.proposals.resolve(id);
            ed.error(format!(
                "proposal {} applied and saved, but {} could not start: {e}",
                id.0, v.program
            ));
        }
    }
}

fn finished(ed: &mut Editor, id: ProposalId, code: Option<i32>, duration: Duration, tail: Tail) {
    let Some(p) = ed.proposals.get_mut(id) else {
        return;
    };
    if p.state != State::Verifying {
        return;
    }
    p.handle = None;
    let status = if code == Some(0) {
        VerifyStatus::Passed
    } else {
        VerifyStatus::Failed
    };
    let result = VerifyResult {
        status,
        exit_code: code,
        duration: Some(duration),
        output_tail: tail.lines.into_iter().collect(),
        dropped: tail.dropped,
        // A cancel that came after the command exited 0 changed nothing.
        error: (p.canceled && status != VerifyStatus::Passed)
            .then(|| "canceled by the user".to_string()),
    };
    let msg = format!("proposal {} verification {}", id.0, result.summary());
    p.result = Some(result);
    ed.proposals.resolve(id);
    if status == VerifyStatus::Passed {
        ed.info(msg);
    } else {
        ed.error(msg);
    }
}

/// Stops a running verification command (`c` in the review). Its result
/// says it was canceled.
pub fn cancel_verification(ed: &mut Editor, id: ProposalId) {
    match ed.proposals.get_mut(id) {
        Some(p) if p.state == State::Verifying => {
            p.canceled = true;
            if let Some(h) = &p.handle {
                h.kill();
            }
            ed.info(format!("stopping proposal {}'s verification command", id.0));
        }
        _ => ed.error("no verification command runs for this proposal"),
    }
}

/// A document was closed: its pending proposals can never apply.
pub fn document_closed(ed: &mut Editor, doc: DocumentId) {
    let ids: Vec<ProposalId> = ed
        .proposals
        .items
        .iter()
        .filter(|p| p.doc == doc && p.state == State::Pending)
        .map(|p| p.id)
        .collect();
    for id in ids {
        if let Some(p) = ed.proposals.get_mut(id) {
            decide(
                p,
                Decision::Stale,
                DecidedBy::Editor,
                Some("the document was closed".into()),
            );
        }
        ed.proposals.resolve(id);
    }
    review::after_change(ed);
}

/// Stops running verification commands, for quitting.
pub fn stop_all(ed: &mut Editor) {
    for p in &mut ed.proposals.items {
        if let Some(h) = &p.handle {
            p.canceled = true;
            h.kill();
        }
    }
}

fn proposal_review(ed: &mut Editor) {
    if ed.proposals.review_open() {
        review::close(ed);
    } else {
        review::open(ed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_are_shown_with_every_character_visible() {
        let v = VerifySpec {
            program: "sh".into(),
            args: vec![
                "-c".into(),
                "grep -q 'it''s' f".into(),
                "".into(),
                "a\nb".into(),
                "safe\u{202E}txt.exe".into(),
            ],
            cwd: PathBuf::from("/"),
        };
        assert_eq!(
            v.display(),
            r"sh -c 'grep -q '\''it'\'''\''s'\'' f' '' $'a\nb' $'safe\u202Etxt.exe'"
        );
        assert_eq!(shell_quote("cargo"), "cargo");
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("back\\slash\t"), r"$'back\\slash\t'");
        // A program with `=` would read as an assignment; reserved words
        // as syntax; non-ASCII letters as other letters.
        assert_eq!(quote_program("CHECK=./runner"), "'CHECK=./runner'");
        assert_eq!(quote_program("time"), "'time'");
        assert_eq!(quote_program("sh"), "sh");
        assert_eq!(quote_program("\u{0455}h"), r"$'\u0455h'");
        assert_eq!(shell_quote("\u{0455}h"), "'\u{0455}h'");
        let v = VerifySpec {
            program: "CHECK=./runner".into(),
            args: vec!["true".into()],
            cwd: PathBuf::from("/"),
        };
        assert_eq!(v.display(), "'CHECK=./runner' true");
    }
}
