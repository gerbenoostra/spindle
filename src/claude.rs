//! The Claude Code plugin: inventory of live session files and durable
//! transcripts, parsed defensively into normalized conversations.
//!
//! Live state comes from `<root>/sessions/<pid>.json` - one file per running
//! session, rewritten on status change, and cleaned up by Claude itself, so
//! no reaper is needed here. History comes from
//! `<root>/projects/<slug>/<uuid>.jsonl` transcripts, which survive worktree
//! deletion; every safe UUID-named regular non-empty file is indexed, with no
//! age cutoff. A live file and a transcript naming the same `sessionId` form
//! one conversation.
//!
//! Parsing follows the provider's observed drift: unknown keys are ignored,
//! a missing field makes that field unknown rather than guessed, and one
//! malformed record is excluded with a source error retained - it never fails
//! the scan. Incremental scans revisit only transcripts whose identity
//! (device, inode), size or mtime changed, and transcripts are append-only:
//! a file that grew re-parses only the tail past the last newline consumed.
//!
//! The plugin only reads. It never writes anything under the provider's
//! directories.

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::process::{self, ProcessStart};
use crate::provider::{
    Capabilities, HookDispatch, InventorySource, OwnershipSource, ProcessBindingSource,
    PublishedState, PublishedStatus, ResumeCommand, SourceError, StateEvidence, StateSource,
};
use crate::runtime::{AgentSessionKey, EvidenceSource, ProcessClaim, Provider};

/// The basename Claude's own executable runs under; also the liveness check's
/// guard against pid reuse.
const EXE: &str = "claude";

/// A Claude plugin rooted at its config directory (`~/.claude`), holding the
/// transcript index across scans so only changed files are reparsed.
pub struct Claude {
    root: PathBuf,
    /// Path -> last-parsed identity. Entries persist across scans so an
    /// unchanged transcript is never re-read; a file that vanished from the
    /// projects tree is dropped with its record.
    index: HashMap<PathBuf, Indexed>,
}

/// One conversation's merged view of its live file and transcript evidence.
/// Either side may be absent: live-only is a session with no transcript yet,
/// transcript-only is durable history.
#[derive(Debug)]
pub struct Conversation {
    /// Claude's own `sessionId`, the durable conversation identity.
    pub session_id: String,
    pub live: Option<Live>,
    /// Shared with the plugin's index, so an unchanged transcript costs a
    /// scan nothing but a refcount.
    pub transcript: Option<Arc<Transcript>>,
}

/// A parsed live session file.
#[derive(Debug)]
pub struct Live {
    pub file: PathBuf,
    pub pid: u32,
    /// `procStart`, the provider's UTC ctime, normalized to epoch - the pair
    /// `(pid, pid_start)` is the liveness authority. `Unavailable` when the
    /// provider does not date its process.
    pub pid_start: ProcessStart,
    pub cwd: Option<PathBuf>,
    /// The published tmux handle (`session:@window.%pane`).
    pub tmux: Option<String>,
    /// The provider's raw `status` string; `None` when the key is absent.
    pub status_raw: Option<String>,
    pub status: Option<PublishedStatus>,
    /// `waitingFor`, carried verbatim; only meaningful while waiting.
    pub waiting_for: Option<String>,
    /// `updatedAt` / `statusUpdatedAt`, millisecond epochs.
    pub updated_at: Option<SystemTime>,
    pub status_updated_at: Option<SystemTime>,
    /// The session's display name, when Claude named it.
    pub name: Option<String>,
}

/// A parsed transcript plus the metadata that keeps it incremental.
#[derive(Debug, Clone)]
pub struct Transcript {
    pub file: PathBuf,
    /// The `projects/<slug>` component the file sits under; evidence of the
    /// project the path was spelled from, not identity.
    pub slug: String,
    /// Claude's `sessionId` as the records carry it; the merge key that ties
    /// this transcript to a live session of the same conversation.
    pub session_id: String,
    /// The project path as the records' `cwd` field reports it.
    pub project_cwd: Option<PathBuf>,
    /// The `summary` record's title, when present.
    pub summary: Option<String>,
    /// The latest user text and the latest assistant text, verbatim.
    pub latest_prompt: Option<String>,
    pub latest_reply: Option<String>,
    /// Oldest and newest record timestamps seen.
    pub first_at: Option<SystemTime>,
    pub last_at: Option<SystemTime>,
    /// Lines that did not parse, retained for the evidence view.
    pub malformed_lines: usize,
}

/// One scan's output: the merged conversations plus the failures and skips
/// the evidence view keeps.
#[derive(Debug, Default)]
pub struct Inventory {
    pub conversations: Vec<Conversation>,
    /// Malformed files, unreadable entries, records without a session id -
    /// each excluded and retained, none of them fatal.
    pub errors: Vec<SourceError>,
    /// Entries rejected by the safety rules (non-UUID names, symlinks,
    /// non-regular or empty files). Not errors: they were never transcripts.
    pub skipped: Vec<PathBuf>,
}

/// What a transcript file was when last parsed: device, inode, length and
/// mtime. Equal means unchanged; same device+inode with a greater length
/// means appended; anything else means reparse whole.
#[derive(Debug, PartialEq, Eq)]
struct FileIdentity {
    dev: u64,
    ino: u64,
    len: u64,
    mtime: Option<SystemTime>,
}

/// The transcript index entry: what the file was when last parsed, how far
/// into it the parse reached, and the record it parsed to. `Err` is retained
/// too, so a malformed file is not reparsed - and not re-reported - on every
/// scan.
struct Indexed {
    /// The file's identity at the last parse; `None` before it.
    identity: Option<FileIdentity>,
    /// The byte offset just past the last complete line consumed. A partial
    /// tail is a write in flight - left for the next pass, not counted
    /// malformed.
    consumed: u64,
    /// The record the scan emits, shared unchanged until the file changes.
    record: Result<Arc<Transcript>, String>,
    /// `sessionId`s seen that disagree with the record's: the conflict
    /// check's running state, kept across incremental parses.
    other_ids: HashSet<String>,
}

impl Indexed {
    /// An entry for a file never parsed; the first sighting reparses whole.
    fn blank(path: &Path, slug: &str) -> Indexed {
        Indexed {
            identity: None,
            consumed: 0,
            record: Ok(Arc::new(Transcript::blank(path, slug))),
            other_ids: HashSet::new(),
        }
    }
}

impl Conversation {
    /// The conversation key used across the snapshot.
    pub fn key(&self) -> AgentSessionKey {
        AgentSessionKey {
            provider: Provider::Claude,
            session_id: self.session_id.clone(),
        }
    }

