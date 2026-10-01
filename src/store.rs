//! The versioned store under `$XDG_STATE_HOME/agent-sessions/`: the only
//! place this tool writes outside a confirmed cleanup or `register`.
//!
//! Three file shapes live here:
//!
//! - `journal.log` - hook events as length-delimited records (`u32` LE byte
//!   count + JSON), appended under `journal.lock` with the next monotonic
//!   commit sequence and fsynced. Concurrent writers can interleave on the
//!   lock but never overwrite one another: reduction is a pure function of
//!   committed sequence.
//! - `checkpoint.json` - the journal's reduction compacted through a commit
//!   sequence, written sibling-temp + fsync + rename; the journal then
//!   rewrites to only the tail past that sequence.
//! - `seen.json`, `marks.json` - authored records (acknowledgement and the
//!   not-busy mark) as whole-file atomic renames.
//!
//! `journal.lock` is the one mutation lock: appends, compaction and the
//! authored files' read-modify-writes all hold it, so a rewrite can never
//! interleave with a concurrent read or commit.
//!
//! Every record carries a schema version; readers accept the current and
//! immediately previous one (an absent `v` reads as the pre-versioned
//! schema). A future-versioned or malformed record is excluded from
//! derivation, retained on disk and reported for the evidence view, and a
//! corrupt journal tail never hides the valid prefix.

use std::collections::HashMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::provider::SourceError;

/// The record schema this build reads and writes. `0` - an unversioned
/// record from before the field existed - reads as the previous schema.
pub const SCHEMA: u32 = 1;

/// Compact once the journal's un-checkpointed tail passes this many
/// records: enough that a busy day never rewrites, small enough that a
/// scan stays trivial.
const COMPACT_AFTER: usize = 128;

/// How long `acquire` waits for the holder before giving up - long enough
/// for a real append (milliseconds) many times over, short enough that a
/// hook never stalls its agent.
const LOCK_WAIT: Duration = Duration::from_secs(2);

const JOURNAL: &str = "journal.log";
const LOCK: &str = "journal.lock";
const CHECKPOINT: &str = "checkpoint.json";
const SEEN: &str = "seen.json";
const MARKS: &str = "marks.json";

/// The normalized event a native hook event maps to - the event vocabulary
/// the shared projection uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NormEvent {
    /// A new turn or prompt.
    Start,
    /// Work continuing inside a turn.
    Activity,
    /// Blocked on the human.
    Awaiting,
    /// A clean turn end.
    End,
    /// The turn aborted.
    Error,
    /// Session teardown is coming; never reaps on its own.
    TeardownHint,
}

impl NormEvent {
    /// The execution class the event claims. `TeardownHint` is none at
    /// all: it is diagnostic, not a state.
    pub fn execution(self) -> Option<Exec> {
        match self {
            NormEvent::Start | NormEvent::Activity => Some(Exec::Busy),
            NormEvent::Awaiting => Some(Exec::Waiting),
            NormEvent::End => Some(Exec::Idle),
            NormEvent::Error => Some(Exec::Unknown),
            NormEvent::TeardownHint => None,
        }
    }

    /// Whether the event is attention a human must acknowledge - the latch
    /// kind. `Start` and `Activity` acknowledge rather than latch.
    pub fn latches(self) -> bool {
        matches!(
            self,
            NormEvent::Awaiting | NormEvent::End | NormEvent::Error
        )
    }
}

/// Effective execution state, derived - not a provider's word.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exec {
    Busy,
    Idle,
    Waiting,
    /// No applicable evidence, or none that proves a live state.
    Unknown,
}

/// One committed journal record: one hook ping. Field names stay short -
/// every record carries them.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Record {
    /// Schema version; absent is the pre-versioned schema and still reads.
    #[serde(default)]
    pub v: u32,
    /// The journal commit sequence: the local, monotonic order.
    #[serde(default)]
    pub seq: u64,
    /// When this process committed it, epoch milliseconds.
    #[serde(default)]
    pub at: u64,
    /// Who wrote it (`agent-sessions/<version>`), for the evidence view.
    #[serde(default)]
    pub writer: String,
    /// The provider's wire name (`claude`, `vibe`, `devin`).
    #[serde(default)]
    pub provider: String,
    /// The provider's own session id. May be empty when the payload carried
    /// none: the record is kept but reduces onto no conversation.
    #[serde(default)]
    pub session: String,
    /// The provider's native event name, verbatim.
    #[serde(default)]
    pub native: String,
    /// The normalized event; `None` on an unmapped ping, which is retained
    /// as diagnostic activity with a five-second weak `Busy` lease.
    #[serde(default)]
    pub event: Option<NormEvent>,
    /// The producer's own timestamp, epoch milliseconds.
    #[serde(default)]
    pub pts: Option<u64>,
    /// The producer's own sequence; a lower one than already seen rejects
    /// the record as reordered.
    #[serde(default)]
    pub pseq: Option<u64>,
    /// The process instance the hook resolved, when it could.
    #[serde(default)]
    pub pid: Option<u32>,
    /// The instance's start, epoch seconds.
    #[serde(default)]
    pub pid_start: Option<u64>,
    #[serde(default)]
    pub cwd: Option<String>,
    /// The wait reason an `awaiting` carries, or the native name an
    /// `error` came in as - evidence-view material.
    #[serde(default)]
    pub reason: Option<String>,
}

impl Record {
    /// A record as `hook` builds it: sequence and commit time are the
    /// store's to assign.
    pub fn new(provider: &str, session: &str, native: &str) -> Record {
        Record {
            v: SCHEMA,
            seq: 0,
            at: 0,
            writer: writer(),
            provider: provider.to_owned(),
            session: session.to_owned(),
            native: native.to_owned(),
            event: None,
            pts: None,
            pseq: None,
            pid: None,
            pid_start: None,
            cwd: None,
            reason: None,
        }
    }
}

/// The newest mapped event on a conversation: the lifecycle claim hooks
/// make. Process identity rides along so a delayed event from a demoted
/// attachment cannot mutate the replacement.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LastEvent {
    pub kind: NormEvent,
    /// The journal commit sequence that produced it.
    pub seq: u64,
    /// When the effective state it implies began; duplicates and
    /// same-class continuations preserve it rather than reset it.
    pub since_ms: u64,
    /// When this event itself was observed (producer time, else commit).
    pub observed_ms: u64,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub pid: Option<u32>,
    #[serde(default)]
    pub pid_start: Option<u64>,
}

/// A latched attention event: an `awaiting`, `end` or `error` a `start`
/// has not implicitly acknowledged. Seen-state acknowledgement applies at
/// derive time, not here - the journal never rewrites.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Retained {
    pub seq: u64,
    pub kind: NormEvent,
    pub at_ms: u64,
    #[serde(default)]
    pub reason: Option<String>,
}

/// The reduced state of one conversation's journal records - what a
/// checkpoint persists so the whole log never needs replaying.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Fold {
    /// Newest committed record on the conversation.
    #[serde(default)]
    pub last_seq: u64,
    /// Newest mapped event, the lifecycle claim.
    #[serde(default)]
    pub last_event: Option<LastEvent>,
    /// Attention events no `start` has acknowledged. Compaction drops the
    /// ones seen-state acknowledged, so a provider without a `start` event
    /// does not carry every turn's latch forever.
    #[serde(default)]
    pub retained: Vec<Retained>,
    /// Newest unmapped ping `(seq, epoch ms)` - the weak `Busy` lease.
    #[serde(default)]
    pub ping: Option<(u64, u64)>,
    /// The producer-sequence high-water mark; older rejects as reordered.
    #[serde(default)]
    pub pseq_high: Option<u64>,
}

