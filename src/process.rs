//! Live-process evidence: one `ps` snapshot per observation, the
//! `(pid, pid_start)` liveness authority, and the parent chain that pane
//! resolution walks.
//!
//! Process start times are compared as epoch seconds within one second.
//! The platform reports the kernel's start time (`ps -o lstart`, read in
//! UTC) truncated to the second, and provider-formatted start times are
//! normalized to the same epoch before comparing. Elapsed time (`etime`)
//! is not used: deriving a start from it depends on when `ps` sampled, and
//! a loaded machine's `ps -A` can take over a second. A process the platform cannot date
//! is lower-authority evidence: the pid is alive, but reuse cannot be ruled
//! out. A pid alone is never identity - lock files and session records
//! outlive the processes they name, and a recycled pid would otherwise
//! resurrect a dead session.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::time::Duration;

/// When a process started, for telling a live pid from a recycled one.
///
/// Compared within one second: `lstart` truncates to whole seconds and
/// providers report their own quantized times, so an exact match would
/// reject the true instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessStart {
    /// Epoch seconds.
    At(u64),
    /// The platform or provider reported no start time. The instance is
    /// pid-only evidence and cannot be distinguished from reuse.
    Unavailable,
}

impl ProcessStart {
    /// Whether two start times name the same instance: both known and no
    /// more than a second apart. `None` when either side cannot say, which
    /// is neither a match nor a mismatch.
    pub fn matches(&self, other: &ProcessStart) -> Option<bool> {
        match (self, other) {
            (ProcessStart::At(a), ProcessStart::At(b)) => {
                Some(a.abs_diff(*b) <= Duration::from_secs(1).as_secs())
            }
            _ => None,
        }
    }
}

/// A `(pid, pid_start)` claim: the only process identity this tool trusts.
/// Two records naming the same pid are the same process only while their
/// start times agree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessInstance {
    pub pid: u32,
    pub pid_start: ProcessStart,
}

/// What a snapshot can prove about a claimed process instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Liveness {
    /// The running process is the claimed instance: the pid is live and,
    /// when both sides carry a start time or an executable name, they agree.
    Instance,
    /// The pid is alive but the instance cannot be proven - a start time is
    /// missing on a side that would need it. Lower-authority evidence:
    /// enough to keep an attachment live, never enough to prove identity.
    PidOnly(String),
    /// The pid is gone or provably belongs to a different instance: dead,
    /// a zombie, a mismatched start time, or a different executable. The
    /// only verdict that retires live claims.
    Dead(String),
    /// Nothing could be checked - the process table itself failed to read.
    /// Neither life nor death is proven; nothing is reaped on this.
    Unverifiable(String),
}

impl Liveness {
    /// Whether the claim is consistent with a still-live process.
    /// `Unverifiable` counts: failing to check is not evidence of death.
    pub fn may_be_live(&self) -> bool {
        matches!(
            self,
            Liveness::Instance | Liveness::PidOnly(_) | Liveness::Unverifiable(_)
        )
    }
}

/// One process-table row: the fields pane resolution and liveness need.
#[derive(Debug, Clone)]
pub struct ProcessRow {
    pub pid: u32,
    pub ppid: u32,
    /// The kernel's start time from `lstart`, epoch seconds. `Unavailable`
    /// when `ps` reported no parseable start - the pid is observably live,
    /// so the row is kept as pid-only evidence rather than dropped and
    /// reaped as dead.
    pub start: ProcessStart,
    /// The executable basename: `comm`'s path stripped, and the `-` login
    /// shell marker (`-zsh`) removed. `None` where the platform gave nothing.
    pub exe: Option<String>,
    /// The controlling terminal as a `/dev/` path, or `None` (`?`, `??`,
    /// `-`): a daemonized process keeps its tty after losing its ancestry,
    /// which is what makes it the last-resort pane binding.
    pub tty: Option<String>,
    /// First state letter of `stat`. `Z` is a zombie: present in the table
    /// but dead.
    pub state: char,
}