    /// The display title: the live name wins, then the transcript summary,
    /// then a truncation of the latest user prompt. `None` renders `?`.
    pub fn title(&self) -> Option<String> {
        if let Some(name) = self.live.as_ref().and_then(|l| l.name.as_ref()) {
            return Some(name.clone());
        }
        if let Some(summary) = self.transcript.as_ref().and_then(|t| t.summary.as_ref()) {
            return Some(summary.clone());
        }
        self.transcript
            .as_ref()
            .and_then(|t| t.latest_prompt.as_ref())
            .map(|p| truncate(&one_line(p), 40))
    }

    /// The path this conversation's Work identity derives from: the live
    /// cwd wins (it reflects where the session was started), then the
    /// transcript's project cwd. Either may be stale if the tree moved -
    /// still the best source-backed anchor a transcript leaves.
    pub fn cwd(&self) -> Option<&Path> {
        self.live
            .as_ref()
            .and_then(|l| l.cwd.as_deref())
            .or_else(|| {
                self.transcript
                    .as_ref()
                    .and_then(|t| t.project_cwd.as_deref())
            })
    }

    /// The normalized state observation for the core's arbitration: the
    /// published live state while live evidence exists, otherwise absent -
    /// a transcript is history, not a state claim.
    pub fn state(&self) -> StateEvidence {
        match &self.live {
            Some(live) => StateEvidence::Published(PublishedState {
                status: live.status,
                raw: live.status_raw.clone(),
                waiting_for: live.waiting_for.clone(),
            }),
            None => StateEvidence::Absent,
        }
    }

    /// The time the current effective state began - `statusUpdatedAt` while
    /// live, else the transcript's last record.
    pub fn state_since(&self) -> Option<SystemTime> {
        self.live
            .as_ref()
            .and_then(|l| l.status_updated_at.or(l.updated_at))
            .or_else(|| self.transcript.as_ref().and_then(|t| t.last_at))
    }

    /// The most recent evidence of the conversation at all.
    pub fn last_activity(&self) -> Option<SystemTime> {
        self.live
            .as_ref()
            .and_then(|l| l.updated_at.or(l.status_updated_at))
            .or_else(|| self.transcript.as_ref().and_then(|t| t.last_at))
    }

    /// The process claim a live session makes, for runtime resolution.
    pub fn claim(&self, observed_at: SystemTime) -> Option<ProcessClaim> {
        self.live.as_ref().map(|live| ProcessClaim {
            session: Some(self.key()),
            pid: live.pid,
            pid_start: live.pid_start,
            expected_exe: Some(EXE.to_owned()),
            published_pane: live.tmux.clone(),
            source: EvidenceSource::Published,
            observed_at: live.updated_at.unwrap_or(observed_at),
        })
    }

    /// Claude's verified resume argv: `claude --resume <session_id>` as an
    /// executable plus argument vector - the session id is one argument.
    pub fn resume_argv(&self) -> Vec<OsString> {
        let command = Claude::capabilities();
        let Some(resume) = command.resume else {
            return Vec::new(); // coverage: off - Claude always has resume
        };
        let mut argv = vec![resume.executable];
        argv.extend(resume.argv);
        argv.push(OsString::from(&self.session_id));
        argv
    }
}

impl Claude {
    /// A plugin rooted at `root` (the `~/.claude` directory shape).
    pub fn new(root: PathBuf) -> Claude {
        Claude {
            root,
            index: HashMap::new(),
        }
    }

    /// Claude's declared capabilities: what it proves and, just as much, what
    /// it does not.
    pub fn capabilities() -> Capabilities {
        Capabilities {
            inventory: InventorySource::LiveFilesAndTranscripts,
            ownership: OwnershipSource::PublishedProcess,
            process_binding: ProcessBindingSource::PublishedHandle,
            state: StateSource::Published,
            // The branch is derived through cwd, not read from a field.
            branch: false,
            dispatch: HookDispatch::PerEvent,
            // `SessionEnd` reports teardown; `Stop` is the turn boundary.
            session_end_is_turn_boundary: false,
            resume: Some(ResumeCommand {
                executable: OsString::from(EXE),
                argv: vec![OsString::from("--resume")],
            }),
        }
    }

    /// One full inventory pass: live session files, then transcripts (only
    /// re-reading what changed), merged on `sessionId`.
    pub fn scan(&mut self) -> Inventory {
        let mut inventory = Inventory::default();
        let live = self.scan_sessions(&mut inventory.errors);
        let transcripts = self.scan_transcripts(&mut inventory.errors, &mut inventory.skipped);

        let mut by_id: HashMap<String, Conversation> = HashMap::new();
        for transcript in transcripts {
            let session_id = transcript.session_id.clone();
            by_id
                .entry(session_id.clone())
                .or_insert_with(|| Conversation {
                    session_id,
                    live: None,
                    transcript: None,
                })
                .transcript = Some(transcript);
        }
        for record in live {
            by_id
                .entry(record.session_id.clone())
                .or_insert_with(|| Conversation {
                    session_id: record.session_id.clone(),
                    live: None,
                    transcript: None,
                })
                .live = Some(record.live);
        }
        inventory.conversations = by_id.into_values().collect();
        inventory
            .conversations
            .sort_by(|a, b| a.session_id.cmp(&b.session_id));
        inventory
    }