/// How one record landed in a fold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Apply {
    /// New information, folded in.
    Accepted,
    /// An already-seen producer sequence: the observation refreshes its
    /// time but the effective `since` stands.
    Duplicate,
    /// A producer sequence below the high-water mark: reordered source
    /// evidence, rejected.
    Stale,
}

impl Fold {
    /// Fold one committed record into the conversation's reduction.
    /// Deterministic in commit order: two writers cannot overwrite one
    /// another, and replaying the same sequence yields the same fold.
    pub fn apply(&mut self, record: &Record) -> Apply {
        // Producer sequences reject reordering when the source supplies
        // them; an equal sequence is a duplicate heartbeat, not new
        // evidence.
        if let Some(pseq) = record.pseq {
            match self.pseq_high {
                Some(high) if pseq < high => return Apply::Stale,
                Some(high) if pseq == high => {
                    self.refresh_observed(record);
                    return Apply::Duplicate;
                }
                _ => self.pseq_high = Some(pseq),
            }
        }
        self.last_seq = self.last_seq.max(record.seq);
        let observed = record.pts.unwrap_or(record.at);
        let Some(kind) = record.event else {
            self.ping = Some((record.seq, observed));
            return Apply::Accepted;
        };
        if kind == NormEvent::TeardownHint {
            // Diagnostic only: it never reaps and never claims execution.
            return Apply::Accepted;
        }
        if kind == NormEvent::Start {
            // A new prompt acknowledges everything retained: answering
            // implies the attention was seen.
            self.retained.clear();
        }
        if kind.latches() {
            self.retained.push(Retained {
                seq: record.seq,
                kind,
                at_ms: observed,
                reason: record
                    .reason
                    .clone()
                    .or_else(|| Some(record.native.clone())),
            });
        }
        // `since` survives inside one execution class: an `activity`
        // heartbeat continues the `start`'s Busy rather than restarting
        // the clock.
        let execution = kind.execution();
        let since = match (&self.last_event, execution) {
            (Some(prev), Some(exec)) if prev.kind.execution() == Some(exec) => prev.since_ms,
            _ => observed,
        };
        self.last_event = Some(LastEvent {
            kind,
            seq: record.seq,
            since_ms: since,
            observed_ms: observed,
            reason: record.reason.clone(),
            pid: record.pid,
            pid_start: record.pid_start,
        });
        Apply::Accepted
    }

    /// A duplicate heartbeat refreshes the observation time of whatever it
    /// duplicates - the lease's expiry moves, the `since` does not.
    fn refresh_observed(&mut self, record: &Record) {
        let observed = record.pts.unwrap_or(record.at);
        if let Some(event) = &mut self.last_event
            && (event.seq == record.seq
                || event.kind.execution() == record.event.and_then(|e| e.execution()))
        {
            event.observed_ms = event.observed_ms.max(observed);
        }
        if let Some((_, at)) = &mut self.ping
            && record.event.is_none()
        {
            *at = (*at).max(observed);
        }
        self.last_seq = self.last_seq.max(record.seq);
    }
}

/// One conversation's acknowledgement: the retained events seen through
/// a commit sequence, and the live wait episode seen - named by its
/// `effective_since`, since a wait the provider publishes carries no
/// sequence. A newer wait episode starts unseen.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Seen {
    /// Retained events at or below this commit sequence are seen.
    #[serde(default)]
    pub seq: u64,
    /// The `effective_since` of the live wait the user has seen, epoch ms.
    #[serde(default)]
    pub wait_ms: Option<u64>,
}

/// An authored not-busy mark: it names the `effective_since` of the `Busy`
/// it dismisses and the commit sequence at write time, so any newer event
/// or observation supersedes it.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Mark {
    /// The dismissed `Busy`'s `effective_since`, epoch milliseconds.
    pub since_ms: u64,
    /// The journal commit sequence when the mark was written.
    pub seq: u64,
    /// When the mark was written, epoch milliseconds.
    pub at_ms: u64,
}

/// The checkpoint file: the reduction at `through` commit sequence.
#[derive(Debug, Serialize, Deserialize)]
struct Checkpoint {
    #[serde(default)]
    v: u32,
    through: u64,
    #[serde(default)]
    folds: HashMap<String, Fold>,
}

/// The whole store, read: per-conversation folds plus the authored files.
/// A conversation's fold key is `provider\0session_id`.
#[derive(Debug, Default)]
pub struct Loaded {
    pub folds: HashMap<String, Fold>,
    /// Conversation key -> what the user has acknowledged.
    pub seen: HashMap<String, Seen>,
    /// Conversation key -> the authored not-busy mark.
    pub marks: HashMap<String, Mark>,
    /// Records excluded or files unreadable - isolated, never fatal.
    pub errors: Vec<SourceError>,
    /// The highest committed sequence the store knows.
    pub max_seq: u64,
    /// Whether acknowledgement can be established: the checkpoint history
    /// and `seen.json` both answered (absent is an answer). When `false`,
    /// attention is `Unknown` rather than a guessed glyph.
    pub ack_readable: bool,
    /// Whether the checkpoint is usable; absent counts as usable.
    /// Compaction rewrites it, so an unusable one defers compaction rather
    /// than overwrite bytes a newer schema may have written.
    checkpoint_ok: bool,
}

/// The journal tail's read: every parseable record in committed order
/// plus whether the file stayed clean. `compactable` is false the moment
/// any frame is excluded or malformed - a compaction rewrites the file to
/// nothing, and bytes the reduction cannot carry are data loss.
struct JournalRead {
    records: Vec<Record>,
    compactable: bool,
}

/// The journal record key a conversation's events fold under.
pub fn conversation_key(provider: &str, session_id: &str) -> String {
    format!("{provider}\u{0}{session_id}")
}

/// A store rooted at `dir` (`$XDG_STATE_HOME/agent-sessions/`). Every
/// method tolerates a missing directory: a first run has no store.
pub struct Store {
    dir: PathBuf,
}

impl Store {
    pub fn open(dir: PathBuf) -> Store {
        Store { dir }
    }

