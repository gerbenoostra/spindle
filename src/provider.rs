//! The provider capability contract: what a plugin can prove about its own
//! sessions, and the normalized evidence it hands the core.
//!
//! A plugin returns evidence, never final state. The core resolves
//! process, pane, cwd, worktree and branch identity, rejects stale or
//! inapplicable evidence, and derives the snapshot's effective values. A
//! capability a provider does not have is simply absent from its
//! [`Capabilities`]; it is never stubbed, and the snapshot renders the
//! corresponding cells as `Unknown` rather than as a plausible guess.

use std::ffi::OsString;

/// What a provider can deliver. Absent capabilities stay absent: a plugin
/// without a published state source emits records whose state is `Unknown`,
/// and a provider without a verified resume invocation carries `None` for
/// `resume` so the row reports that rather than guessing an argv.
#[derive(Debug)]
pub struct Capabilities {
    /// How session records are found.
    pub inventory: InventorySource,
    /// How a record proves which process owns it.
    pub ownership: OwnershipSource,
    /// How a live record binds to its pane.
    pub process_binding: ProcessBindingSource,
    /// Where execution state evidence comes from.
    pub state: StateSource,
    /// Whether the provider publishes the branch a session runs on.
    pub branch: bool,
    /// Whether the provider can deliver hook events.
    pub dispatch: HookDispatch,
    /// Whether the provider's session-end signal also ends the turn. When it
    /// does not, an end-of-session is a teardown hint rather than `end`.
    pub session_end_is_turn_boundary: bool,
    /// The verified resume invocation, when one exists.
    pub resume: Option<ResumeCommand>,
}

/// Where inventory comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InventorySource {
    /// Provider-written live session files plus durable transcripts: both are
    /// indexed, and a live file and transcript naming the same session id form
    /// one conversation.
    LiveFilesAndTranscripts,
    /// A provider-maintained index plus per-session metadata files.
    IndexAndMetadata,
    /// A read-only database projection.
    Database,
}

/// How a record claims its owning process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnershipSource {
    /// The record itself publishes pid and process start.
    PublishedProcess,
    /// A lock file names a pid; validity requires process-instance checks.
    LockPid,
}

/// How a live record's pane is found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessBindingSource {
    /// The record publishes a tmux handle (`session:@window.%pane`).
    PublishedHandle,
    /// Only the process is known; pane resolution derives from ancestry or
    /// the controlling tty.
    DerivedOnly,
}

/// Where execution state evidence comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateSource {
    /// The provider publishes its own live state.
    Published,
    /// Only registered hook events carry state.
    Hooks,
}

/// Whether a provider can be given hook registrations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookDispatch {
    /// Per-native-event hooks; the event name selects the mapping row.
    PerEvent,
    /// The provider supports no hook registrations.
    Unsupported,
}

/// A verified resume invocation: an executable plus an argument vector, never
/// a shell string. The provider session id is appended as one argument by the
/// caller, so `claude --resume <id>` is `executable` `claude` with argv
/// `["--resume"]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumeCommand {
    pub executable: OsString,
    pub argv: Vec<OsString>,
}

/// One collector's report that something it tried to read failed. A source
/// error never fails the snapshot: the record it concerns is excluded or its
/// field is `Unknown`, and the error itself is retained for the evidence view.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct SourceError {
    /// Which evidence source produced it (`claude live file`, `transcript`,
    /// `tmux`, ...).
    pub source: String,
    pub detail: String,
}

/// Execution state as a provider published or reported it - the normalized
/// form the core arbitrates, not the provider's raw strings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StateEvidence {
    /// The provider published a live state.
    Published(PublishedState),
    /// Nothing applicable: the field stays `Unknown`.
    Absent,
}

/// A provider's own live-state reading.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishedState {
    /// The mapped status; `None` when the provider's value is not one the
    /// mapping knows, which is an `Unknown` with the raw value retained -
    /// never a guess at the nearest state.
    pub status: Option<PublishedStatus>,
    /// The provider's raw status string, for the evidence view; `None`
    /// when the live record carried no status key at all.
    pub raw: Option<String>,
    /// The provider's own reason for a wait ("permission prompt"), kept
    /// verbatim; only meaningful while `status` is `Waiting`.
    pub waiting_for: Option<String>,
}

/// The execution states a v1 provider can publish.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublishedStatus {
    Busy,
    Idle,
    Waiting,
}
