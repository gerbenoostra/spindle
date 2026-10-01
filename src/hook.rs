//! `agent-sessions hook <provider> <event>`: one atomic ping from an
//! agent's own hook machinery.
//!
//! The argv words select the event-mapping row - provider and native event
//! are explicit because providers share event names. The provider's hook
//! payload arrives on stdin (JSON; unknown keys are ignored), one record
//! is appended to the journal, and the process exits zero on every write
//! failure: a hook that broke the agent would be worse than a dropped
//! event. Debug detail goes to stderr; stdout stays empty.
//!
//! An event name the mapping does not know is still recorded - unmapped,
//! as diagnostic activity carrying a weak five-second lease.

use std::fs;
use std::path::Path;

use crate::runtime::Provider;
use crate::store::{self, NormEvent, Record, Store};

/// The mapped outcome of one `(provider, native event)` pair, or `None`
/// for an unmapped ping.
struct Mapped {
    event: NormEvent,
    /// The wait reason an `awaiting` record carries.
    reason: Option<String>,
}

/// Run the `hook` subcommand. `provider`/`event` are the argv words;
/// `stdin` is the provider's payload. Err is a usage error (unknown
/// provider); every operational failure exits zero.
pub fn run(provider: &str, event: &str, stdin: &[u8]) -> Result<(), String> {
    let provider = parse_provider(provider)?;
    let payload = Payload::parse(stdin);
    let Some(dir) = crate::config::Config::state_dir(&|name| std::env::var(name).ok()) else {
        debug("no XDG_STATE_HOME or HOME to place the store under");
        return Ok(());
    };
    let store = Store::open(dir);
    let mut record = Record::new(provider.as_str(), payload.session_id(), event);
    if let Some(mapped) = map(provider, event, &payload) {
        record.event = Some(mapped.event);
        record.reason = mapped.reason;
    }
    record.cwd = payload.cwd();
    record.pts = payload.timestamp_ms();
    record.pseq = payload.sequence();
    if let Some((pid, start)) = resolve_process(provider, &payload) {
        record.pid = Some(pid);
        record.pid_start = start;
    }
    match store.append(record) {
        Ok(_) => Ok(()),
        Err(e) => {
            debug(&format!("could not commit the event: {e}"));
            Ok(())
        }
    }
}

/// The provider's wire name, or a usage error. Hook configuration is
/// authored: a misspelled provider corrects loudly rather than recording
/// events under a name nothing reads.
fn parse_provider(name: &str) -> Result<Provider, String> {
    Provider::parse(name)
        .ok_or_else(|| format!("unknown provider `{name}` (expected claude, vibe or devin)"))
}

/// Debug stderr only - a hook never writes stdout and never exits nonzero
/// for an operational failure.
fn debug(message: &str) {
    eprintln!("agent-sessions hook: {message}");
}

/// The provider's hook payload, defensively parsed. A missing or
/// non-JSON stdin is an empty payload, not a failure - the record still
/// lands with what it has.
struct Payload(serde_json::Value);

impl Payload {
    fn parse(stdin: &[u8]) -> Payload {
        let text = String::from_utf8_lossy(stdin);
        Payload(serde_json::from_str(&text).unwrap_or(serde_json::Value::Null))
    }

    /// The first present string value among `keys`.
    fn get(&self, keys: &[&str]) -> Option<&str> {
        keys.iter()
            .find_map(|k| self.0.get(k).and_then(|v| v.as_str()))
    }

    /// The provider session id. Claude writes `session_id`; the key itself
    /// carries no provider rules, so every plausible spelling is accepted.
    fn session_id(&self) -> &str {
        self.get(&["session_id", "sessionId", "sessionID"])
            .unwrap_or("")
    }

    fn cwd(&self) -> Option<String> {
        self.get(&["cwd", "origin_directory"]).map(str::to_owned)
    }

    /// The tool a `PreToolUse`/`PostToolUse` fired for - what tells a
    /// waiting `PreToolUse` from a non-waiting one.
    fn tool(&self) -> Option<&str> {
        self.get(&["tool_name", "toolName", "tool"])
    }

    /// A `Notification`'s type - `permission_prompt`, `elicitation_dialog`
    /// and friends.
    fn notification(&self) -> Option<&str> {
        self.get(&["notification_type", "notificationType", "type"])
    }