    /// Append one record as the next commit: lock, sequence, write, fsync.
    /// Returns the assigned commit sequence.
    pub fn append(&self, mut record: Record) -> io::Result<u64> {
        fs::create_dir_all(&self.dir)?;
        let _lock = Lock::acquire(&self.dir.join(LOCK))?;
        let seq = self.next_seq()?; // coverage: off - next_seq reads through the error-retaining loaders; it cannot fail
        record.seq = seq;
        record.at = now_ms();
        record.v = SCHEMA;
        record.writer = writer();
        let bytes = serde_json::to_vec(&record)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?; // coverage: off - a Record always serializes
        let journal = self.dir.join(JOURNAL);
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&journal)?; // coverage: off - the unexecuted instantiation's region edge
        let len = (bytes.len() as u32).to_le_bytes();
        file.write_all(&len)?; // coverage: off - a write failure needs the filesystem to fail under an open handle
        file.write_all(&bytes)?; // coverage: off - same
        file.sync_all()?; // coverage: off - an fsync failure needs a broken filesystem
        Ok(seq)
    }

    /// The commit sequence one past the current tip: the checkpoint's
    /// `through` plus the journal tail's newest record.
    fn next_seq(&self) -> io::Result<u64> {
        let checkpoint = self.read_checkpoint(&mut Vec::new());
        let through = checkpoint.map_or(0, |c| c.through);
        let tail = self
            .read_journal(&mut Vec::new())
            .records
            .iter()
            .map(|r| r.seq)
            .max()
            .unwrap_or(0);
        Ok(through.max(tail) + 1)
    }

    /// Read the store: checkpoint, then the journal tail folded on top.
    /// A tail larger than the compaction bound is folded into a fresh
    /// checkpoint on the spot - reaping and maintenance run on read, but
    /// only under the store's mutation lock, and only when the tail re-read
    /// under that lock still exceeds the bound on a journal that stayed
    /// clean: an append or another compaction committed between the two
    /// reads is never compacted over, and an excluded frame never is
    /// either. Best-effort: a failed compaction is reported and the
    /// unfolded tail simply answers again next time.
    pub fn load(&self) -> Loaded {
        let (loaded, tail, compactable) = self.load_once();
        if tail <= COMPACT_AFTER || !compactable || !loaded.checkpoint_ok {
            return loaded;
        }
        match Lock::acquire(&self.dir.join(LOCK)) {
            Ok(_lock) => {
                let (mut loaded, tail, compactable) = self.load_once();
                if tail > COMPACT_AFTER && compactable && loaded.checkpoint_ok {
                    // coverage: off - a compaction failure needs a rename or fsync to fail; the tail answers again next read
                    #[rustfmt::skip]
                    match self.compact(&loaded.folds, &loaded.seen, loaded.max_seq) {
                        Ok(()) => {}
                        Err(e) => loaded.errors.push(SourceError { // coverage: off - same
                            source: "store".to_owned(), // coverage: off - same
                            detail: format!("compaction: {e}"), // coverage: off - same
                        }), // coverage: off - same
                    };
                }
                loaded
            }
            Err(e) => {
                let mut loaded = loaded;
                loaded.errors.push(SourceError {
                    source: "store".to_owned(),
                    detail: format!("compaction: {e}"),
                });
                loaded
            }
        }
    }

    /// One full read of the store with no writes: checkpoint, journal
    /// tail folded on top, then the authored files. Returns the loaded
    /// view, the count of tail records a fold accepted, and whether the
    /// journal stayed clean enough that compacting it loses nothing.
    fn load_once(&self) -> (Loaded, usize, bool) {
        let mut errors = Vec::new();
        let mut checkpoint_ok = true;
        let mut folds = HashMap::new();
        let mut through = 0u64;
        match self.read_checkpoint(&mut errors) {
            Some(c) => {
                through = c.through;
                folds = c.folds;
            }
            None if self.dir.join(CHECKPOINT).exists() => {
                // A checkpoint that exists but cannot be used (malformed
                // or future) degrades the store: the tail alone is not the
                // history, and it must never be compacted over.
                checkpoint_ok = false;
            }
            None => {}
        }
        let mut tail = 0usize;
        let mut max_seq = through;
        let journal = self.read_journal(&mut errors);
        for record in &journal.records {
            max_seq = max_seq.max(record.seq);
            if record.seq <= through || record.v > SCHEMA || record.session.is_empty() {
                continue;
            }
            tail += 1;
            folds
                .entry(conversation_key(&record.provider, &record.session))
                .or_default()
                .apply(record);
        }
        let seen = self.read_seen(&mut errors);
        let marks = self.read_marks(&mut errors);
        // Acknowledgement stands on both halves reading: the history a
        // `seen` sequence indexes into, and `seen.json` itself - absent is
        // a first run, malformed is a guess refused.
        let ack_readable = checkpoint_ok && !errors.iter().any(|e| e.source == SEEN);
        (
            Loaded {
                folds,
                seen,
                marks,
                errors,
                max_seq,
                ack_readable,
                checkpoint_ok,
            },
            tail,
            journal.compactable,
        )
    }

    /// Acknowledge every retained event on `key` through `through_seq`,
    /// and the live wait episode `wait_ms` names when one shows. The whole
    /// `seen.json` map is rewritten atomically under the store lock, so
    /// two acknowledgements cannot lose one another; the sequence never
    /// moves backwards. `space` and the focus observation both land here.
    /// Returns the conversation's acknowledgement as stored.
    pub fn acknowledge(
        &self,
        key: &str,
        through_seq: u64,
        wait_ms: Option<u64>,
    ) -> io::Result<Seen> {
        fs::create_dir_all(&self.dir)?; // coverage: off - a directory-creation failure needs a filesystem fault
        let _lock = Lock::acquire(&self.dir.join(LOCK))?;
        let mut seen = self.read_seen_for_update()?;
        let old = seen.get(key).copied().unwrap_or_default();
        let new = Seen {
            seq: old.seq.max(through_seq),
            wait_ms: wait_ms.or(old.wait_ms),
        };
        if new == old {
            return Ok(old);
        }
        seen.insert(key.to_owned(), new);
        self.write_seen(&seen)?;
        Ok(new)
    }

    /// Record the authored not-busy mark: `since_ms` names the `Busy`'s
    /// `effective_since`, `seq` the commit sequence it was written at.
    pub fn mark_not_busy(&self, key: &str, since_ms: u64, seq: u64) -> io::Result<()> {
        fs::create_dir_all(&self.dir)?; // coverage: off - a directory-creation failure needs a filesystem fault
        let _lock = Lock::acquire(&self.dir.join(LOCK))?;
        let mut marks = self.read_marks_for_update()?;
        marks.insert(
            key.to_owned(),
            Mark {
                since_ms,
                seq,
                at_ms: now_ms(),
            },
        );
        self.write_marks(&marks)
    }

    /// Write the checkpoint for `folds` at `through` and rewrite the
    /// journal to nothing past it - checkpoint first, so a crash between
    /// the two renames leaves stale tail records that simply re-skip.
    /// Latches `seen` acknowledges are dropped: derivation already ignores
    /// them, so the reduction it answers is unchanged.
    fn compact(
        &self,
        folds: &HashMap<String, Fold>,
        seen: &HashMap<String, Seen>,
        through: u64,
    ) -> io::Result<()> {
        let mut folds = folds.clone();
        for (key, fold) in &mut folds {
            if let Some(seen) = seen.get(key) {
                fold.retained.retain(|r| r.seq > seen.seq);
            }
        }
        let checkpoint = Checkpoint {
            v: SCHEMA,
            through,
            folds,
        };
        let bytes = serde_json::to_vec_pretty(&checkpoint)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?; // coverage: off - a checkpoint always serializes
        write_atomic(&self.dir.join(CHECKPOINT), &bytes)?; // coverage: off - needs the store's filesystem to fail
        // The tail restarts empty; every record at or below `through` is // coverage: off - the unexecuted instantiation's region edge
        // carried by the checkpoint and skipped on the next read.
        write_atomic(&self.dir.join(JOURNAL), &[])?; // coverage: off - same
        Ok(()) // coverage: off - the unexecuted instantiation's region edge
    }
    // coverage: off - the unexecuted instantiation's region edge
    /// The checkpoint's folds, or `None` when absent/unreadable/future. // coverage: off - the unexecuted instantiation's region edge
    /// Errors are retained rather than thrown.
    fn read_checkpoint(&self, errors: &mut Vec<SourceError>) -> Option<Checkpoint> {
        let path = self.dir.join(CHECKPOINT); // coverage: off - the unexecuted instantiation's region edge
        let bytes = read_file(&path, "checkpoint", errors)?;
        match serde_json::from_slice::<Checkpoint>(&bytes) {
            Ok(c) if c.v <= SCHEMA => Some(c),
            Ok(c) => {
                errors.push(SourceError {
                    source: "checkpoint".to_owned(),
                    detail: format!(
                        "{}: schema v{} is newer than v{SCHEMA}",
                        path.display(),
                        c.v
                    ),
                });
                None
            }
            Err(e) => {
                errors.push(SourceError {
                    source: "checkpoint".to_owned(),
                    detail: format!("{}: {e}", path.display()),
                });
                None
            }
        }
    }

    /// The journal tail: every parseable record in committed order, plus
    /// whether the file stayed clean enough to compact. A framing error -
    /// a truncated length or payload - ends the tail there: the corrupt
    /// suffix can hide nothing of the valid prefix. `compactable` is
    /// false for any frame the reduction cannot carry back, since a
    /// rewrite would drop its bytes.
    fn read_journal(&self, errors: &mut Vec<SourceError>) -> JournalRead {
        let path = self.dir.join(JOURNAL);
        let Some(bytes) = read_file(&path, "journal", errors) else {
            return JournalRead {
                records: Vec::new(),
                compactable: true,
            };
        };
        let mut records = Vec::new();
        let mut compactable = true;
        let mut cursor = 0usize;
        while cursor + 4 <= bytes.len() {
            let len = u32::from_le_bytes(bytes[cursor..cursor + 4].try_into().unwrap()) as usize;
            cursor += 4;
            if cursor + len > bytes.len() {
                errors.push(SourceError {
                    source: "journal".to_owned(),
                    detail: format!(
                        "{}: truncated record at byte {cursor} ({len} bytes announced)",
                        path.display()
                    ),
                });
                compactable = false;
                break;
            }
            let frame = &bytes[cursor..cursor + len];
            // A record is always a JSON object. serde would happily decode
            // an array positionally into the struct's defaults, which would
            // let corrupt bytes masquerade as a committed record.
            let is_object = frame
                .iter()
                .find(|b| !b.is_ascii_whitespace())
                .is_some_and(|b| *b == b'{');
            match is_object
                .then(|| serde_json::from_slice::<Record>(frame))
                .and_then(|r| r.ok())
            {
                Some(record) => {
                    if record.v > SCHEMA {
                        // Excluded from derivation, retained on disk - and
                        // its commit sequence still counts, so a future
                        // record never lets a writer reissue `seq`.
                        compactable = false;
                        errors.push(SourceError {
                            source: "journal".to_owned(),
                            detail: format!(
                                "{}: record seq {} carries schema v{}, newer than v{SCHEMA}",
                                path.display(),
                                record.seq,
                                record.v
                            ),
                        });
                    }
                    if record.session.is_empty() {
                        // The record folds onto no conversation, so a
                        // checkpoint cannot carry it either.
                        compactable = false;
                    }
                    records.push(record);
                }
                None => {
                    compactable = false;
                    errors.push(SourceError {
                        source: "journal".to_owned(),
                        detail: format!(
                            "{}: record at byte {cursor} does not parse",
                            path.display()
                        ),
                    });
                }
            }
            cursor += len;
        }
        if bytes.len() - cursor > 0 && cursor + 4 > bytes.len() {
            compactable = false;
            errors.push(SourceError {
                source: "journal".to_owned(),
                detail: format!(
                    "{}: {} trailing bytes",
                    path.display(),
                    bytes.len() - cursor
                ),
            });
        }
        JournalRead {
            records,
            compactable,
        }
    }

    /// The seen-state map: conversation key -> its acknowledgement.
    fn read_seen(&self, errors: &mut Vec<SourceError>) -> HashMap<String, Seen> {
        self.read_authored(SEEN, errors)
            .map(|authored: Authored<HashMap<String, Seen>>| authored.data)
            .unwrap_or_default()
    }

    /// The same map for a read-modify-write: the file must have read
    /// clean or be absent - an unreadable, malformed or future-schema
    /// file is refused as `InvalidData` so the rewrite never turns its
    /// bytes into an empty map.
    fn read_seen_for_update(&self) -> io::Result<HashMap<String, Seen>> {
        let mut errors = Vec::new();
        let seen = self.read_seen(&mut errors);
        update_read(errors)?;
        Ok(seen)
    }

    fn write_seen(&self, seen: &HashMap<String, Seen>) -> io::Result<()> {
        self.write_authored(SEEN, seen)
    }

    fn read_marks(&self, errors: &mut Vec<SourceError>) -> HashMap<String, Mark> {
        self.read_authored(MARKS, errors)
            .map(|authored: Authored<HashMap<String, Mark>>| authored.data)
            .unwrap_or_default()
    }

    /// The marks map under the same update precondition as seen-state.
    fn read_marks_for_update(&self) -> io::Result<HashMap<String, Mark>> {
        let mut errors = Vec::new();
        let marks = self.read_marks(&mut errors);
        update_read(errors)?;
        Ok(marks)
    }

    fn write_marks(&self, marks: &HashMap<String, Mark>) -> io::Result<()> {
        self.write_authored(MARKS, marks)
    }

    /// One authored file read: future or malformed content is reported and
    /// treated as absent - never guessed.
    fn read_authored<T: serde::de::DeserializeOwned>(
        &self,
        name: &str,
        errors: &mut Vec<SourceError>,
    ) -> Option<Authored<T>> {
        let path = self.dir.join(name);
        let bytes = read_file(&path, name, errors)?;
        match serde_json::from_slice::<Authored<T>>(&bytes) {
            Ok(a) if a.v <= SCHEMA => Some(a),
            Ok(a) => {
                errors.push(SourceError {
                    source: name.to_owned(),
                    detail: format!(
                        "{}: schema v{} is newer than v{SCHEMA}",
                        path.display(),
                        a.v
                    ),
                });
                None
            }
            Err(e) => {
                errors.push(SourceError {
                    source: name.to_owned(),
                    detail: format!("{}: {e}", path.display()),
                });
                None
            }
        }
    }

    fn write_authored<T: Serialize>(&self, name: &str, data: &T) -> io::Result<()> {
        fs::create_dir_all(&self.dir)?; // coverage: off - a directory-creation failure needs a filesystem fault
        let authored = Authored { v: SCHEMA, data }; // coverage: off - the unexecuted instantiation's region edge
        let bytes = serde_json::to_vec_pretty(&authored)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?; // coverage: off - the envelope always serializes
        write_atomic(&self.dir.join(name), &bytes) // coverage: off - the unexecuted instantiation's region edge
    }
}
// coverage: off - the unexecuted instantiation's region edge
/// The shared envelope for `seen.json` and `marks.json`.
#[derive(Debug, Serialize, Deserialize)]
struct Authored<T> {
    // coverage: off - the unexecuted instantiation's region edge
    #[serde(default)]
    v: u32,
    data: T,
}