impl ProcessRow {
    /// Whether the process is a zombie: a dead child not yet reaped. It
    /// still occupies the pid, so liveness has to say so itself.
    fn is_zombie(&self) -> bool {
        self.state == 'Z'
    }
}

/// One `ps` snapshot. Reading the whole table at once keeps a refresh
/// self-consistent: a pid, its ancestors and its tty all come from the same
/// instant.
#[derive(Debug)]
pub struct ProcessTable {
    rows: HashMap<u32, ProcessRow>,
}

/// A `ps` that failed. Carries argv and exit state for the evidence view.
#[derive(Debug)]
pub struct Error {
    pub argv: String,
    pub code: Option<i32>,
    pub detail: String,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.code {
            Some(code) => write!(f, "`{}` exited {code}: {}", self.argv, self.detail),
            None => write!(f, "`{}`: {}", self.argv, self.detail),
        }
    }
}

impl std::error::Error for Error {}

impl ProcessTable {
    /// `ps -A -o ...`: every process, as one snapshot. `LC_ALL=C` keeps the
    /// field formats stable across locales; the POSIX `TZ=UTC0` makes
    /// `lstart` a UTC ctime without consulting a timezone database.
    pub fn snapshot() -> Result<ProcessTable, Error> {
        let argv = "ps -A -o pid=,ppid=,lstart=,stat=,tty=,comm=";
        #[rustfmt::skip]
        let out = std::process::Command::new("ps")
            .args(["-A", "-o", "pid=,ppid=,lstart=,stat=,tty=,comm="])
            .env("LC_ALL", "C")
            .env("TZ", "UTC0")
            .output()
            .map_err(|e| Error { argv: argv.to_owned(), code: None, detail: e.to_string() })?; // coverage: off - needs a PATH without ps
        if !out.status.success() {
            #[rustfmt::skip]
            return Err(Error { argv: argv.to_owned(), code: out.status.code(), detail: String::from_utf8_lossy(&out.stderr).trim().to_owned() }); // coverage: off - `ps -A` with these fields does not fail
        }
        Ok(ProcessTable {
            rows: String::from_utf8_lossy(&out.stdout)
                .lines()
                .filter_map(parse_row)
                .map(|row| (row.pid, row))
                .collect(),
        })
    }

    /// The row for `pid`, when a live-or-zombie process carries it.
    pub fn get(&self, pid: u32) -> Option<&ProcessRow> {
        self.rows.get(&pid)
    }

    /// Every row, for callers that pick processes by parentage or name.
    pub fn rows(&self) -> impl Iterator<Item = &ProcessRow> {
        self.rows.values()
    }

    /// A table built out of `rows` instead of a live `ps` run - callers
    /// carrying their own process observations, and tests faking a shape a
    /// live machine cannot guarantee.
    pub fn from_rows(rows: Vec<ProcessRow>) -> ProcessTable {
        ProcessTable {
            rows: rows.into_iter().map(|row| (row.pid, row)).collect(),
        }
    }

    /// `pid` itself first, then each parent up to the table's root. A
    /// missing row or a cycle (which a real table cannot have, but a
    /// half-read one could) ends the walk.
    pub fn ancestors(&self, pid: u32) -> Vec<u32> {
        let mut chain = Vec::new();
        let mut seen = HashSet::new();
        let mut current = Some(pid);
        while let Some(p) = current {
            if !seen.insert(p) {
                break; // coverage: off - a real process table cannot hold a cycle
            }
            match self.rows.get(&p) {
                Some(row) => {
                    chain.push(p);
                    current = (row.ppid != p).then_some(row.ppid);
                }
                None => break,
            }
        }
        chain
    }