    /// Every live session file `<root>/sessions/<pid>.json`. Entries that are
    /// not pid-named regular files are skipped silently; malformed records
    /// and pid/name mismatches are errors, each excluded alone.
    fn scan_sessions(&mut self, errors: &mut Vec<SourceError>) -> Vec<LiveRecord> {
        let dir = self.root.join("sessions");
        let mut found = Vec::new();
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            // No sessions directory means no live sessions - a machine that
            // never ran Claude reads as empty, not as a failure.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return found,
            Err(e) => {
                errors.push(SourceError {
                    source: "claude sessions".to_owned(),
                    detail: format!("{}: {e}", dir.display()),
                });
                return found;
            }
        };
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(e) /* // coverage: off - a mid-iteration entry failure needs a racing mutation */ => {
                    let detail = format!("{}: {e}", dir.display()); // coverage: off - same
                    errors.push(SourceError { source: "claude sessions".to_owned(), detail }); // coverage: off - same
                    continue; // coverage: off - same
                }
            };
            let path = entry.path();
            let Some(pid) = pid_name(&path) else {
                continue;
            };
            if !entry.file_type().is_ok_and(|t| t.is_file()) {
                continue;
            }
            match parse_live(&path) {
                Ok(record) if record.live.pid != pid => errors.push(SourceError {
                    source: "claude session".to_owned(),
                    detail: format!(
                        "{}: file names pid {pid} but records pid {}",
                        path.display(),
                        record.live.pid
                    ),
                }),
                Ok(record) => found.push(record),
                Err(detail) => errors.push(SourceError {
                    source: "claude session".to_owned(),
                    detail,
                }),
            }
        }
        found
    }

    /// Every transcript under `<root>/projects/`, reparsing only files whose
    /// identity, size or mtime changed since the index last saw them. A file
    /// that only grew re-reads from the last consumed offset - transcripts
    /// are append-only, so an active session's transcript costs its tail,
    /// not its whole history, per pass.
    fn scan_transcripts(
        &mut self,
        errors: &mut Vec<SourceError>,
        skipped: &mut Vec<PathBuf>,
    ) -> Vec<Arc<Transcript>> {
        let dir = self.root.join("projects");
        let mut candidates = Vec::new();
        match fs::read_dir(&dir) {
            Ok(_) => collect_transcripts(&dir, &mut candidates, skipped, errors),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => errors.push(SourceError {
                source: "claude transcripts".to_owned(),
                detail: format!("{}: {e}", dir.display()),
            }),
        }

        // Anything the walk no longer sees drops out of the index with it.
        let seen: HashSet<&Path> = candidates.iter().map(|(p, _, _)| p.as_path()).collect();
        self.index.retain(|path, _| seen.contains(path.as_path()));

        let mut found = Vec::new();
        for (path, slug, meta) in candidates {
            let identity = FileIdentity {
                dev: meta.dev(),
                ino: meta.ino(),
                len: meta.len(),
                mtime: meta.modified().ok(),
            };
            let indexed = self
                .index
                .entry(path.clone())
                .or_insert_with(|| Indexed::blank(&path, &slug));
            if indexed.identity.as_ref() == Some(&identity) {
                // Unchanged: the parsed record stands, shared not rebuilt.
                if let Ok(record) = &indexed.record {
                    found.push(Arc::clone(record));
                }
                continue;
            }
            // The same file grown past the consumed offset is an append:
            // read only what arrived after it. A new inode, a shrink or a
            // remembered failure parses whole. An in-place rewrite that
            // grows the same inode is indistinguishable from an append on
            // (dev, ino, len) alone - transcripts are append-only, so that
            // is the contract, not a case to detect.
            let appended = indexed
                .identity
                .as_ref()
                .is_some_and(|i| i.dev == identity.dev && i.ino == identity.ino)
                && identity.len > indexed.consumed
                && indexed.record.is_ok();
            let parsed = if appended {
                let mut record = indexed.record.as_ref().unwrap().as_ref().clone();
                read_transcript(&path, indexed.consumed, &mut record, &mut indexed.other_ids)
                    .and_then(|consumed| {
                        validate(&path, &slug, &record, &indexed.other_ids)
                            .map(|()| (consumed, record))
                    })
            } else {
                let mut record = Transcript::blank(&path, &slug);
                indexed.other_ids.clear();
                read_transcript(&path, 0, &mut record, &mut indexed.other_ids).and_then(
                    |consumed| {
                        validate(&path, &slug, &record, &indexed.other_ids)
                            .map(|()| (consumed, record))
                    },
                )
            };
            match parsed {
                Ok((consumed, record)) => {
                    indexed.consumed = consumed;
                    indexed.record = Ok(Arc::new(record));
                }
                Err(detail) => {
                    errors.push(SourceError {
                        source: "claude transcript".to_owned(),
                        detail: detail.clone(),
                    });
                    indexed.record = Err(detail);
                }
            }
            indexed.identity = Some(identity);
            if let Ok(record) = &indexed.record {
                found.push(Arc::clone(record));
            }
        }
        found
    }
}

/// The pid a session file's name carries: `<pid>.json`, digits only.
fn pid_name(path: &Path) -> Option<u32> {
    if path.extension().and_then(|e| e.to_str()) != Some("json") {
        return None;
    }
    path.file_stem()?.to_str()?.parse().ok() // coverage: off - a non-UTF-8 stem cannot be created on APFS
}

/// Recursively collect transcript candidates under `dir`. The rules are the
/// provider's promise: a transcript is a UUID-named regular non-empty file.
/// Anything else is skipped, never parsed; an unreadable directory or entry
/// is a retained error, consistent with the rest of the scan.
fn collect_transcripts(
    dir: &Path,
    out: &mut Vec<(PathBuf, String, fs::Metadata)>,
    skipped: &mut Vec<PathBuf>,
    errors: &mut Vec<SourceError>,
) {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) => {
            errors.push(SourceError {
                source: "claude transcripts".to_owned(),
                detail: format!("{}: {e}", dir.display()),
            });
            return;
        }
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) /* // coverage: off - a mid-iteration entry failure needs a racing mutation */ => {
                let detail = format!("{}: {e}", dir.display()); // coverage: off - same
                errors.push(SourceError { source: "claude transcripts".to_owned(), detail }); // coverage: off - same
                continue; // coverage: off - same
            }
        };
        let path = entry.path();
        let meta = match entry.metadata() {
            Ok(meta) => meta,
            Err(e) /* // coverage: off - a stat failure needs a racing delete */ => {
                let detail = format!("{}: {e}", path.display()); // coverage: off - same
                errors.push(SourceError { source: "claude transcripts".to_owned(), detail }); // coverage: off - same
                continue; // coverage: off - same
            }
        };
        if meta.is_dir() {
            collect_transcripts(&path, out, skipped, errors);
            continue;
        }
        if !meta.is_file() || meta.len() == 0 || !uuid_name(&path) {
            skipped.push(path);
            continue;
        }
        let slug = path
            .parent()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        out.push((path, slug, meta));
    }
}

/// Whether the name is `<uuid>.jsonl` - 8-4-4-4-12 lowercase hex, the shape
/// Claude writes. Uppercase is rejected too: the check is for files this
/// provider would have written, not merely uuid-ish ones.
fn uuid_name(path: &Path) -> bool {
    if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
        return false;
    }
    let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
        return false; // coverage: off - non-UTF8 names fail to_str above anyway
    };
    let groups: Vec<&str> = stem.split('-').collect();
    groups.len() == 5
        && [8, 4, 4, 4, 12].iter().zip(&groups).all(|(len, g)| {
            g.len() == *len
                && g.bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        })
}

/// A live file's contents, defensively parsed. Unknown keys are ignored;
/// required identity is `pid` + `sessionId`. Numeric times are millisecond
/// epochs; `procStart` is the provider's UTC ctime.
struct LiveRecord {
    session_id: String,
    live: Live,
}