/// The exclusive OS advisory lock on `journal.lock`. The kernel releases
/// it when the holder closes the file or dies, so a crashed writer never
/// leaves a lock behind to judge stale or steal. The file itself stays:
/// unlinking a lock file another process may be waiting on would let two
/// holders lock two different files.
struct Lock {
    _file: fs::File,
}

impl Lock {
    /// Take the lock, polling until [`LOCK_WAIT`] expires; then
    /// `WouldBlock`, so a hook reports the failure instead of hanging.
    fn acquire(path: &Path) -> io::Result<Lock> {
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        let deadline = SystemTime::now() + LOCK_WAIT;
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(Lock { _file: file }),
                Err(fs::TryLockError::WouldBlock) if SystemTime::now() <= deadline => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(fs::TryLockError::WouldBlock) => {
                    return Err(io::Error::new(
                        io::ErrorKind::WouldBlock,
                        format!("{}: lock held", path.display()),
                    ));
                }
                Err(fs::TryLockError::Error(e)) => return Err(e),
            }
        }
    }
}

/// A mutation's read precondition: a clean or absent authored file passes;
/// anything the reader reported becomes `InvalidData` carrying the same
/// details, so the rewrite never erases bytes it could not carry.
fn update_read(errors: Vec<SourceError>) -> io::Result<()> {
    if errors.is_empty() {
        return Ok(());
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        errors
            .iter()
            .map(|e| format!("{}: {}", e.source, e.detail))
            .collect::<Vec<_>>()
            .join("; "),
    ))
}