    /// The producer's timestamp as epoch milliseconds: an ISO-8601 string
    /// or a bare number in seconds or milliseconds, whichever the provider
    /// wrote.
    fn timestamp_ms(&self) -> Option<u64> {
        for key in ["timestamp", "ts", "time"] {
            let Some(value) = self.0.get(key) else {
                continue;
            };
            if let Some(text) = value.as_str()
                && let Some(t) = crate::claude::parse_iso8601(text)
            {
                return Some(store::epoch_ms(t));
            }
            if let Some(n) = value.as_u64() {
                // Below 1e12 the number is seconds, not milliseconds.
                return Some(if n < 1_000_000_000_000 { n * 1000 } else { n });
            } // coverage: off - the unexecuted instantiation's region edge
        }
        None
    }

    /// The producer's own sequence number, when the provider keeps one.
    fn sequence(&self) -> Option<u64> {
        ["seq", "sequence", "event_seq"]
            .iter()
            .find_map(|k| self.0.get(k).and_then(|v| v.as_u64()))
    }
}

/// The event-mapping table, executable. `(provider, native event, payload)`
/// -> normalized event; a miss is an unmapped ping. The waiting rows are
/// the only ones that consult the payload.
fn map(provider: Provider, event: &str, payload: &Payload) -> Option<Mapped> {
    let awaiting = |reason: &str| {
        Some(Mapped {
            event: NormEvent::Awaiting,
            reason: Some(reason.to_owned()),
        })
    };
    let plain = |event| {
        Some(Mapped {
            event,
            reason: None,
        })
    };
    match (provider, event) {
        (Provider::Claude, "SessionStart" | "UserPromptSubmit") => plain(NormEvent::Start),
        (Provider::Claude, "PreToolUse") => match payload.tool() {
            Some("AskUserQuestion") => awaiting("question"),
            Some("ExitPlanMode") => awaiting("plan approval"),
            _ => plain(NormEvent::Activity),
        },
        (
            Provider::Claude,
            "PostToolUse" | "PostToolUseFailure" | "SubagentStart" | "SubagentStop",
        ) => plain(NormEvent::Activity),
        (Provider::Claude, "PermissionRequest") => awaiting("permission prompt"),
        (Provider::Claude, "Notification") => match payload.notification() {
            Some("permission_prompt") => awaiting("permission prompt"),
            Some(t @ ("elicitation_dialog" | "elicitation_url_dialog")) => awaiting(t),
            Some("agent_needs_input") => awaiting("agent needs input"),
            _ => None,
        },
        (Provider::Claude, "Stop") => plain(NormEvent::End),
        (Provider::Claude, "StopFailure") => plain(NormEvent::Error),
        (Provider::Claude, "SessionEnd") => plain(NormEvent::TeardownHint),
        (Provider::Vibe, "pre_tool" | "post_tool") => plain(NormEvent::Activity),
        (Provider::Vibe, "post_agent") => plain(NormEvent::End),
        (Provider::Devin, "SessionStart" | "UserPromptSubmit") => plain(NormEvent::Start),
        (Provider::Devin, "PreToolUse") => match payload.tool() {
            Some("ask_user_question") => awaiting("question"),
            Some("exit_plan_mode") => awaiting("plan approval"),
            _ => plain(NormEvent::Activity),
        },
        (Provider::Devin, "PostToolUse" | "PostCompaction") => plain(NormEvent::Activity),
        (Provider::Devin, "PermissionRequest") => awaiting("permission prompt"),
        (Provider::Devin, "Stop") => plain(NormEvent::End),
        (Provider::Devin, "SessionEnd") => plain(NormEvent::TeardownHint),
        _ => None,
    }
}

/// The process instance the event's conversation is bound to. The payload
/// may carry `pid`/`pid_start` directly; for Claude the provider's own
/// session file is authoritative - `sessions/<pid>.json` naming this
/// session id.
fn resolve_process(provider: Provider, payload: &Payload) -> Option<(u32, Option<u64>)> {
    if provider == Provider::Claude {
        let root = crate::claude::default_root().ok()?;
        if let Some(found) = find_claude_process(&root, payload.session_id()) {
            return Some(found);
        }
    }
    let pid = ["pid", "process_id"]
        .iter()
        .find_map(|k| payload.0.get(k).and_then(|v| v.as_u64()))
        .and_then(|p| u32::try_from(p).ok())?;
    let start = ["pid_start", "proc_start", "process_start"]
        .iter()
        .find_map(|k| payload.0.get(k).and_then(|v| v.as_u64()));
    Some((pid, start))
}