    /// Whether the claimed instance is the process still running at
    /// `claim.pid`.
    ///
    /// `expected_exe` is what the claim implies about the executable - a
    /// provider's lock or session file names its own kind of process. When
    /// it is given and the running basename provably differs, the pid was
    /// reused or the process exec'd elsewhere; either way not the claimed
    /// instance. A difference that could be the platform's `comm`
    /// truncation is unproven rather than a mismatch.
    pub fn is_live(&self, claim: &ProcessInstance, expected_exe: Option<&str>) -> Liveness {
        let Some(row) = self.rows.get(&claim.pid) else {
            return Liveness::Dead(format!("pid {} has no process", claim.pid));
        };
        if row.is_zombie() {
            return Liveness::Dead(format!("pid {} is a zombie", claim.pid));
        }
        if let (Some(expected), Some(observed)) = (expected_exe, row.exe.as_deref())
            && exe_mismatch(expected, observed)
        {
            return Liveness::Dead(format!(
                "pid {} runs `{observed}`, not `{expected}`",
                claim.pid
            ));
        }
        match claim.pid_start.matches(&row.start) {
            Some(true) => Liveness::Instance,
            Some(false) => Liveness::Dead(format!(
                "pid {} started at a different time than claimed",
                claim.pid
            )),
            // A start time missing on either side cannot prove the instance;
            // the pid is live and that is all that is known.
            None => Liveness::PidOnly(format!(
                "pid {} is live but its start time is unverifiable",
                claim.pid
            )),
        }
    }
}

/// Whether the observed basename proves the process is not the claimed
/// executable. `comm` can be a truncation - Linux caps it at 15 bytes and
/// macOS can emit a 16-byte argv0 prefix - so a mismatch where the
/// observed name is a cap-length prefix of the expected one may be the
/// same executable cut short, and proves nothing. A shorter `observed`
/// cannot be a truncation, so the mismatch stands.
fn exe_mismatch(expected: &str, observed: &str) -> bool {
    expected != observed && !(observed.len() >= 15 && expected.starts_with(observed))
}

/// One `ps` output row: `pid ppid <lstart: 5 fields> stat tty comm...`.
/// `comm` is the rest of the line - an argv0 with a space stays one field.
fn parse_row(line: &str) -> Option<ProcessRow> {
    let fields: Vec<&str> = line.split_whitespace().collect();
    let head = fields.get(..9)?;
    let (pid, ppid, lstart, stat, tty) = (head[0], head[1], &head[2..7], head[7], head[8]);
    let comm = &fields[9..];
    let pid = pid.parse().ok()?;
    let ppid = ppid.parse().ok()?;
    let start = parse_utc_ctime(&lstart.join(" "))
        .map(ProcessStart::At)
        .unwrap_or(ProcessStart::Unavailable);
    let state = stat.chars().next()?; // coverage: off - split_whitespace yields no empty fields
    let tty = normalize_tty(tty);
    let comm = comm.join(" ");
    let comm = comm.trim();
    Some(ProcessRow {
        pid,
        ppid,
        start,
        exe: normalize_exe(comm),
        tty,
        state,
    })
}

/// The executable basename of a `comm` value: a path becomes its last
/// component and a leading `-` (the login-shell marker on argv0) comes off.
/// `None` for `?`-style blanks and empty output.
fn normalize_exe(comm: &str) -> Option<String> {
    let base = comm.rsplit('/').next().unwrap_or(comm);
    let base = base.strip_prefix('-').unwrap_or(base);
    (!base.is_empty() && base != "?" && base != "??").then(|| base.to_owned())
}

/// The controlling terminal as `/dev/<name>`: `ps` prints `ttys003` or
/// `pts/3` while tmux prints `/dev/ttys003` or `/dev/pts/3`, and only the
/// shared form can join them. `?`, `??` and `-` mean no controlling tty.
fn normalize_tty(tty: &str) -> Option<String> {
    match tty {
        "?" | "??" | "-" => None,
        t if t.starts_with("/dev/") => Some(t.to_owned()),
        t => Some(format!("/dev/{t}")),
    }
}