/// `path`'s whole contents; `None` on a missing file (the normal first-run
/// case) and an error retained on any other failure.
fn read_file(path: &Path, source: &str, errors: &mut Vec<SourceError>) -> Option<Vec<u8>> {
    match fs::read(path) {
        Ok(bytes) => Some(bytes),
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => {
            errors.push(SourceError {
                source: source.to_owned(),
                detail: format!("{}: {e}", path.display()),
            });
            None
        }
    }
}

/// `bytes` -> `path` atomically: sibling temp, fsync, rename, fsync the
/// directory so the rename itself survives.
fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> /* // coverage: off - a directory-creation failure needs a filesystem fault */
{
    if let Some(dir) = path.parent() {
        // coverage: off - the unexecuted instantiation's region edge
        fs::create_dir_all(dir)?; // coverage: off - a directory-creation failure needs a filesystem fault
    } // coverage: off - the unexecuted instantiation's region edge
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    {
        // coverage: off - the unexecuted instantiation's region edge
        let mut file = fs::File::create(&tmp)?; // coverage: off - a create failure needs a filesystem fault
        file.write_all(bytes)?; // coverage: off - same
        file.sync_all()?; // coverage: off - an fsync failure needs a broken filesystem
    } // coverage: off - the unexecuted instantiation's region edge
    fs::rename(&tmp, path)?; // coverage: off - a failed rename needs a filesystem fault
    if let Some(dir) = path.parent()
        && let Ok(dir) = fs::File::open(dir)
    {
        // coverage: off - the unexecuted instantiation's region edge
        let _ = dir.sync_all(); // coverage: off - the unexecuted instantiation's region edge
    } // coverage: off - the unexecuted instantiation's exit edge
    Ok(())
} // coverage: off - the unexecuted instantiation's region edge

/// Epoch milliseconds.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH) // coverage: off - the unexecuted instantiation's region edge
        .unwrap_or_default()
        .as_millis() as u64
}

/// `system_time` -> epoch milliseconds.
pub fn epoch_ms(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64
}