/// The `(pid, pid_start)` Claude's live session file binds `session_id` to.
fn find_claude_process(root: &Path, session_id: &str) -> Option<(u32, Option<u64>)> {
    let sessions = root.join("sessions");
    let entries = fs::read_dir(&sessions).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        if value.get("sessionId").and_then(|v| v.as_str()) != Some(session_id) {
            continue;
        }
        let pid = value
            .get("pid")
            .and_then(|v| v.as_u64())
            .and_then(|p| u32::try_from(p).ok())?;
        let start = value
            .get("procStart")
            .and_then(|v| v.as_str())
            .and_then(crate::process::parse_utc_ctime);
        return Some((pid, start));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload(json: &str) -> Payload {
        Payload::parse(json.as_bytes())
    }

    #[test]
    fn every_mapped_row_maps_and_everything_else_is_a_ping() {
        let empty = payload("{}");
        // Claude's full lifecycle.
        for (event, want) in [
            ("SessionStart", NormEvent::Start),
            ("UserPromptSubmit", NormEvent::Start),
            ("PostToolUse", NormEvent::Activity),
            ("PostToolUseFailure", NormEvent::Activity),
            ("SubagentStart", NormEvent::Activity),
            ("SubagentStop", NormEvent::Activity),
            ("Stop", NormEvent::End),
            ("StopFailure", NormEvent::Error),
            ("SessionEnd", NormEvent::TeardownHint),
            ("PermissionRequest", NormEvent::Awaiting),
        ] {
            let mapped = map(Provider::Claude, event, &empty);
            assert_eq!(mapped.map(|m| m.event), Some(want), "{event}");
        }
        // PreToolUse is waiting only for the two named tools.
        let ask = payload("{\"tool_name\":\"AskUserQuestion\"}");
        assert_eq!(
            map(Provider::Claude, "PreToolUse", &ask).map(|m| m.event),
            Some(NormEvent::Awaiting)
        );
        let plan = payload("{\"tool_name\":\"ExitPlanMode\"}");
        assert_eq!(
            map(Provider::Claude, "PreToolUse", &plan)
                .and_then(|m| m.reason)
                .as_deref(),
            Some("plan approval")
        );
        let bash = payload("{\"tool_name\":\"Bash\"}");
        assert_eq!(
            map(Provider::Claude, "PreToolUse", &bash).map(|m| m.event),
            Some(NormEvent::Activity)
        );
        // Notification waits only on the listed types.
        for (kind, want) in [
            ("permission_prompt", "permission prompt"),
            ("elicitation_dialog", "elicitation_dialog"),
            ("agent_needs_input", "agent needs input"),
        ] {
            let n = payload(&format!("{{\"notification_type\":\"{kind}\"}}"));
            let mapped = map(Provider::Claude, "Notification", &n).unwrap();
            assert_eq!(mapped.event, NormEvent::Awaiting, "{kind}");
            assert_eq!(mapped.reason.as_deref(), Some(want));
        }
        let idle = payload("{\"notification_type\":\"idle_prompt\"}");
        assert!(map(Provider::Claude, "Notification", &idle).is_none());
        assert!(map(Provider::Claude, "Unlisted", &empty).is_none());

        // Vibe and Devin's rows.
        assert_eq!(
            map(Provider::Vibe, "post_agent", &empty).map(|m| m.event),
            Some(NormEvent::End)
        );
        assert_eq!(
            map(Provider::Vibe, "pre_tool", &empty).map(|m| m.event),
            Some(NormEvent::Activity)
        );
        assert!(map(Provider::Vibe, "waiting", &empty).is_none());
        assert_eq!(
            map(Provider::Devin, "Stop", &empty).map(|m| m.event),
            Some(NormEvent::End)
        );
        let ask = payload("{\"tool_name\":\"ask_user_question\"}");
        assert_eq!(
            map(Provider::Devin, "PreToolUse", &ask).map(|m| m.event),
            Some(NormEvent::Awaiting)
        );
        assert_eq!(
            map(Provider::Devin, "PostCompaction", &empty).map(|m| m.event),
            Some(NormEvent::Activity)
        );
        assert_eq!(
            map(Provider::Devin, "SessionEnd", &empty).map(|m| m.event),
            Some(NormEvent::TeardownHint)
        );
        // Devin's remaining rows: start, the plan-approval and ordinary
        // tool arms of PreToolUse, PostToolUse, the permission wait.
        assert_eq!(
            map(Provider::Devin, "SessionStart", &empty).map(|m| m.event),
            Some(NormEvent::Start)
        );
        assert_eq!(
            map(Provider::Devin, "UserPromptSubmit", &empty).map(|m| m.event),
            Some(NormEvent::Start)
        );
        let plan = payload("{\"tool_name\":\"exit_plan_mode\"}");
        assert_eq!(
            map(Provider::Devin, "PreToolUse", &plan)
                .and_then(|m| m.reason)
                .as_deref(),
            Some("plan approval")
        );
        let bash = payload("{\"tool_name\":\"bash\"}");
        assert_eq!(
            map(Provider::Devin, "PreToolUse", &bash).map(|m| m.event),
            Some(NormEvent::Activity)
        );
        assert_eq!(
            map(Provider::Devin, "PostToolUse", &empty).map(|m| m.event),
            Some(NormEvent::Activity)
        );
        assert_eq!(
            map(Provider::Devin, "PermissionRequest", &empty)
                .and_then(|m| m.reason)
                .as_deref(),
            Some("permission prompt")
        );
    }

    #[test]
    fn resolve_process_reads_the_payload_or_claude_files() {
        // A provider whose payload carries the instance itself.
        let p = payload("{\"pid\":42,\"pid_start\":99}");
        assert_eq!(resolve_process(Provider::Devin, &p), Some((42, Some(99))));
        let p = payload("{\"process_id\":42,\"proc_start\":99}");
        assert_eq!(resolve_process(Provider::Devin, &p), Some((42, Some(99))));
        // A pid too large is no pid at all, not a wrapped one.
        let p = payload("{\"pid\":4294967297}");
        assert_eq!(resolve_process(Provider::Devin, &p), None);
        // No process claim anywhere: nothing.
        let p = payload("{\"session_id\":\"x\"}");
        assert_eq!(resolve_process(Provider::Devin, &p), None);
    }

    #[test]
    fn find_claude_process_skips_everything_but_the_session_itself() {
        let dir = tempfile_path("sessions");
        let sessions = dir.join("sessions");
        fs::create_dir_all(&sessions).unwrap();
        // Not a .json file: ignored.
        fs::write(sessions.join("notes.txt"), "{}").unwrap();
        // An unreadable .json: a directory masquerading under the name.
        fs::create_dir_all(sessions.join("dir.json")).unwrap();
        // Unparsable JSON and a different session's record: both skipped.
        fs::write(sessions.join("broken.json"), "{oops").unwrap();
        fs::write(
            sessions.join("other.json"),
            "{\"sessionId\":\"other\",\"pid\":1}",
        )
        .unwrap();
        assert!(find_claude_process(&dir, "s1").is_none());
        fs::write(
            sessions.join("7.json"),
            "{\"sessionId\":\"s1\",\"pid\":7,\"procStart\":\"Tue Sep 22 16:18:53 2026\"}",
        )
        .unwrap();
        let found = find_claude_process(&dir, "s1");
        assert_eq!(found.map(|(pid, _)| pid), Some(7));
        // A matching session with no usable pid resolves nothing.
        fs::write(sessions.join("7.json"), "{\"sessionId\":\"s2\"}").unwrap();
        assert!(find_claude_process(&dir, "s2").is_none());
        fs::remove_dir_all(&dir).unwrap();
    }

    /// A unique temp path - hook tests do not need the harness's TempDir.
    fn tempfile_path(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("agent-sessions-hook-{name}-{}", std::process::id()))
    }

    #[test]
    fn payloads_yield_their_identity_fields_defensively() {
        let p = payload(
            "{\"session_id\":\"abc\",\"cwd\":\"/w\",\"timestamp\":\"2026-09-22T16:18:53.123Z\",\"seq\":4}",
        );
        assert_eq!(p.session_id(), "abc");
        assert_eq!(p.cwd().as_deref(), Some("/w"));
        assert!(p.timestamp_ms().is_some());
        assert_eq!(p.sequence(), Some(4));
        // Numeric timestamps read as seconds or milliseconds.
        assert_eq!(
            payload("{\"timestamp\":1790123456}").timestamp_ms(),
            Some(1_790_123_456_000)
        );
        assert_eq!(
            payload("{\"timestamp\":1790123456000}").timestamp_ms(),
            Some(1_790_123_456_000)
        );
        // Garbage in is an empty payload, not a panic.
        let p = payload("not json at all");
        assert_eq!(p.session_id(), "");
        assert!(p.timestamp_ms().is_none());
    }

    #[test]
    fn an_unknown_provider_is_a_usage_error() {
        assert!(run("notanagent", "Stop", b"{}").is_err());
        assert!(parse_provider("claude").is_ok());
    }
}