/// A provider-published start time normalized to epoch seconds.
///
/// Provider records format starts as UTC ctime (`Tue Sep 22 16:18:53 2026`
/// for Claude's `procStart`), the same form `ps -o lstart` prints under
/// `TZ=UTC`. Normalizing both to epoch makes them comparable inside the
/// one-second window, which is what the `pid_start` pair exists for.
pub fn parse_utc_ctime(text: &str) -> Option<u64> {
    // `<wday> <month> <day> <hh:mm:ss> <year>`; the weekday carries no
    // information beyond a sanity hint and is ignored.
    let fields: Vec<&str> = text.split_whitespace().collect();
    if fields.len() != 5 {
        return None;
    }
    let month = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ]
    .iter()
    .position(|m| *m == fields[1])? as i64
        + 1;
    let day = fields[2].parse::<u64>().ok()?;
    let mut hms = fields[3].split(':');
    let (hour, min, sec) = (
        hms.next()? // coverage: off - the field always has a first part
            .parse::<u64>()
            .ok()?,
        hms.next()?.parse::<u64>().ok()?,
        hms.next()?.parse::<u64>().ok()?,
    );
    if hms.next().is_some() {
        return None;
    }
    let year = fields[4].parse::<i64>().ok()?;
    if day == 0 || day > 31 || hour > 23 || min > 59 || sec > 60 {
        return None;
    }
    let days = days_from_civil(year, month, day);
    u64::try_from(days * 86_400 + (hour * 3_600 + min * 60 + sec) as i64).ok()
}