fn parse_live(path: &Path) -> Result<LiveRecord, String> {
    let text = fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let value: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("{}: not JSON ({e})", path.display()))?;
    let Some(session_id) = value.get("sessionId").and_then(|v| v.as_str()) else {
        return Err(format!("{}: no sessionId", path.display()));
    };
    let Some(pid) = value.get("pid").and_then(|v| v.as_u64()) else {
        return Err(format!("{}: no pid", path.display()));
    };
    let Some(pid) = u32::try_from(pid).ok() else {
        return Err(format!("{}: pid out of range", path.display()));
    };
    let status_raw = value
        .get("status")
        .and_then(|v| v.as_str())
        .map(str::to_owned);
    let status = status_raw.as_deref().and_then(|s| match s {
        "busy" => Some(PublishedStatus::Busy),
        "idle" => Some(PublishedStatus::Idle),
        "waiting" => Some(PublishedStatus::Waiting),
        _ => None,
    });
    Ok(LiveRecord {
        session_id: session_id.to_owned(),
        live: Live {
            file: path.to_owned(),
            pid,
            pid_start: value
                .get("procStart")
                .and_then(|v| v.as_str())
                .and_then(process::parse_utc_ctime)
                .map(ProcessStart::At)
                .unwrap_or(ProcessStart::Unavailable),
            cwd: value.get("cwd").and_then(|v| v.as_str()).map(PathBuf::from),
            tmux: value
                .get("tmux")
                .and_then(|v| v.as_str())
                .map(str::to_owned),
            waiting_for: value
                .get("waitingFor")
                .and_then(|v| v.as_str())
                .map(str::to_owned),
            status_raw,
            status,
            updated_at: millis(value.get("updatedAt")),
            status_updated_at: millis(value.get("statusUpdatedAt")),
            name: value
                .get("name")
                .and_then(|v| v.as_str())
                .map(str::to_owned),
        },
    })
}

/// A millisecond-epoch JSON number as a `SystemTime`.
fn millis(value: Option<&serde_json::Value>) -> Option<SystemTime> {
    value
        .and_then(|v| v.as_u64())
        .map(|ms| UNIX_EPOCH + Duration::from_millis(ms))
}

impl Transcript {
    /// An empty record for `file` before its first line lands.
    fn blank(file: &Path, slug: &str) -> Transcript {
        Transcript {
            file: file.to_owned(),
            slug: slug.to_owned(),
            session_id: String::new(),
            project_cwd: None,
            summary: None,
            latest_prompt: None,
            latest_reply: None,
            first_at: None,
            last_at: None,
            malformed_lines: 0,
        }
    }
}

/// Read `path` from `offset` and absorb its complete lines into `record`,
/// returning the new consumed offset - just past the last newline. A
/// trailing partial line is a write still in flight: left unread until it
/// completes, and never counted malformed.
fn read_transcript(
    path: &Path,
    offset: u64,
    record: &mut Transcript,
    other_ids: &mut HashSet<String>,
) -> Result<u64, String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if offset > 0 {
        file.seek(SeekFrom::Start(offset))
            .map_err(|e| format!("{}: {e}", path.display()))?; // coverage: off - a seek fails only on a racing truncation
    }
    let mut text = String::new();
    file.read_to_string(&mut text)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(offset + absorb(&text, record, other_ids) as u64)
}

/// Fold each complete line of `text` into `record`, returning the bytes
/// consumed - everything through the last newline. Each line is one record;
/// a line that fails to parse is counted, not fatal.
fn absorb(text: &str, record: &mut Transcript, other_ids: &mut HashSet<String>) -> usize {
    let mut consumed = 0;
    for piece in text.split_inclusive('\n') {
        let Some(line) = piece.strip_suffix('\n') else {
            break;
        };
        consumed += piece.len();
        if line.trim().is_empty() {
            continue;
        }
        let Ok(json) = serde_json::from_str::<serde_json::Value>(line) else {
            record.malformed_lines += 1;
            continue;
        };
        if let Some(id) = json.get("sessionId").and_then(|v| v.as_str()) {
            if record.session_id.is_empty() {
                record.session_id = id.to_owned();
            } else if record.session_id != id {
                other_ids.insert(id.to_owned());
            }
        }
        if record.project_cwd.is_none()
            && let Some(cwd) = json.get("cwd").and_then(|v| v.as_str())
        {
            record.project_cwd = Some(PathBuf::from(cwd));
        }
        if record.summary.is_none() && json.get("type").and_then(|v| v.as_str()) == Some("summary")
        {
            record.summary = json
                .get("summary")
                .and_then(|v| v.as_str())
                .map(str::to_owned);
        }
        if let Some(ts) = json
            .get("timestamp")
            .and_then(|v| v.as_str())
            .and_then(parse_iso8601)
        {
            record.first_at = Some(record.first_at.map_or(ts, |f| f.min(ts)));
            record.last_at = Some(record.last_at.map_or(ts, |l| l.max(ts)));
        }
        match json.get("type").and_then(|v| v.as_str()) {
            Some("user") => {
                if let Some(text) = message_text(&json, "user") {
                    record.latest_prompt = Some(text);
                }
            }
            Some("assistant") => {
                if let Some(text) = message_text(&json, "assistant") {
                    record.latest_reply = Some(text);
                }
            }
            _ => {}
        }
    }
    consumed
}

/// The whole-file verdicts a scan cannot give until the lines are read: a
/// transcript that never names a session, or names several, is excluded
/// wholesale.
fn validate(
    path: &Path,
    slug: &str,
    record: &Transcript,
    other_ids: &HashSet<String>,
) -> Result<(), String> {
    if record.session_id.is_empty() {
        return Err(format!("{}: no sessionId in any record", path.display()));
    }
    if !other_ids.is_empty() {
        return Err(format!(
            "{slug}: {} carries {} conflicting session ids",
            path.display(),
            other_ids.len() + 1
        ));
    }
    Ok(())
}

/// The text a record's `message.content` carries: a bare string, or the last
/// `text` block of a block array. `tool_result`/`tool_use` blocks carry no
/// visible text and yield `None`, so a tool round-trip is never mistaken for
/// a prompt or a reply.
fn message_text(record: &serde_json::Value, role: &str) -> Option<String> {
    let message = record.get("message")?;
    if message.get("role").and_then(|v| v.as_str()) != Some(role) {
        return None;
    }
    match message.get("content")? {
        serde_json::Value::String(text) => Some(text.clone()),
        serde_json::Value::Array(blocks) => blocks
            .iter()
            .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
            .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
            .next_back()
            .map(str::to_owned),
        _ => None,
    }
}