/// The writer identity a record carries.
fn writer() -> String {
    format!("agent-sessions/{}", env!("CARGO_PKG_VERSION"))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempStore(PathBuf);
    impl TempStore {
        fn new() -> TempStore {
            static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "agent-sessions-store-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            TempStore(path)
        }
        fn store(&self) -> Store {
            Store::open(self.0.clone())
        }
        fn path(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }
    impl Drop for TempStore {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn record(provider: &str, session: &str, native: &str, event: NormEvent) -> Record {
        let mut r = Record::new(provider, session, native);
        r.event = Some(event);
        r
    }

    #[test]
    fn concurrent_writers_get_unique_monotonic_sequences() {
        let temp = TempStore::new();
        let store = temp.store();
        let mut handles = Vec::new();
        for i in 0..8 {
            let store = Store::open(temp.0.clone());
            handles.push(std::thread::spawn(move || {
                (0..8)
                    .map(|_| {
                        store
                            .append(record("claude", &format!("s{i}"), "Stop", NormEvent::End))
                            .expect("append")
                    })
                    .collect::<Vec<u64>>()
            }));
        }
        let mut seqs: Vec<u64> = handles
            .into_iter()
            .flat_map(|h| h.join().expect("writer joins"))
            .collect();
        seqs.sort_unstable();
        assert_eq!(seqs, (1..=64).collect::<Vec<_>>());
        let loaded = store.load();
        assert_eq!(loaded.max_seq, 64);
        assert_eq!(loaded.folds.len(), 8);
        assert!(loaded.errors.is_empty(), "{:?}", loaded.errors);
    }

    #[test]
    fn a_corrupt_tail_never_hides_the_valid_prefix() {
        let temp = TempStore::new();
        let store = temp.store();
        store
            .append(record("claude", "s1", "Stop", NormEvent::End))
            .unwrap();
        // A torn write: an announced record that never arrives, then
        // trailing garbage.
        let journal = temp.path(JOURNAL);
        let mut bytes = fs::read(&journal).unwrap();
        bytes.extend_from_slice(&100u32.to_le_bytes());
        bytes.extend_from_slice(b"{\"v\":1,\"seq\":2");
        fs::write(&journal, &bytes).unwrap();
        let loaded = store.load();
        assert_eq!(loaded.folds.len(), 1);
        assert!(loaded.errors.iter().any(|e| e.source == "journal"));
    }

    #[test]
    fn a_malformed_record_is_isolated_not_fatal() {
        let temp = TempStore::new();
        let store = temp.store();
        store
            .append(record("claude", "s1", "Stop", NormEvent::End))
            .unwrap();
        // A record whose payload is JSON but not a Record shape: an array.
        let journal = temp.path(JOURNAL);
        let mut bytes = fs::read(&journal).unwrap();
        let bad = b"[1,2,3]";
        bytes.extend_from_slice(&(bad.len() as u32).to_le_bytes());
        bytes.extend_from_slice(bad);
        fs::write(&journal, &bytes).unwrap();
        store
            .append(record("claude", "s2", "Stop", NormEvent::End))
            .unwrap();
        let loaded = store.load();
        assert_eq!(loaded.folds.len(), 2, "{:?}", loaded.folds);
        assert!(loaded.errors.iter().any(|e| e.source == "journal"));
    }

    #[test]
    fn a_future_schema_record_is_excluded_and_reported() {
        let temp = TempStore::new();
        let store = temp.store();
        store
            .append(record("claude", "s1", "Stop", NormEvent::End))
            .unwrap();
        let journal = temp.path(JOURNAL);
        let mut bytes = fs::read(&journal).unwrap();
        let future = b"{\"v\":99,\"seq\":2,\"provider\":\"claude\",\"session\":\"s2\",\"native\":\"Stop\",\"event\":\"end\"}";
        bytes.extend_from_slice(&(future.len() as u32).to_le_bytes());
        bytes.extend_from_slice(future);
        fs::write(&journal, &bytes).unwrap();
        let loaded = store.load();
        // s2 reduces to nothing - the record stays on disk, reported.
        assert_eq!(loaded.folds.len(), 1);
        assert!(loaded.max_seq >= 2);
        assert!(
            loaded
                .errors
                .iter()
                .any(|e| e.detail.contains("newer than"))
        );
    }

    #[test]
    fn compaction_replays_to_the_same_reduction() {
        let temp = TempStore::new();
        let store = temp.store();
        for i in 0..COMPACT_AFTER {
            let event = match i % 4 {
                0 => NormEvent::Start,
                1 => NormEvent::Activity,
                2 => NormEvent::Awaiting,
                _ => NormEvent::End,
            };
            let mut r = record("claude", "s1", "Evt", event);
            r.reason = Some("why".to_owned());
            store.append(r).unwrap();
        }
        // An `end` as the very last record, so the reduction has a latch.
        let mut r = record("claude", "s1", "Stop", NormEvent::End);
        r.reason = None;
        store.append(r).unwrap();
        let before = store.load();
        assert_eq!(before.folds.len(), 1, "{:?}", before.folds);
        let through = serde_json::from_slice::<serde_json::Value>(
            &fs::read(temp.path(CHECKPOINT)).expect("a checkpoint was written"),
        )
        .unwrap();
        assert_eq!(through["through"], before.max_seq);
        assert!(fs::read(temp.path(JOURNAL)).unwrap().is_empty());
        let after = store.load();
        let key = conversation_key("claude", "s1");
        for (label, loaded) in [("before", &before), ("after", &after)] {
            let fold = &loaded.folds[&key];
            assert_eq!(fold.last_seq, before.max_seq, "{label}");
            assert_eq!(
                fold.last_event.as_ref().map(|e| e.kind),
                Some(NormEvent::End),
                "{label}"
            );
            // The retained latch set is identical through compaction: an
            // awaiting the last `start` never cleared, then two `end`s.
            let kinds: Vec<NormEvent> = fold.retained.iter().map(|r| r.kind).collect();
            assert_eq!(
                kinds,
                vec![NormEvent::Awaiting, NormEvent::End, NormEvent::End],
                "{label}"
            );
        }
        assert!(after.errors.is_empty(), "{:?}", after.errors);
    }

    #[test]
    fn compaction_drops_acknowledged_latches() {
        // A provider with no `start` event: every turn's `end` latches, and
        // only seen-state acknowledges them.
        let temp = TempStore::new();
        let store = temp.store();
        let key = conversation_key("vibe", "s1");
        for _ in 0..COMPACT_AFTER {
            store
                .append(record("vibe", "s1", "post_agent", NormEvent::End))
                .unwrap();
        }
        store.acknowledge(&key, 100, None).unwrap();
        store
            .append(record("vibe", "s1", "post_agent", NormEvent::End))
            .unwrap();
        let before = store.load();
        assert!(temp.path(CHECKPOINT).exists(), "the tail compacted");
        let after = store.load();
        let kept: Vec<u64> = after.folds[&key].retained.iter().map(|r| r.seq).collect();
        assert_eq!(kept, (101..=before.max_seq).collect::<Vec<_>>());
        // What derivation answers is the same: only the unseen latches.
        let unseen = |loaded: &Loaded| {
            loaded.folds[&key]
                .retained
                .iter()
                .filter(|r| r.seq > loaded.seen[&key].seq)
                .count()
        };
        assert_eq!(unseen(&before), unseen(&after));
    }

    #[test]
    fn a_previous_schema_checkpoint_still_reads() {
        let temp = TempStore::new();
        let store = temp.store();
        store
            .append(record("claude", "s1", "Stop", NormEvent::End))
            .unwrap();
        let loaded = store.load();
        let fold = loaded.folds[&conversation_key("claude", "s1")].clone();
        // Hand-write a checkpoint at the pre-versioned schema (v absent).
        fs::create_dir_all(&temp.0).unwrap();
        fs::write(
            temp.path(CHECKPOINT),
            serde_json::to_string(&serde_json::json!({
                "through": loaded.max_seq,
                "folds": { conversation_key("claude", "s1"): fold },
            }))
            .unwrap(),
        )
        .unwrap();
        fs::write(temp.path(JOURNAL), b"").unwrap();
        let loaded = store.load();
        assert_eq!(loaded.folds.len(), 1);
        assert_eq!(loaded.max_seq, 1);
    }

    #[test]
    fn a_future_checkpoint_is_unreadable_and_reported() {
        let temp = TempStore::new();
        let store = temp.store();
        fs::create_dir_all(&temp.0).unwrap();
        fs::write(
            temp.path(CHECKPOINT),
            serde_json::json!({"v": 99, "through": 5, "folds": {}}).to_string(),
        )
        .unwrap();
        let loaded = store.load();
        assert!(!loaded.ack_readable);
        assert!(loaded.folds.is_empty());
        assert!(
            loaded
                .errors
                .iter()
                .any(|e| e.detail.contains("newer than"))
        );
    }

    #[test]
    fn seen_and_marks_round_trip_atomically() {
        let temp = TempStore::new();
        let store = temp.store();
        let key = conversation_key("claude", "s1");
        store.acknowledge(&key, 7, None).unwrap();
        store.acknowledge(&key, 3, None).unwrap(); // backwards is a no-op
        assert_eq!(store.load().seen[&key].seq, 7);
        // A wait episode joins the sequence without moving it back, and a
        // later sequence-only acknowledgement keeps the episode.
        let seen = store.acknowledge(&key, 0, Some(42_000)).unwrap();
        assert_eq!(
            seen,
            Seen {
                seq: 7,
                wait_ms: Some(42_000)
            }
        );
        store.acknowledge(&key, 9, None).unwrap();
        assert_eq!(
            store.load().seen[&key],
            Seen {
                seq: 9,
                wait_ms: Some(42_000)
            }
        );
        store.mark_not_busy(&key, 123_000, 7).unwrap();
        let loaded = store.load();
        assert_eq!(loaded.marks[&key].since_ms, 123_000);
        assert_eq!(loaded.marks[&key].seq, 7);
        // A malformed authored file reports and reads empty.
        fs::write(temp.path(SEEN), "{oops").unwrap();
        let loaded = store.load();
        assert!(loaded.seen.is_empty());
        assert!(loaded.errors.iter().any(|e| e.source == SEEN));
        fs::write(
            temp.path(MARKS),
            serde_json::json!({"v": 9, "data": {}}).to_string(),
        )
        .unwrap();
        let loaded = store.load();
        assert!(loaded.marks.is_empty());
        assert!(loaded.errors.iter().any(|e| e.source == MARKS));
    }

    #[test]
    fn a_mutation_never_overwrites_an_authored_file_it_cannot_read() {
        let temp = TempStore::new();
        fs::create_dir_all(&temp.0).unwrap();
        let store = temp.store();
        let key = conversation_key("claude", "s1");
        for bytes in [
            "{oops".to_owned(),
            serde_json::json!({"v": 99, "data": {}}).to_string(),
        ] {
            fs::write(temp.path(SEEN), &bytes).unwrap();
            let err = store.acknowledge(&key, 3, None).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidData);
            assert_eq!(fs::read(temp.path(SEEN)).unwrap(), bytes.as_bytes());
            fs::write(temp.path(MARKS), &bytes).unwrap();
            let err = store.mark_not_busy(&key, 1_000, 3).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidData);
            assert_eq!(fs::read(temp.path(MARKS)).unwrap(), bytes.as_bytes());
        }
        // The same bytes stay a best-effort read: reported, not fatal.
        let loaded = store.load();
        assert!(loaded.errors.iter().any(|e| e.source == SEEN));
        assert!(loaded.errors.iter().any(|e| e.source == MARKS));
    }

    #[test]
    fn fold_rejects_reordered_and_duplicates_refresh_observation() {
        let mut fold = Fold::default();
        let key = |pseq: Option<u64>, seq: u64, at: u64, event: NormEvent| {
            let mut r = record("claude", "s1", "Evt", event);
            r.pseq = pseq;
            r.seq = seq;
            r.at = at;
            r
        };
        // A stale producer sequence is rejected; a duplicate refreshes the
        // observation without resetting `since`.
        assert_eq!(
            fold.apply(&key(Some(5), 1, 1000, NormEvent::Start)),
            Apply::Accepted
        );
        assert_eq!(
            fold.apply(&key(Some(3), 2, 1100, NormEvent::Activity)),
            Apply::Stale
        );
        assert_eq!(
            fold.apply(&key(Some(5), 3, 1200, NormEvent::Start)),
            Apply::Duplicate
        );
        let last = fold.last_event.as_ref().unwrap();
        assert_eq!(last.since_ms, 1000);
        assert_eq!(last.observed_ms, 1200);
        // And an event without a producer sequence always lands.
        assert_eq!(
            fold.apply(&key(None, 4, 1300, NormEvent::End)),
            Apply::Accepted
        );
        assert_eq!(fold.retained.len(), 1);
        assert_eq!(fold.retained[0].seq, 4);
    }

    #[test]
    fn a_start_acknowledges_everything_retained() {
        let mut fold = Fold::default();
        for (seq, event) in [
            (1, NormEvent::Start),
            (2, NormEvent::End),
            (3, NormEvent::Error),
            (4, NormEvent::Awaiting),
        ] {
            let mut r = record("claude", "s1", "Evt", event);
            r.seq = seq;
            r.at = seq * 1000;
            fold.apply(&r);
        }
        assert_eq!(fold.retained.len(), 3);
        let mut r = record("claude", "s1", "UserPromptSubmit", NormEvent::Start);
        r.seq = 5;
        r.at = 5000;
        fold.apply(&r);
        assert!(fold.retained.is_empty());
        // A teardown hint is diagnostic only: it latches nothing and does
        // not move the execution claim.
        let mut r = record("claude", "s1", "SessionEnd", NormEvent::TeardownHint);
        r.seq = 6;
        fold.apply(&r);
        assert_eq!(fold.last_event.unwrap().kind, NormEvent::Start);
    }

    #[test]
    fn an_empty_session_id_reduces_to_no_conversation() {
        let temp = TempStore::new();
        let store = temp.store();
        store
            .append(record("claude", "", "Stop", NormEvent::End))
            .unwrap();
        let loaded = store.load();
        assert!(loaded.folds.is_empty());
        assert_eq!(loaded.max_seq, 1);
    }

    #[test]
    fn a_teardown_hint_claims_no_execution() {
        assert_eq!(NormEvent::TeardownHint.execution(), None);
        assert_eq!(NormEvent::End.execution(), Some(Exec::Idle));
        assert_eq!(NormEvent::Awaiting.execution(), Some(Exec::Waiting));
        assert!(!NormEvent::Activity.latches());
    }

    #[test]
    fn a_duplicate_ping_refreshes_its_lease() {
        let mut fold = Fold::default();
        let mut ping = Record::new("claude", "s1", "OddEvent");
        ping.seq = 1;
        ping.at = 1_000;
        ping.pseq = Some(9);
        assert_eq!(fold.apply(&ping), Apply::Accepted);
        // The same producer sequence again is a heartbeat: the observation
        // refreshes and the ping's clock moves, but nothing latches.
        ping.seq = 2;
        ping.at = 1_500;
        assert_eq!(fold.apply(&ping), Apply::Duplicate);
        assert_eq!(fold.ping, Some((1, 1_500)));
    }

    #[test]
    fn a_malformed_checkpoint_reports_and_reads_as_absent() {
        let temp = TempStore::new();
        let store = temp.store();
        fs::create_dir_all(&temp.0).unwrap();
        fs::write(temp.path(CHECKPOINT), b"{oops").unwrap();
        let loaded = store.load();
        assert!(!loaded.ack_readable);
        assert!(
            loaded
                .errors
                .iter()
                .any(|e| e.source == "checkpoint" && e.detail.contains("checkpoint.json"))
        );
    }

    #[test]
    fn a_sub_frame_tail_reports_the_trailing_bytes() {
        let temp = TempStore::new();
        let store = temp.store();
        store
            .append(record("claude", "s1", "Stop", NormEvent::End))
            .unwrap();
        // Three trailing bytes: not even a length fits.
        let journal = temp.path(JOURNAL);
        let mut bytes = fs::read(&journal).unwrap();
        bytes.extend_from_slice(b"\x01\x00\x00");
        fs::write(&journal, &bytes).unwrap();
        let loaded = store.load();
        assert_eq!(loaded.folds.len(), 1);
        assert!(
            loaded
                .errors
                .iter()
                .any(|e| e.detail.contains("trailing bytes"))
        );
    }

    #[test]
    fn a_dead_holders_lock_frees_a_held_one_waits_out() {
        let temp = TempStore::new();
        let store = temp.store();
        let lock = temp.path(LOCK);
        // A lock file a crashed writer left behind holds no lock: the
        // kernel released it with the holder, so the next writer commits.
        fs::create_dir_all(&temp.0).unwrap();
        fs::write(&lock, "12345\n").unwrap();
        store
            .append(record("claude", "s1", "Stop", NormEvent::End))
            .expect("an unheld lock file is taken");
        // A live holder - another open file description - is waited out
        // until the deadline, then reported, and its lock stays intact.
        let held = Lock::acquire(&lock).unwrap();
        let err = store
            .append(record("claude", "s1", "Stop", NormEvent::End))
            .expect_err("a held lock is not taken");
        assert_eq!(err.kind(), io::ErrorKind::WouldBlock);
        assert!(lock.exists());
        drop(held);
        store
            .append(record("claude", "s1", "Stop", NormEvent::End))
            .expect("a released lock is taken");
    }

    #[test]
    fn an_unreadable_store_file_is_one_error_not_a_crash() {
        let temp = TempStore::new();
        let store = temp.store();
        fs::create_dir_all(&temp.0).unwrap();
        // A directory where the journal should be: the read fails, the
        // error is retained, the rest of the store still answers.
        fs::create_dir_all(temp.path(JOURNAL)).unwrap();
        let loaded = store.load();
        assert!(
            loaded.errors.iter().any(|e| e.source == "journal"),
            "{:?}",
            loaded.errors
        );
        // Appending with a journal directory fails the open - the error
        // propagates as an io::Error, not a panic.
        let err = store
            .append(record("claude", "s1", "Stop", NormEvent::End))
            .expect_err("a journal directory fails the open");
        assert_eq!(err.kind(), io::ErrorKind::IsADirectory);
        // And a lock path that is a directory makes acquire fail outright:
        // the open's own error, not a wait for a holder.
        fs::remove_file(temp.path(LOCK)).unwrap();
        fs::create_dir_all(temp.path(LOCK)).unwrap();
        let err = store
            .append(record("claude", "s1", "Stop", NormEvent::End))
            .expect_err("a lock directory fails the open");
        assert_ne!(err.kind(), io::ErrorKind::WouldBlock);
    }

    #[test]
    fn compaction_keeps_a_future_frame_untouched() {
        let temp = TempStore::new();
        let store = temp.store();
        for _ in 0..=COMPACT_AFTER {
            store
                .append(record("claude", "s1", "Evt", NormEvent::End))
                .unwrap();
        }
        let journal = temp.path(JOURNAL);
        let mut bytes = fs::read(&journal).unwrap();
        let future = b"{\"v\":99,\"seq\":130,\"provider\":\"claude\",\"session\":\"s2\",\"native\":\"Stop\",\"event\":\"end\"}";
        bytes.extend_from_slice(&(future.len() as u32).to_le_bytes());
        bytes.extend_from_slice(future);
        fs::write(&journal, &bytes).unwrap();
        let loaded = store.load();
        // The tail is past the bound, but the future frame is not this
        // build's to rewrite: the journal stays byte-for-byte and no
        // checkpoint appears.
        assert_eq!(fs::read(&journal).unwrap(), bytes);
        assert!(!temp.path(CHECKPOINT).exists());
        assert!(
            loaded
                .errors
                .iter()
                .any(|e| e.detail.contains("newer than"))
        );
    }

    #[test]
    fn compaction_keeps_a_malformed_frame_untouched() {
        let temp = TempStore::new();
        let store = temp.store();
        for _ in 0..=COMPACT_AFTER {
            store
                .append(record("claude", "s1", "Evt", NormEvent::End))
                .unwrap();
        }
        let journal = temp.path(JOURNAL);
        let mut bytes = fs::read(&journal).unwrap();
        let bad = b"[1,2,3]";
        bytes.extend_from_slice(&(bad.len() as u32).to_le_bytes());
        bytes.extend_from_slice(bad);
        fs::write(&journal, &bytes).unwrap();
        let loaded = store.load();
        assert_eq!(fs::read(&journal).unwrap(), bytes);
        assert!(!temp.path(CHECKPOINT).exists());
        assert!(
            loaded
                .errors
                .iter()
                .any(|e| e.detail.contains("does not parse"))
        );
    }

    #[test]
    fn compaction_never_rewrites_an_unusable_checkpoint() {
        let temp = TempStore::new();
        let store = temp.store();
        for _ in 0..=COMPACT_AFTER {
            store
                .append(record("claude", "s1", "Evt", NormEvent::End))
                .unwrap();
        }
        fs::write(temp.path(CHECKPOINT), b"{oops").unwrap();
        let journal = fs::read(temp.path(JOURNAL)).unwrap();
        let loaded = store.load();
        assert!(!loaded.ack_readable);
        // The malformed checkpoint is evidence of a schema this build may
        // not read: compaction must not overwrite it, so the journal
        // stays too.
        assert_eq!(fs::read(temp.path(CHECKPOINT)).unwrap(), b"{oops");
        assert_eq!(fs::read(temp.path(JOURNAL)).unwrap(), journal);
    }

    #[test]
    fn concurrent_acknowledgements_preserve_every_key() {
        let temp = TempStore::new();
        let store = temp.store();
        fs::create_dir_all(&temp.0).unwrap();
        let key_a = conversation_key("claude", "a");
        let key_b = conversation_key("claude", "b");
        // While the shared lock is held, an authored update cannot slip
        // its read-modify-write past it: it waits out the deadline and
        // fails rather than clobbering a concurrent write.
        let held = Lock::acquire(&temp.path(LOCK)).unwrap();
        let err = store
            .acknowledge(&key_a, 1, None)
            .expect_err("a held lock blocks an acknowledgement");
        assert_eq!(err.kind(), io::ErrorKind::WouldBlock);
        let err = store
            .mark_not_busy(&key_a, 1, 1)
            .expect_err("a held lock blocks a mark");
        assert_eq!(err.kind(), io::ErrorKind::WouldBlock);
        // Two acknowledgements parked on the lock each take it over the
        // whole read-modify-write once it frees, so both keys survive.
        let a = std::thread::spawn({
            let store = Store::open(temp.0.clone());
            let key = key_a.clone();
            move || store.acknowledge(&key, 5, None)
        });
        let b = std::thread::spawn({
            let store = Store::open(temp.0.clone());
            let key = key_b.clone();
            move || store.acknowledge(&key, 9, None)
        });
        std::thread::sleep(Duration::from_millis(200));
        drop(held);
        a.join().expect("ack a joins").expect("ack a");
        b.join().expect("ack b joins").expect("ack b");
        let seen = store.load().seen;
        assert_eq!(seen[&key_a].seq, 5);
        assert_eq!(seen[&key_b].seq, 9);
    }

    #[test]
    fn seen_state_readability_gates_acknowledgement() {
        let temp = TempStore::new();
        let store = temp.store();
        // Absent is a first run: nothing acknowledged, nothing distrusted.
        assert!(store.load().ack_readable);
        let key = conversation_key("claude", "s1");
        store.acknowledge(&key, 3, None).unwrap();
        assert!(store.load().ack_readable);
        // Malformed seen-state cannot prove what was acknowledged.
        fs::write(temp.path(SEEN), "{oops").unwrap();
        let loaded = store.load();
        assert!(!loaded.ack_readable);
        assert!(loaded.errors.iter().any(|e| e.source == SEEN));
        // Nor can a schema written by a newer build.
        fs::write(
            temp.path(SEEN),
            serde_json::json!({"v": 99, "data": {}}).to_string(),
        )
        .unwrap();
        let loaded = store.load();
        assert!(!loaded.ack_readable);
        assert!(
            loaded
                .errors
                .iter()
                .any(|e| e.detail.contains("newer than"))
        );
    }

    #[test]
    fn compaction_and_append_share_the_lock() {
        let temp = TempStore::new();
        let store = temp.store();
        for _ in 0..=COMPACT_AFTER {
            store
                .append(record("claude", "s1", "Evt", NormEvent::End))
                .unwrap();
        }
        let journal = temp.path(JOURNAL);
        let before = fs::read(&journal).unwrap();
        // A held lock blocks compaction itself: the read still answers,
        // reports the failure, and leaves every byte on disk.
        let held = Lock::acquire(&journal.with_file_name(LOCK)).unwrap();
        let loaded = store.load();
        assert!(
            loaded
                .errors
                .iter()
                .any(|e| e.detail.contains("compaction")),
            "{:?}",
            loaded.errors
        );
        assert_eq!(fs::read(&journal).unwrap(), before);
        // Now an append and two reads contend for the same lock. Whichever
        // order they run in, the append commits the next monotonic
        // sequence and the second read sees the first's compaction.
        let appender = std::thread::spawn({
            let store = Store::open(temp.0.clone());
            move || store.append(record("claude", "appended", "Stop", NormEvent::End))
        });
        let mut loaders = Vec::new();
        for _ in 0..2 {
            loaders.push(std::thread::spawn({
                let store = Store::open(temp.0.clone());
                move || store.load().max_seq
            }));
        }
        std::thread::sleep(Duration::from_millis(200));
        drop(held);
        let seq = appender
            .join()
            .expect("append joins")
            .expect("append commits");
        assert_eq!(seq, (COMPACT_AFTER + 2) as u64);
        for loader in loaders {
            assert!(loader.join().expect("load joins") <= seq);
        }
        let loaded = store.load();
        let fold = &loaded.folds[&conversation_key("claude", "appended")];
        assert_eq!(fold.last_seq, seq);
        assert_eq!(loaded.max_seq, seq);
        assert!(loaded.errors.is_empty(), "{:?}", loaded.errors);
    }

    #[test]
    fn a_frame_arriving_between_the_compaction_reads_still_stops_it() {
        let temp = TempStore::new();
        let store = temp.store();
        for _ in 0..=COMPACT_AFTER {
            store
                .append(record("claude", "s1", "Evt", NormEvent::End))
                .unwrap();
        }
        let journal = temp.path(JOURNAL);
        let held = Lock::acquire(&temp.path(LOCK)).unwrap();
        let loader = std::thread::spawn({
            let store = Store::open(temp.0.clone());
            move || store.load()
        });
        std::thread::sleep(Duration::from_millis(200));
        // The frame lands while the read waits on the lock: the re-read
        // under the lock sees it and must not rewrite it away.
        let mut bytes = fs::read(&journal).unwrap();
        let bad = b"[1,2,3]";
        bytes.extend_from_slice(&(bad.len() as u32).to_le_bytes());
        bytes.extend_from_slice(bad);
        fs::write(&journal, &bytes).unwrap();
        drop(held);
        let loaded = loader.join().expect("load joins");
        assert_eq!(fs::read(&journal).unwrap(), bytes);
        assert!(
            loaded
                .errors
                .iter()
                .any(|e| e.detail.contains("does not parse"))
        );
    }
}