/// Days since the Unix epoch for a civil date (Howard Hinnant's
/// `days_from_civil`), so a UTC timestamp needs no timezone database.
pub(crate) fn days_from_civil(y: i64, m: i64, d: u64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_times_compare_within_one_second() {
        let at = ProcessStart::At(1_000);
        assert_eq!(at.matches(&ProcessStart::At(1_001)), Some(true));
        assert_eq!(at.matches(&ProcessStart::At(999)), Some(true));
        assert_eq!(at.matches(&ProcessStart::At(1_002)), Some(false));
        assert_eq!(at.matches(&ProcessStart::Unavailable), None);
        assert_eq!(ProcessStart::Unavailable.matches(&at), None);
    }

    #[test]
    fn a_tty_that_arrives_absolute_keeps_its_path() {
        assert_eq!(normalize_tty("/dev/pts/4").as_deref(), Some("/dev/pts/4"));
    }

    #[test]
    fn ps_rows_parse_and_normalize() {
        const L: &str = "Mon Jan 12 13:46:40 1970";
        let row =
            parse_row(&format!("  3091  3090 {L} S+   ttys042  -/bin/zsh")).expect("a ps row");
        assert_eq!(row.pid, 3091);
        assert_eq!(row.ppid, 3090);
        // `lstart` under `TZ=UTC` is the start itself, to the second.
        assert_eq!(row.start, ProcessStart::At(1_000_000));
        assert_eq!(row.state, 'S');
        assert_eq!(row.tty.as_deref(), Some("/dev/ttys042"));
        assert_eq!(row.exe.as_deref(), Some("zsh"));

        let full_path = parse_row(&format!("  1     0 {L} Ss   ??       /sbin/launchd")).unwrap();
        assert_eq!(full_path.tty, None);
        assert_eq!(full_path.exe.as_deref(), Some("launchd"));
        assert!(!full_path.is_zombie());

        let zombie = parse_row(&format!("  5     1 {L} Z    ?       <defunct>")).unwrap();
        assert!(zombie.is_zombie());
        assert_eq!(zombie.exe.as_deref(), Some("<defunct>"));

        // A missing command column and a nonsense line both fail cleanly.
        let bare = parse_row(&format!("  9     1 {L} S    -")).unwrap();
        assert_eq!(bare.exe, None);
        assert_eq!(bare.tty, None);
        assert!(parse_row("not a process row").is_none());
        assert!(parse_row("").is_none());
        // A row whose pid or ppid does not parse is not a process row.
        for bad in [
            format!("  x     1 {L} S    ttys0  zsh"),
            format!("  9     x {L} S    ttys0  zsh"),
        ] {
            assert!(parse_row(&bad).is_none(), "{bad}");
        }
        // An unparseable start keeps the row with an unavailable start:
        // the pid is observably live, so it is pid-only evidence, not
        // a dead one.
        let undated = parse_row("  9     1 Mon Xxx 12 13:46:40 1970 S    ttys0  zsh").unwrap();
        assert_eq!(undated.start, ProcessStart::Unavailable);
        let table = ProcessTable::from_rows(vec![undated]);
        let claim = ProcessInstance {
            pid: 9,
            pid_start: ProcessStart::At(1_000_000),
        };
        assert!(matches!(table.is_live(&claim, None), Liveness::PidOnly(_))); // coverage: off - miss edge is the assert failing
    }

    #[test]
    fn utc_ctime_parses_to_epoch() {
        // 2026-09-22 16:18:53 UTC.
        assert_eq!(
            parse_utc_ctime("Tue Sep 22 16:18:53 2026"),
            Some(1_790_093_933)
        );
        // A provider may pad the day; a weekday is not verified.
        assert_eq!(
            parse_utc_ctime("Mon  Sep  1 00:00:00 2025"),
            Some(1_756_684_800)
        );
        // January exercises the civil-date wrap (months are 0-based
        // inside the calculation).
        assert_eq!(
            parse_utc_ctime("Thu Jan 1 00:00:00 2026"),
            Some(1_767_225_600)
        );
        for bad in [
            "",
            "Sep 22 16:18:53 2026",
            "Tue Sep 22 16:18:53",
            "Tue Foo 22 16:18:53 2026",
            "Tue Sep 0 16:18:53 2026",
            "Tue Sep 32 16:18:53 2026",
            "Tue Sep 22 25:18:53 2026",
            "Tue Sep 22 16:18:53:9 2026",
            "Tue Sep x 16:18:53 2026",
            "Tue Sep 22 x:18:53 2026",
            "Tue Sep 22 16:x:53 2026",
            "Tue Sep 22 16:18:x 2026",
            "Tue Sep 22 16:18:53 x",
            // Short times: a minute or hour is not a ctime.
            "Tue Sep 22 16 2026",
            "Tue Sep 22 16:18 2026",
            // Before the epoch: parses but cannot be an instance stamp.
            "Thu Jan 1 00:00:00 1900",
            "Thu Jan 1 00:00:00 0",
        ] {
            assert_eq!(parse_utc_ctime(bad), None, "{bad}");
        }
    }

    #[test]
    fn errors_describe_their_argv() {
        let err = Error {
            argv: "ps".to_owned(),
            code: Some(1),
            detail: "no such field".to_owned(),
        };
        assert_eq!(err.to_string(), "`ps` exited 1: no such field");
        let err = Error {
            argv: "ps".to_owned(),
            code: None,
            detail: "spawn failed".to_owned(),
        };
        assert_eq!(err.to_string(), "`ps`: spawn failed");
    }

    #[test]
    fn a_live_snapshot_reads_the_table() {
        let table = ProcessTable::snapshot().expect("ps runs");
        // The test process itself must be in the table, with sane fields.
        let row = table
            .get(std::process::id())
            .expect("the test process is in the table");
        assert_ne!(row.state, 'Z');
        // Its ancestry chain reaches pid 1 or a missing parent at the top.
        let chain = table.ancestors(std::process::id());
        assert_eq!(chain.first(), Some(&std::process::id()));
        assert!(chain.len() >= 2);
        // A pid far above any real one has no row and no ancestors.
        let bogus = 4_000_000;
        assert!(table.get(bogus).is_none());
        assert!(table.ancestors(bogus).is_empty());
    }

    #[test]
    fn liveness_proves_and_rejects() {
        let table = ProcessTable::snapshot().expect("ps runs");
        let pid = std::process::id();
        let start = table.get(pid).unwrap().start;

        assert_eq!(
            table.is_live(
                &ProcessInstance {
                    pid,
                    pid_start: start
                },
                None
            ),
            Liveness::Instance
        );
        // A pid-only claim is pid-only evidence.
        let pid_only = ProcessInstance {
            pid,
            pid_start: ProcessStart::Unavailable,
        };
        let live = table.is_live(&pid_only, None);
        assert!(matches!(live, Liveness::PidOnly(_))); // coverage: off - miss edge is the assert failing
        // A wrong start time is a different instance, not a weak one.
        let ProcessStart::At(at) = start else {
            panic!("the test process has a start time") // coverage: off - lstart always parses on a live row
        };
        let later = ProcessInstance {
            pid,
            pid_start: ProcessStart::At(at + 3_600),
        };
        let live = table.is_live(&later, None);
        assert!(matches!(live, Liveness::Dead(_))); // coverage: off - miss edge is the assert failing
        // A wrong expected executable rejects; the right one passes, and
        // an executable the platform cannot report degrades the check to
        // start-time evidence alone. Fabricated rows keep this
        // independent of what `comm` reports for the test binary.
        let table = ProcessTable::from_rows(vec![
            ProcessRow {
                pid: 11,
                ppid: 1,
                start: ProcessStart::At(1_700_000_000),
                exe: Some("worker".to_owned()),
                tty: None,
                state: 'S',
            },
            ProcessRow {
                pid: 12,
                ppid: 1,
                start: ProcessStart::At(1_700_000_000),
                exe: None,
                tty: None,
                state: 'S',
            },
        ]);
        let worker = ProcessInstance {
            pid: 11,
            pid_start: ProcessStart::At(1_700_000_000),
        };
        let live = table.is_live(&worker, Some("other"));
        assert!(matches!(live, Liveness::Dead(_))); // coverage: off - miss edge is the assert failing
        assert_eq!(table.is_live(&worker, Some("worker")), Liveness::Instance);
        let unseen = ProcessInstance {
            pid: 12,
            pid_start: ProcessStart::At(1_700_000_000),
        };
        assert_eq!(table.is_live(&unseen, Some("other")), Liveness::Instance);
        // A basename that is only a platform-truncated prefix of the
        // expected name proves nothing: the check falls through to the
        // start time rather than reaping the live instance.
        let truncated = ProcessTable::from_rows(vec![ProcessRow {
            pid: 13,
            ppid: 1,
            start: ProcessStart::At(1_700_000_000),
            exe: Some("provider-with-l".to_owned()),
            tty: None,
            state: 'S',
        }]);
        let claim = ProcessInstance {
            pid: 13,
            pid_start: ProcessStart::At(1_700_000_000),
        };
        assert_eq!(
            truncated.is_live(&claim, Some("provider-with-long-name")),
            Liveness::Instance
        );
        // A same-cap basename that is not a prefix is still a proven
        // mismatch.
        let live = truncated.is_live(&claim, Some("provider-with-x"));
        assert!(matches!(live, Liveness::Dead(_))); // coverage: off - miss edge is the assert failing
        // A dead pid is dead.
        let gone = ProcessInstance {
            pid: 4_000_000,
            pid_start: ProcessStart::Unavailable,
        };
        assert!(matches!(table.is_live(&gone, None), Liveness::Dead(_))); // coverage: off - miss edge is the assert failing
    }

    #[test]
    fn a_zombie_is_dead_evidence() {
        // A child that exits but is never wait()ed stays a zombie in the
        // parent's table: liveness cannot say it is alive.
        let mut child = std::process::Command::new("sleep")
            .arg("1")
            .spawn()
            .expect("sleep spawns");
        let pid = child.id();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let table = loop {
            let table = ProcessTable::snapshot().expect("ps runs");
            if table.get(pid).is_some_and(|row| row.is_zombie()) {
                break table;
            }
            if std::time::Instant::now() > deadline {
                panic!("the exited child never became a zombie") // coverage: off - the miss edge is the test's own timeout
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        let instance = ProcessInstance {
            pid,
            pid_start: ProcessStart::Unavailable,
        };
        assert!(matches!(table.is_live(&instance, None), Liveness::Dead(_))); // coverage: off - miss edge is the assert failing
        let _ = child.wait();
    }
}