/// `2026-09-22T16:18:53.123Z` (or a `+02:00` offset) as a `SystemTime`.
/// Provider timestamps are ISO-8601; anything else is no timestamp, not a
/// guessed one.
fn parse_iso8601(text: &str) -> Option<SystemTime> {
    let (date, time) = text.split_once('T').or_else(|| text.split_once(' '))?;
    let mut d = date.split('-');
    let (year, month, day) = (
        d.next()?.parse::<i64>().ok()?, // coverage: off - the first split piece always exists
        d.next()?.parse::<i64>().ok()?,
        d.next()?.parse::<u64>().ok()?,
    );
    if d.next().is_some() || !(1..=12).contains(&month) || day == 0 || day > 31 {
        return None;
    }
    let (hms, offset_secs) = match time.split_once('Z') {
        Some((hms, "")) => (hms, 0i64),
        _ => {
            let (hms, sign, offset) = match time.split_once('+') {
                Some((hms, off)) => (hms, 1i64, off),
                None => {
                    let (hms, off) = time.split_once('-').filter(|(h, _)| !h.is_empty())?;
                    (hms, -1i64, off)
                }
            };
            let mut o = offset.split(':');
            let (oh, om) = (
                o.next()?.parse::<i64>().ok()?, // coverage: off - the first split piece always exists
                o.next()?.parse::<i64>().ok()?,
            );
            if o.next().is_some() || oh > 23 || om > 59 {
                return None;
            }
            (hms, sign * (oh * 3600 + om * 60))
        }
    };
    let hms = hms.split('.').next()?; // coverage: off - a split's first piece always exists
    let mut t = hms.split(':');
    let (hour, min, sec) = (
        t.next()?.parse::<i64>().ok()?, // coverage: off - the first split piece always exists
        t.next()?.parse::<i64>().ok()?,
        t.next()?.parse::<i64>().ok()?,
    );
    if t.next().is_some() || hour > 23 || min > 59 || sec > 60 {
        return None;
    }
    let epoch = process::days_from_civil(year, month, day) * 86_400 + hour * 3600 + min * 60 + sec
        - offset_secs;
    Some(UNIX_EPOCH + Duration::from_secs(u64::try_from(epoch).ok()?))
}

/// One line of `text`, for titles derived from prompts.
fn one_line(text: &str) -> String {
    text.lines().next().unwrap_or_default().trim().to_owned()
}

/// `text` cut to at most `max` chars on a char boundary, `…`-suffixed when
/// it was cut.
fn truncate(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        None => text.to_owned(),
        Some((end, _)) => format!("{}…", &text[..end]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    /// A throwaway `<root>` shaped like `~/.claude`.
    struct Root(PathBuf);
    impl Root {
        fn new() -> Root {
            static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "agent-sessions-claude-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            Root(path)
        }
        fn write(&self, rel: &str, text: &str) -> PathBuf {
            let path = self.0.join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, text).unwrap();
            path
        }
    }
    impl Drop for Root {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    const ID_A: &str = "11111111-2222-3333-4444-555555555555";
    const ID_B: &str = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";

    fn live_json(pid: u32, id: &str, status: &str) -> String {
        format!(
            r#"{{"pid":{pid},"sessionId":"{id}","cwd":"/work/repo","tmux":"s:@1.%2","name":"{id}-name","status":"{status}","updatedAt":1788621019906,"statusUpdatedAt":1788621019906,"procStart":"Tue Sep 22 16:18:53 2026","version":"2.1.261","futureKey":{{"nested":[1]}}}}"#
        )
    }

    fn transcript_lines(id: &str) -> String {
        format!(
            concat!(
                r#"{{"type":"summary","summary":"fix the pane labels","leafUuid":"x"}}"#,
                "\n",
                r#"{{"type":"user","sessionId":"{id}","cwd":"/work/repo","gitBranch":"main","message":{{"role":"user","content":[{{"type":"text","text":"rename pane titles"}}]}},"uuid":"u1","timestamp":"2026-09-22T16:18:53.000Z"}}"#,
                "\n",
                r#"{{"type":"assistant","sessionId":"{id}","message":{{"role":"assistant","content":[{{"type":"tool_use","name":"Edit"}},{{"type":"text","text":"renamed three panes"}}]}},"uuid":"a1","timestamp":"2026-09-22T16:19:53.500Z"}}"#,
                "\n"
            ),
            id = id
        )
    }

    #[test]
    fn a_missing_store_invents_nothing() {
        let mut claude = Claude::new(PathBuf::from("/definitely/not/here"));
        let inv = claude.scan();
        assert!(inv.conversations.is_empty());
        assert!(inv.errors.is_empty());
        assert!(inv.skipped.is_empty());
    }

    #[test]
    fn live_files_parse_defensively() {
        let root = Root::new();
        root.write(
            "sessions/4200.json",
            &live_json(4200, ID_A, "waiting").replace(
                "\"status\":\"waiting\"",
                "\"status\":\"waiting\",\"waitingFor\":\"permission prompt\"",
            ),
        );
        // Unknown keys drift in without breaking anything (F1 shape).
        root.write("sessions/4300.json", &live_json(4300, ID_B, "busy"));
        // Malformed JSON and a missing sessionId are each one excluded record.
        root.write("sessions/4400.json", "{not json");
        root.write("sessions/4500.json", r#"{"pid":4500}"#);
        // A pid/file-name conflict cannot prove identity.
        root.write("sessions/4600.json", &live_json(4601, ID_B, "busy"));
        // Non-pid names and non-files are not session records at all.
        root.write("sessions/notes.txt", "x");
        root.write("sessions/abc.json", "{}");
        fs::create_dir_all(root.0.join("sessions/4700.json")).unwrap();

        let mut claude = Claude::new(root.0.clone());
        let inv = claude.scan();
        assert_eq!(inv.conversations.len(), 2, "{:?}", inv.errors);
        assert_eq!(inv.errors.len(), 3, "{:?}", inv.errors);

        let waiting = &inv.conversations[0];
        assert_eq!(waiting.session_id, ID_A);
        let live = waiting.live.as_ref().unwrap();
        assert_eq!(live.pid, 4200);
        assert_eq!(live.pid_start, ProcessStart::At(1_790_093_933));
        assert_eq!(live.status, Some(PublishedStatus::Waiting));
        assert_eq!(live.waiting_for.as_deref(), Some("permission prompt"));
        assert_eq!(live.tmux.as_deref(), Some("s:@1.%2"));
        assert_eq!(live.cwd.as_deref(), Some(Path::new("/work/repo")));
        assert_eq!(
            waiting.title().as_deref(),
            Some(&format!("{ID_A}-name")[..])
        );

        let busy = &inv.conversations[1];
        assert_eq!(
            busy.live.as_ref().unwrap().status,
            Some(PublishedStatus::Busy)
        );
        assert_eq!(busy.state(), busy_state());
    }

    fn busy_state() -> StateEvidence {
        StateEvidence::Published(PublishedState {
            status: Some(PublishedStatus::Busy),
            raw: Some("busy".to_owned()),
            waiting_for: None,
        })
    }

    #[test]
    fn an_unknown_status_stays_unknown_not_guessed() {
        let root = Root::new();
        root.write("sessions/1.json", &live_json(1, ID_A, "thinking"));
        let mut claude = Claude::new(root.0.clone());
        let inv = claude.scan();
        let live = inv.conversations[0].live.as_ref().unwrap();
        assert_eq!(live.status, None);
        assert_eq!(live.status_raw.as_deref(), Some("thinking"));
        match inv.conversations[0].state() {
            StateEvidence::Published(p) => {
                assert_eq!(p.status, None);
                assert_eq!(p.raw.as_deref(), Some("thinking"));
            }
            _ => panic!("published state keeps its raw value"), // coverage: off - failure path
        }
        // And a session file with no status key keeps the absence.
        root.write(
            "sessions/2.json",
            &live_json(2, ID_B, "x").replace(r#""status":"x","#, ""),
        );
        let inv = claude.scan();
        assert_eq!(inv.conversations.len(), 2);
        match inv.conversations[1].state() {
            StateEvidence::Published(p) => assert_eq!(p.raw, None),
            _ => panic!("a live record publishes state evidence"), // coverage: off - failure path
        }
    }

    #[test]
    fn transcripts_cover_every_safe_file_with_no_age_cutoff() {
        let root = Root::new();
        root.write(
            &format!("projects/proj-a/{ID_A}.jsonl"),
            &transcript_lines(ID_A),
        );
        root.write(
            &format!("projects/deep/nest/{ID_B}.jsonl"),
            &transcript_lines(ID_B),
        );
        // Unsafe names, wrong extensions, empty files and symlinks are all
        // skipped - never parsed, never errors.
        root.write("projects/proj-a/not-a-uuid.jsonl", &transcript_lines(ID_A));
        root.write(&format!("projects/proj-a/{}.json", ID_A), "x");
        root.write(
            &format!("projects/proj-a/{}.jsonl", ID_A.to_uppercase()),
            &transcript_lines(ID_A),
        );
        root.write(&format!("projects/proj-a/{}.jsonl", "empty"), "");
        let target = root.write(
            &format!("projects/proj-a/real-{}.jsonl", "target"),
            &transcript_lines(ID_A),
        );
        let _ = target;
        fs::create_dir_all(root.0.join("projects/proj-b")).unwrap();
        let link = root.0.join(format!("projects/proj-b/{ID_B}.jsonl"));
        symlink(root.0.join(format!("projects/proj-a/{ID_A}.jsonl")), &link).unwrap();

        let mut claude = Claude::new(root.0.clone());
        let inv = claude.scan();
        assert_eq!(inv.conversations.len(), 2, "{:?}", inv.errors);
        assert!(inv.errors.is_empty(), "{:?}", inv.errors);
        assert_eq!(inv.skipped.len(), 5, "{:?}", inv.skipped);

        let a = &inv.conversations[0];
        let t = a.transcript.as_ref().unwrap();
        assert_eq!(t.session_id, ID_A);
        assert_eq!(t.slug, "proj-a");
        assert_eq!(t.project_cwd.as_deref(), Some(Path::new("/work/repo")));
        assert_eq!(t.summary.as_deref(), Some("fix the pane labels"));
        assert_eq!(t.latest_prompt.as_deref(), Some("rename pane titles"));
        assert_eq!(t.latest_reply.as_deref(), Some("renamed three panes"));
        assert_eq!(t.first_at, parse_iso8601("2026-09-22T16:18:53.000Z"));
        assert_eq!(t.last_at, parse_iso8601("2026-09-22T16:19:53.500Z"));
        assert_eq!(t.malformed_lines, 0);
        // An ancient transcript is inventory too - there is no age cutoff.
        assert!(a.transcript.is_some());
        assert_eq!(a.title().as_deref(), Some("fix the pane labels"));
        assert_eq!(a.state(), StateEvidence::Absent);
    }

    #[test]
    fn malformed_transcript_lines_are_counted_not_fatal() {
        let root = Root::new();
        root.write(
            &format!("projects/p/{ID_A}.jsonl"),
            &format!(
                "{}\n{{truncated\n\n{}\n",
                transcript_lines(ID_A),
                r#"{"type":"user","sessionId":""#,
            ),
        );
        // A file with no sessionId at all, and a file with conflicting ids,
        // are each one excluded record with an error.
        root.write(
            &format!("projects/p/{ID_B}.jsonl"),
            r#"{"type":"user","cwd":"/x"}"#,
        );
        root.write(
            &format!(
                "projects/p/{}.jsonl",
                "cccccccc-1111-2222-3333-444444444444"
            ),
            &format!("{}\n{}", transcript_lines(ID_A), transcript_lines(ID_B)),
        );
        let mut claude = Claude::new(root.0.clone());
        let inv = claude.scan();
        assert_eq!(inv.conversations.len(), 1, "{:?}", inv.errors);
        assert_eq!(inv.errors.len(), 2, "{:?}", inv.errors);
        assert_eq!(
            inv.conversations[0]
                .transcript
                .as_ref()
                .unwrap()
                .malformed_lines,
            2
        );
    }

    #[test]
    fn live_and_transcript_merge_on_the_session_id() {
        let root = Root::new();
        root.write("sessions/4200.json", &live_json(4200, ID_A, "idle"));
        root.write(&format!("projects/p/{ID_A}.jsonl"), &transcript_lines(ID_A));
        root.write(&format!("projects/p/{ID_B}.jsonl"), &transcript_lines(ID_B));
        let mut claude = Claude::new(root.0.clone());
        let inv = claude.scan();
        assert_eq!(inv.conversations.len(), 2);
        let a = &inv.conversations[0];
        assert!(a.live.is_some() && a.transcript.is_some());
        let b = &inv.conversations[1];
        assert!(b.live.is_none() && b.transcript.is_some());
        assert_eq!(b.title().as_deref().map(|t| &t[..5]), Some("fix t"));
    }

    #[test]
    fn incremental_scans_reparse_only_changed_files() {
        let root = Root::new();
        let path = root.write(&format!("projects/p/{ID_A}.jsonl"), &transcript_lines(ID_A));
        root.write(&format!("projects/p/{ID_B}.jsonl"), &transcript_lines(ID_B));
        let mut claude = Claude::new(root.0.clone());
        let first = claude.scan();
        assert_eq!(first.conversations.len(), 2);

        // Second scan, nothing changed: same result, and the index kept both.
        assert_eq!(claude.index.len(), 2);
        let second = claude.scan();
        assert_eq!(second.conversations.len(), 2);

        // Appending records reparses only that file - the new reply shows up
        // while the untouched transcript stays indexed.
        let reply = format!(
            "{{\"type\":\"assistant\",\"sessionId\":\"{ID_A}\",\"message\":{{\"role\":\"assistant\",\"content\":\"done now\"}},\"timestamp\":\"2026-09-22T16:20:00Z\"}}"
        );
        fs::write(&path, format!("{}{reply}\n", transcript_lines(ID_A))).unwrap();
        let third = claude.scan();
        assert_eq!(
            third.conversations[0]
                .transcript
                .as_ref()
                .unwrap()
                .latest_reply
                .as_deref(),
            Some("done now")
        );

        // Deleting a file drops its index entry and its conversation.
        fs::remove_file(root.0.join(format!("projects/p/{ID_B}.jsonl"))).unwrap();
        let fourth = claude.scan();
        assert_eq!(fourth.conversations.len(), 1);
        assert_eq!(claude.index.len(), 1);
    }

    #[test]
    fn a_partial_tail_waits_for_its_newline() {
        let root = Root::new();
        let path = root.write(&format!("projects/p/{ID_A}.jsonl"), &transcript_lines(ID_A));
        // A torn write: the half-written last record is in flight - not
        // parsed, not counted malformed.
        let partial = r#"{"type":"user","sessionId":""#;
        fs::write(&path, format!("{}{partial}", transcript_lines(ID_A))).unwrap();
        let mut claude = Claude::new(root.0.clone());
        let first = claude.scan();
        let t = first.conversations[0].transcript.as_ref().unwrap();
        assert_eq!(t.malformed_lines, 0);
        assert_eq!(t.last_at, parse_iso8601("2026-09-22T16:19:53.500Z"));

        // The rest of the record lands on the next append, and the
        // completed line parses then - from the tail, not a full reparse.
        fs::write(
            &path,
            format!(
                "{}{}{}{}\n",
                transcript_lines(ID_A),
                partial,
                ID_A,
                r#"","timestamp":"2026-09-22T16:25:00Z"}"#
            ),
        )
        .unwrap();
        let second = claude.scan();
        let t = second.conversations[0].transcript.as_ref().unwrap();
        assert_eq!(t.malformed_lines, 0);
        assert_eq!(t.last_at, parse_iso8601("2026-09-22T16:25:00Z"));
    }

    #[test]
    fn a_transcript_that_is_not_utf8_is_one_error() {
        let root = Root::new();
        let path = root.0.join(format!("projects/p/{ID_A}.jsonl"));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, [0x7b, 0x80]).unwrap();
        let mut claude = Claude::new(root.0.clone());
        let inv = claude.scan();
        assert!(inv.conversations.is_empty());
        assert_eq!(inv.errors.len(), 1, "{:?}", inv.errors);
    }

    #[test]
    fn a_reparse_failure_is_remembered_not_spammed() {
        let root = Root::new();
        root.write(
            &format!("projects/p/{ID_A}.jsonl"),
            r#"{"type":"user","cwd":"/x"}"#,
        );
        let mut claude = Claude::new(root.0.clone());
        let first = claude.scan();
        assert_eq!(first.errors.len(), 1);
        // Unchanged: the index remembers the failure rather than reporting it again.
        let second = claude.scan();
        assert!(second.errors.is_empty());
        assert_eq!(claude.index.len(), 1);
    }

    #[test]
    fn iso8601_parses_offsets_and_fractional_seconds() {
        let z = parse_iso8601("2026-09-22T16:18:53.123Z").unwrap();
        assert_eq!(
            z.duration_since(UNIX_EPOCH).unwrap().as_secs(),
            1_790_093_933
        );
        assert_eq!(parse_iso8601("2026-09-22T18:18:53+02:00").unwrap(), z);
        assert_eq!(parse_iso8601("2026-09-22T14:18:53-02:00").unwrap(), z);
        for bad in [
            "",
            "2026-09-22",
            "T16:18:53Z",
            "2026-13-01T00:00:00Z",
            "2026-09-00T00:00:00Z",
            "2026-09-22T25:00:00Z",
            "2026-09-22T16:18:53+25:00",
            "2026-09-22 16:18:53:01Z",
            "garbage",
        ] {
            assert!(parse_iso8601(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn titles_and_prompts_pick_the_best_source_backed_value() {
        let root = Root::new();
        // No summary and no name: the first line of the latest prompt titles
        // the row; multi-line prompts collapse to their first line.
        let mut line = format!(
            r#"{{"type":"user","sessionId":"{ID_A}","cwd":"/x","message":{{"role":"user","content":"first line\nsecond line which is quite long and keeps going past forty characters"}},"timestamp":"2026-01-01T00:00:00Z"}}"#
        );
        line.push('\n');
        root.write(&format!("projects/p/{ID_A}.jsonl"), &line);
        let mut claude = Claude::new(root.0.clone());
        let inv = claude.scan();
        assert_eq!(inv.conversations[0].title().as_deref(), Some("first line"));

        // No assistant record at all: no reply is reported, not a guess.
        assert_eq!(
            inv.conversations[0]
                .transcript
                .as_ref()
                .unwrap()
                .latest_reply,
            None
        );
    }

    #[test]
    fn claims_and_resume_are_capability_shaped() {
        let root = Root::new();
        root.write("sessions/99.json", &live_json(99, ID_A, "busy"));
        let mut claude = Claude::new(root.0.clone());
        let inv = claude.scan();
        let conv = &inv.conversations[0];
        let now = SystemTime::now();
        let claim = conv.claim(now).unwrap();
        assert_eq!(claim.pid, 99);
        assert_eq!(claim.pid_start, ProcessStart::At(1_790_093_933));
        assert_eq!(claim.expected_exe.as_deref(), Some("claude"));
        assert_eq!(claim.published_pane.as_deref(), Some("s:@1.%2"));
        assert_eq!(claim.source, EvidenceSource::Published);
        assert_eq!(claim.session.as_ref().unwrap().session_id, ID_A);

        let argv = conv.resume_argv();
        assert_eq!(argv[0], OsString::from("claude"));
        assert_eq!(argv[1], OsString::from("--resume"));
        assert_eq!(argv[2], OsString::from(ID_A));
    }

    #[test]
    fn a_transcript_only_conversation_has_no_claim() {
        let conv = Conversation {
            session_id: ID_A.to_owned(),
            live: None,
            transcript: None,
        };
        assert!(conv.claim(SystemTime::now()).is_none());
        assert_eq!(conv.state(), StateEvidence::Absent);
        assert_eq!(conv.title(), None);
        assert_eq!(conv.cwd(), None);
        assert_eq!(conv.state_since(), None);
        assert_eq!(conv.last_activity(), None);
    }

    #[test]
    fn capabilities_declare_what_claude_proves() {
        let caps = Claude::capabilities();
        assert_eq!(caps.inventory, InventorySource::LiveFilesAndTranscripts);
        assert_eq!(caps.ownership, OwnershipSource::PublishedProcess);
        assert_eq!(caps.process_binding, ProcessBindingSource::PublishedHandle);
        assert_eq!(caps.state, StateSource::Published);
        assert!(!caps.branch);
        assert_eq!(caps.dispatch, HookDispatch::PerEvent);
        assert!(!caps.session_end_is_turn_boundary);
        let resume = caps.resume.unwrap();
        assert_eq!(resume.executable, OsString::from("claude"));
        assert_eq!(resume.argv, vec![OsString::from("--resume")]);
    }

    #[test]
    fn an_unreadable_projects_subdir_is_one_error() {
        let root = Root::new();
        let locked = root.0.join("projects/locked");
        fs::create_dir_all(&locked).unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
        let mut claude = Claude::new(root.0.clone());
        let inv = claude.scan();
        assert!(
            inv.errors
                .iter()
                .any(|e| e.source == "claude transcripts" && e.detail.contains("locked")),
            "{:?}",
            inv.errors
        );
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn unreadable_store_dirs_report_errors_not_silence() {
        let root = Root::new();
        // `sessions` and `projects` that exist but are not directories read
        // as errors, kept in the inventory's error list.
        root.write("sessions", "not a dir");
        root.write("projects", "not a dir");
        let mut claude = Claude::new(root.0.clone());
        let inv = claude.scan();
        assert!(inv.conversations.is_empty());
        assert!(inv.errors.iter().any(|e| e.source == "claude sessions"));
        assert!(inv.errors.iter().any(|e| e.source == "claude transcripts"));
    }

    #[test]
    fn live_records_without_a_usable_pid_are_errors() {
        let root = Root::new();
        // No `pid` key, a pid that cannot be a process, and an unreadable
        // file: each is one retained error, none of them fatal.
        root.write("sessions/100.json", r#"{"sessionId":"x"}"#);
        root.write(
            "sessions/999.json",
            r#"{"pid":99999999999,"sessionId":"x"}"#,
        );
        let locked = root.write("sessions/200.json", r#"{"pid":200,"sessionId":"x"}"#);
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
        let mut claude = Claude::new(root.0.clone());
        let inv = claude.scan();
        assert!(inv.conversations.is_empty());
        assert_eq!(
            inv.errors
                .iter()
                .filter(|e| e.source == "claude session")
                .count(),
            3,
            "{:?}",
            inv.errors
        );
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o644)).unwrap();
    }

    #[test]
    fn an_unreadable_transcript_is_one_error() {
        let root = Root::new();
        let path = root.write(&format!("projects/-x/{ID_A}.jsonl"), "ok-ish\n");
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
        let mut claude = Claude::new(root.0.clone());
        let inv = claude.scan();
        assert!(inv.conversations.is_empty());
        assert!(
            inv.errors
                .iter()
                .any(|e| e.source == "claude transcript" && e.detail.contains(ID_A)),
            "{:?}",
            inv.errors
        );
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    }

    #[test]
    fn iso8601_rejects_every_malformed_shape() {
        for bad in [
            "not a date",
            "2026-01-01",
            "2026-01-01T",
            // each date component missing or non-numeric
            "x-01-01T00:00:00Z",
            "2026T00:00:00Z",
            "2026-x-01T00:00:00Z",
            "2026-01T00:00:00Z",
            "2026-01-xT00:00:00Z",
            "2026-01-01-4T00:00:00Z",
            "2026-13-01T00:00:00Z",
            "2026-01-32T00:00:00Z",
            // each time component missing, non-numeric or out of range
            "2026-01-01Tx:00:00Z",
            "2026-01-01T00:x:00Z",
            "2026-01-01T00:00:xZ",
            "2026-01-01T00Z",
            "2026-01-01T00:00Z",
            "2026-01-01T25:00:00Z",
            "2026-01-01T00:61:00Z",
            "2026-01-01T00:00:99Z",
            // offset shapes: missing, empty, non-numeric, extra part, bad
            // range, an empty hms before the offset
            "2026-01-01T00:00:00",
            "2026-01-01T-05:00",
            "2026-01-01T00:00:00+",
            "2026-01-01T00:00:00+05",
            "2026-01-01T00:00:00+aa:00",
            "2026-01-01T00:00:00+00:aa",
            "2026-01-01T00:00:00+0:0:0",
            "2026-01-01T00:00:00+99:99",
            "2026-01-01T00:00:00x",
            // before the epoch is no timestamp either
            "1969-12-31T00:00:00Z",
        ] {
            assert_eq!(parse_iso8601(bad), None, "{bad}");
        }
        // Fractional seconds, space separators and real offsets parse.
        assert!(parse_iso8601("2026-01-01T00:00:00.999Z").is_some());
        assert!(parse_iso8601("2026-01-01 00:00:00Z").is_some());
        assert!(parse_iso8601("2026-01-01T00:00:00+02:30").is_some());
        assert!(parse_iso8601("2026-01-01T02:30:00+02:30").is_some());
    }

    #[test]
    fn message_text_follows_only_the_blocks_it_owns() {
        // A record whose message has no `content`, or one that is not a
        // string or block array, yields no text - not a guess.
        for content in ["", r#""content":42"#, r#""content":[{"type":"tool_use"}]"#] {
            let line = format!(
                "{{\"type\":\"user\",\"sessionId\":\"{ID_A}\",\"message\":{{\"role\":\"user\"{extra}}}}}",
                extra = if content.is_empty() {
                    String::new()
                } else {
                    format!(",{content}")
                }
            );
            let record: serde_json::Value = serde_json::from_str(&line).unwrap();
            assert_eq!(message_text(&record, "user"), None, "{line}");
        }
        // A non-message record type contributes nothing.
        let record: serde_json::Value =
            serde_json::from_str(r#"{"type":"system","content":"x"}"#).unwrap();
        assert_eq!(message_text(&record, "user"), None);
        // A message whose role does not match the record's type is skipped.
        let record: serde_json::Value =
            serde_json::from_str(r#"{"type":"user","message":{"role":"assistant","content":"x"}}"#)
                .unwrap();
        assert_eq!(message_text(&record, "user"), None);
    }

    #[test]
    fn records_without_text_leave_prompt_and_reply_unknown() {
        let root = Root::new();
        // User and assistant records whose messages carry no text: the
        // transcript scans clean and reports neither a prompt nor a reply.
        root.write(
            &format!("projects/-x/{ID_A}.jsonl"),
            &format!(
                "{{\"type\":\"user\",\"sessionId\":\"{ID_A}\",\"message\":{{\"role\":\"user\"}}}}\n{{\"type\":\"assistant\",\"sessionId\":\"{ID_A}\",\"message\":{{\"role\":\"assistant\",\"content\":42}}}}\n"
            ),
        );
        let mut claude = Claude::new(root.0.clone());
        let inv = claude.scan();
        let t = &inv.conversations[0].transcript.as_ref().unwrap();
        assert_eq!(t.latest_prompt, None);
        assert_eq!(t.latest_reply, None);
    }

    #[test]
    fn titles_truncate_over_forty_chars() {
        assert_eq!(
            truncate("a much longer single-line title that keeps going", 40),
            "a much longer single-line title that kee…"
        );
        assert_eq!(truncate("short", 40), "short");
        assert_eq!(one_line("first\nsecond"), "first");
        assert_eq!(one_line(""), "");
    }
}
