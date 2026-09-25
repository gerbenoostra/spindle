//! Runtime evidence fused into live attachments: whether a claimed
//! `(pid, pid_start)` is still the running process, and which pane that
//! process belongs to.
//!
//! Pane resolution is ordered and fails closed - a provider-published
//! handle, then the parent chain to a `pane_pid`, then the controlling
//! tty, then `Unknown`. Cwd never resolves a pane; it binds Work, not
//! panes. Where evidence contradicts itself - a published handle naming
//! one live pane while the process provably sits in another - the binding
//! is `Unknown` with both sides named, never a coin toss.

use std::ffi::OsStr;
use std::time::SystemTime;

use crate::evidence::Evidence;
use crate::process::{Liveness, ProcessInstance, ProcessStart, ProcessTable};
use crate::tmux::{PaneInventory, PaneRef, PublishedHandle};

/// The v1 providers. Adding one is a capability decision, not a value a
/// record can freely carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Provider {
    Claude,
    Vibe,
    Devin,
}

/// A conversation's durable identity: provider plus the provider's own
/// session id. It survives process replacement and resume - neither the
/// pid nor the pane identifies a conversation.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AgentSessionKey {
    pub provider: Provider,
    pub session_id: String,
}

/// Where an attachment claim was observed. Provenance, not trust: every
/// source is re-validated against the live process table each refresh.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvidenceSource {
    /// Provider-published session state (a live session file).
    Published,
    /// A provider lock or index file.
    Lock,
    /// A registered hook event.
    Hook,
    /// Derived from process and tmux evidence alone.
    Derived,
}

/// One claimed process-to-attachment binding, as a plugin observed it.
/// Everything in it is re-checked each refresh; nothing in it is trusted.
#[derive(Debug)]
pub struct ProcessClaim {
    /// The conversation this claim belongs to, when identity is known.
    pub session: Option<AgentSessionKey>,
    pub pid: u32,
    /// The provider-published start, normalized to epoch; `Unavailable`
    /// when the provider does not date its process, which demotes the claim
    /// to pid-only evidence.
    pub pid_start: ProcessStart,
    /// The basename the claimed process should run, when the provider's
    /// identity implies one. A live process with a provably different
    /// basename is not this instance; a name the platform truncated is
    /// unproven rather than wrong.
    pub expected_exe: Option<String>,
    /// The provider-published pane handle (`session:@window.%pane` or a
    /// suffix of it), when the provider publishes one.
    pub published_pane: Option<String>,
    /// Where the claim came from.
    pub source: EvidenceSource,
    /// When the claim's evidence was produced - what orders competing
    /// claims on the same pane.
    pub observed_at: SystemTime,
}

impl ProcessClaim {
    /// The claimed `(pid, pid_start)` pair.
    pub fn instance(&self) -> ProcessInstance {
        ProcessInstance {
            pid: self.pid,
            pid_start: self.pid_start,
        }
    }
}

/// A process's current attachment to the terminal world: its conversation
/// when known, its proven process identity and its pane when provable.
/// Rebuilt every refresh; a record that stops validating stops being live.
#[derive(Debug)]
pub struct LiveAttachment {
    pub session: Option<AgentSessionKey>,
    pub process: ProcessInstance,
    /// The current pane binding: `None` when it could not be proven or a
    /// newer claim owns the pane.
    pub pane: Option<PaneRef>,
    pub observed_at: SystemTime,
    pub source: EvidenceSource,
}

/// How a pane was bound, in the order the evidence is asked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneSource {
    /// A provider-published `session:@window.%pane` handle.
    Published,
    /// The process's parent chain reached a `pane_pid`.
    Ancestry,
    /// The process's controlling tty is the pane's pty.
    Tty,
}

/// A resolved pane plus the evidence that bound it.
#[derive(Debug, Clone)]
pub struct PaneBinding {
    pub pane: PaneRef,
    pub source: PaneSource,
}

/// Where a claim's pane ended up after resolution and reconciliation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Placement {
    /// Bound; `source` says by which evidence.
    Bound(PaneSource),
    /// Reasoned `Unknown`: nothing resolved, or evidence contradicted
    /// itself. The reason is carried for the evidence view.
    Unknown(String),
    /// A newer live claim owns the pane; this binding retired during
    /// ownership reconciliation rather than hanging on.
    Superseded,
    /// The claimed process is dead; a dead process owns no pane.
    Dead(String),
}

/// A claim with everything one refresh could say about it.
#[derive(Debug)]
pub struct ResolvedAttachment {
    pub attachment: LiveAttachment,
    /// Whether the claimed process instance still runs.
    pub liveness: Liveness,
    /// How `attachment.pane` came to be what it is.
    pub placement: Placement,
}

/// One refresh of runtime evidence: a whole-machine process table and the
/// merged pane inventory, taken together so pid, pane and tty claims are
/// judged against the same instant.
#[derive(Debug)]
pub struct Runtime {
    /// `None` when the process table could not be read: every liveness
    /// check then answers `Unverifiable` rather than reaping what it
    /// cannot see.
    pub processes: Option<ProcessTable>,
    pub panes: PaneInventory,
    /// When the observation was taken.
    pub observed_at: SystemTime,
}

impl Runtime {
    /// A refresh against this process's environment: sockets are discovered
    /// from the tmux socket dir and `$TMUX`.
    #[rustfmt::skip]
    pub fn observe() -> Runtime { Self::observe_in(std::env::var_os("TMUX_TMPDIR").as_deref(), std::env::var_os("TMUX").as_deref()) } // coverage: off - reads the ambient environment, which tests must not touch

    /// [`observe`] with the environment passed in.
    pub fn observe_in(tmux_tmpdir: Option<&OsStr>, tmux_env: Option<&OsStr>) -> Runtime {
        Runtime {
            processes: ProcessTable::snapshot().ok(),
            panes: PaneInventory::discover_in(tmux_tmpdir, tmux_env),
            observed_at: SystemTime::now(),
        }
    }

    /// A refresh restricted to named server sockets - a caller that already
    /// knows its servers does not need discovery.
    pub fn observe_over(sockets: &[std::path::PathBuf]) -> Runtime {
        Runtime {
            processes: ProcessTable::snapshot().ok(),
            panes: PaneInventory::collect(sockets),
            observed_at: SystemTime::now(),
        }
    }

    /// Whether `claim`'s process is still the running instance.
    pub fn liveness(&self, claim: &ProcessClaim) -> Liveness {
        match &self.processes {
            Some(table) => table.is_live(&claim.instance(), claim.expected_exe.as_deref()),
            None => Liveness::Unverifiable("no process table could be read".to_owned()),
        }
    }

    /// L3 pane resolution for one pid: published handle, parent ancestry
    /// to a `pane_pid`, controlling tty, then `Unknown`. A published handle
    /// that names one live pane binds - unless derived evidence names a
    /// different one, which is a contradiction and fails closed.
    pub fn resolve_pane(&self, pid: u32, published: Option<&str>) -> Evidence<PaneBinding> {
        // Derived evidence: ancestry first, the controlling tty as the
        // fallback for a process daemonized out of its pane's tree. Both
        // answering and disagreeing is a contradiction however it arose.
        let ancestry = self.ancestry_pane(pid);
        let tty = self.tty_pane(pid);
        let derived = match (ancestry, tty) {
            (Some(a), Some(t)) if a.pane != t.pane => {
                return Evidence::Unknown(format!(
                    "ancestry resolves pid {pid} to {} but its tty belongs to {}",
                    a.pane, t.pane
                ));
            }
            (Some(a), _) => Some(a),
            (None, t) => t,
        };

        let handle = published.and_then(PublishedHandle::parse);
        // Only a handle carrying a pane id resolves a pane; a window or
        // session alone narrows the match, never pinpoints it.
        if let Some(candidates) = handle
            .filter(|h| h.pane.is_some())
            .map(|h| self.panes.matching(&h))
        {
            // Zero candidates: a stale handle naming no live pane falls
            // through to derived evidence. Two or more: ambiguous, falls
            // through the same way.
            if let [pane] = candidates.as_slice() {
                let reference = PaneRef {
                    socket: pane.socket.clone(),
                    pane: pane.id.clone(),
                };
                return match &derived {
                    Some(d) if d.pane != reference => Evidence::Unknown(format!(
                        "published handle names {} but {} resolves to {}",
                        reference,
                        if matches!(d.source, PaneSource::Tty) {
                            "the controlling tty"
                        } else {
                            "ancestry"
                        },
                        d.pane
                    )),
                    _ => Evidence::Known(PaneBinding {
                        pane: reference,
                        source: PaneSource::Published,
                    }),
                };
            }
        }
        match derived {
            Some(binding) => Evidence::Known(binding),
            None => Evidence::Unknown(format!(
                "pid {pid} has no resolvable published handle, ancestry or controlling tty"
            )),
        }
    }

    /// The pane whose `pane_pid` is `pid` or one of its ancestors.
    fn ancestry_pane(&self, pid: u32) -> Option<PaneBinding> {
        let table = self.processes.as_ref()?;
        for ancestor in table.ancestors(pid) {
            if let Some(pane) = self.panes.by_pane_pid(ancestor) {
                return Some(PaneBinding {
                    pane: PaneRef {
                        socket: pane.socket.clone(),
                        pane: pane.id.clone(),
                    },
                    source: PaneSource::Ancestry,
                });
            }
        }
        None
    }

    /// The pane sharing `pid`'s controlling terminal - the fallback for a
    /// process that lost its ancestry. A tty shared by several panes
    /// resolves nothing.
    fn tty_pane(&self, pid: u32) -> Option<PaneBinding> {
        let table = self.processes.as_ref()?;
        let tty = table.get(pid)?.tty.as_ref()?;
        let panes = self.panes.by_tty(tty);
        match panes.as_slice() {
            [pane] => Some(PaneBinding {
                pane: PaneRef {
                    socket: pane.socket.clone(),
                    pane: pane.id.clone(),
                },
                source: PaneSource::Tty,
            }),
            _ => None,
        }
    }

    /// Resolve each claim into a [`LiveAttachment`], then reconcile pane
    /// ownership: a pane hosts at most one current attachment, so where two
    /// live claims land on the same pane the newest observation owns it,
    /// the superseded binding retires, and claims that cannot be ordered
    /// are contradictory - every one `Unknown` rather than a guess.
    pub fn resolve_attachments(&self, claims: &[ProcessClaim]) -> Vec<ResolvedAttachment> {
        let mut resolved: Vec<ResolvedAttachment> = claims
            .iter()
            .map(|claim| {
                let liveness = self.liveness(claim);
                let (pane, placement) = match &liveness {
                    Liveness::Dead(reason) => (None, Placement::Dead(reason.clone())),
                    _ => match self.resolve_pane(claim.pid, claim.published_pane.as_deref()) {
                        Evidence::Known(binding) => {
                            (Some(binding.pane), Placement::Bound(binding.source))
                        }
                        Evidence::Unknown(reason) => (None, Placement::Unknown(reason)),
                    },
                };
                ResolvedAttachment {
                    attachment: LiveAttachment {
                        session: claim.session.clone(),
                        process: claim.instance(),
                        pane,
                        observed_at: claim.observed_at,
                        source: claim.source,
                    },
                    liveness,
                    placement,
                }
            })
            .collect();

        // Per-pane ownership: the newest bound claim wins. Older claims on
        // the same pane retire; unordered claims contradict each other.
        for pane in panes_of(&resolved) {
            let mut claimants: Vec<usize> = resolved
                .iter()
                .enumerate()
                .filter(|(_, r)| {
                    matches!(r.placement, Placement::Bound(_))
                        && r.attachment.pane.as_ref() == Some(&pane)
                })
                .map(|(i, _)| i)
                .collect();
            if claimants.len() < 2 {
                continue;
            }
            let newest = claimants
                .iter()
                .map(|&i| resolved[i].attachment.observed_at)
                .max()
                .unwrap_or(SystemTime::UNIX_EPOCH);
            let winners: Vec<usize> = claimants
                .iter()
                .copied()
                .filter(|&i| resolved[i].attachment.observed_at == newest)
                .collect();
            let reason = format!(
                "{} live claims bind {} with no newer observation to order them",
                winners.len(),
                pane
            );
            for i in claimants.drain(..) {
                if winners.len() > 1 || !winners.contains(&i) {
                    resolved[i].attachment.pane = None;
                    resolved[i].placement = if winners.len() == 1 {
                        Placement::Superseded
                    } else {
                        Placement::Unknown(reason.clone())
                    };
                }
            }
        }
        resolved
    }
}

/// The distinct panes any resolved attachment is bound to.
fn panes_of(resolved: &[ResolvedAttachment]) -> Vec<PaneRef> {
    let mut panes: Vec<PaneRef> = resolved
        .iter()
        .filter_map(|r| r.attachment.pane.clone())
        .collect();
    panes.sort();
    panes.dedup();
    panes
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::ProcessRow;
    use crate::tmux::{Pane, PaneId, SessionId, WindowId};
    use std::path::PathBuf;
    use std::time::Duration;

    /// The `matches!`-free way to state a placement, so both edges of the
    /// check are evaluated somewhere in this module.
    fn placement_of(r: &ResolvedAttachment) -> &'static str {
        match &r.placement {
            Placement::Bound(_) => "bound",
            Placement::Superseded => "superseded",
            Placement::Dead(_) => "dead",
            Placement::Unknown(_) => "unknown",
        }
    }

    fn liveness_of(l: &Liveness) -> &'static str {
        match l {
            Liveness::Instance => "instance",
            Liveness::PidOnly(_) => "pid-only",
            Liveness::Unverifiable(_) => "unverifiable",
            Liveness::Dead(_) => "dead",
        }
    }

    fn is_unknown<T>(evidence: &Evidence<T>) -> bool {
        matches!(evidence, Evidence::Unknown(_))
    }

    fn fake_pane(socket: &str, id: &str, pid: u32, tty: &str) -> Pane {
        Pane {
            socket: PathBuf::from(socket),
            id: PaneId::parse(id).unwrap(),
            window: WindowId::parse("@1").unwrap(),
            session: SessionId::parse("$1").unwrap(),
            session_name: "s".to_owned(),
            pid,
            command: "sh".to_owned(),
            cwd: None,
            tty: Some(tty.to_owned()),
            active: true,
            last: false,
            window_active: true,
            window_activity: None,
            session_attached: 1,
            wt_adminid: None,
            wt_handle: None,
        }
    }

    fn inventory(panes: Vec<Pane>) -> PaneInventory {
        PaneInventory {
            panes,
            servers: Vec::new(),
            warnings: Vec::new(),
        }
    }

    fn fake_row(pid: u32, ppid: u32, tty: Option<&str>) -> ProcessRow {
        ProcessRow {
            pid,
            ppid,
            start: ProcessStart::At(1_700_000_000),
            exe: Some("worker".to_owned()),
            tty: tty.map(str::to_owned),
            state: 'S',
        }
    }

    fn runtime(panes: Vec<Pane>, rows: Vec<ProcessRow>) -> Runtime {
        Runtime {
            processes: Some(ProcessTable::from_rows(rows)),
            panes: inventory(panes),
            observed_at: SystemTime::now(),
        }
    }

    fn bare_claim(pid: u32) -> ProcessClaim {
        ProcessClaim {
            session: None,
            pid,
            pid_start: ProcessStart::Unavailable,
            expected_exe: None,
            published_pane: None,
            source: EvidenceSource::Derived,
            observed_at: SystemTime::now(),
        }
    }

    #[test]
    fn disagreeing_derived_evidence_is_a_contradiction() {
        // pid 11 sits in pane %1's tree but holds pane %2's tty: two
        // honest sources that disagree, and the binding fails closed.
        let rt = runtime(
            vec![
                fake_pane("/sock/a", "%1", 10, "/dev/ttyA"),
                fake_pane("/sock/b", "%2", 20, "/dev/ttyB"),
            ],
            vec![
                fake_row(10, 1, Some("/dev/ttyA")),
                fake_row(11, 10, Some("/dev/ttyB")),
            ],
        );
        let binding = rt.resolve_pane(11, None);
        assert!(is_unknown(&binding));
        let Evidence::Unknown(reason) = binding else {
            panic!("a contradiction is Unknown") // coverage: off - failure path of the assertion above
        };
        assert!(reason.contains("ancestry"), "{reason}");
        assert!(reason.contains("%2"), "{reason}");
    }

    #[test]
    fn a_published_handle_against_the_tty_is_a_contradiction() {
        // The handle names %1, but the claim's tty belongs to %2's pane.
        let rt = runtime(
            vec![
                fake_pane("/sock/a", "%1", 10, "/dev/ttyA"),
                fake_pane("/sock/b", "%2", 20, "/dev/ttyB"),
            ],
            vec![fake_row(11, 1, Some("/dev/ttyB"))],
        );
        let binding = rt.resolve_pane(11, Some("%1"));
        let Evidence::Unknown(reason) = binding else {
            panic!("a contradiction is Unknown") // coverage: off - miss edge is the assertion failing
        };
        assert!(reason.contains("the controlling tty"), "{reason}");
    }

    #[test]
    fn an_evidenceless_pid_is_unknown() {
        // No tty and no reachable pane_pid: nothing to resolve with.
        let rt = runtime(
            vec![fake_pane("/sock/a", "%1", 10, "/dev/ttyA")],
            vec![fake_row(10, 1, Some("/dev/ttyA")), fake_row(11, 999, None)],
        );
        assert!(is_unknown(&rt.resolve_pane(11, None)));
        // A pid not in the table at all is just as evidenceless, while a
        // pid under a pane_pid does resolve - the Known edge of the same
        // check.
        assert!(is_unknown(&rt.resolve_pane(999, None)));
        assert!(!is_unknown(&rt.resolve_pane(10, None)));

        // A tty shared by two panes is ambiguous and resolves nothing.
        let rt = runtime(
            vec![
                fake_pane("/sock/a", "%1", 10, "/dev/ttyS"),
                fake_pane("/sock/b", "%2", 20, "/dev/ttyS"),
            ],
            vec![fake_row(11, 999, Some("/dev/ttyS"))],
        );
        assert!(is_unknown(&rt.resolve_pane(11, None)));
    }

    #[test]
    fn the_newest_observation_owns_the_pane() {
        // Two live claims land on %1: the newer binds, the older retires.
        let rt = runtime(
            vec![fake_pane("/sock/a", "%1", 10, "/dev/ttyA")],
            vec![
                fake_row(10, 1, Some("/dev/ttyA")),
                fake_row(11, 10, Some("/dev/ttyA")),
                fake_row(12, 10, Some("/dev/ttyA")),
            ],
        );
        let at = SystemTime::now();
        let claims = [
            bare_claim(11),
            ProcessClaim {
                observed_at: at + Duration::from_secs(60),
                ..bare_claim(12)
            },
        ];
        let resolved = rt.resolve_attachments(&claims);
        assert_eq!(placement_of(&resolved[0]), "superseded");
        assert_eq!(placement_of(&resolved[1]), "bound");
        assert_eq!(liveness_of(&resolved[1].liveness), "pid-only");

        // A claim carrying the instance's own start time validates as the
        // instance itself.
        let claim = ProcessClaim {
            pid_start: ProcessStart::At(1_700_000_000),
            ..bare_claim(11)
        };
        assert_eq!(liveness_of(&rt.liveness(&claim)), "instance");
    }

    #[test]
    fn a_lone_bound_claim_keeps_its_pane() {
        // One claim resolves, one does not: the pane's single claimant is
        // not contested.
        let rt = runtime(
            vec![fake_pane("/sock/a", "%1", 10, "/dev/ttyA")],
            vec![
                fake_row(10, 1, Some("/dev/ttyA")),
                fake_row(11, 10, Some("/dev/ttyA")),
                fake_row(12, 999, None),
            ],
        );
        let resolved = rt.resolve_attachments(&[bare_claim(11), bare_claim(12)]);
        assert_eq!(placement_of(&resolved[0]), "bound");
        assert_eq!(
            resolved[0].attachment.pane.as_ref().unwrap().pane.as_str(),
            "%1"
        );
        assert_eq!(placement_of(&resolved[1]), "unknown");
    }

    #[test]
    fn a_dead_claim_binds_no_pane() {
        let runtime = Runtime {
            processes: Some(ProcessTable::snapshot().unwrap()),
            panes: inventory(vec![fake_pane("/sock/a", "%1", 1, "/dev/tty1")]),
            observed_at: SystemTime::now(),
        };
        let claims = [ProcessClaim {
            session: Some(AgentSessionKey {
                provider: Provider::Claude,
                session_id: "x".to_owned(),
            }),
            pid: 4_000_000,
            pid_start: ProcessStart::Unavailable,
            expected_exe: None,
            published_pane: Some("%1".to_owned()),
            source: EvidenceSource::Published,
            observed_at: SystemTime::now(),
        }];
        let resolved = runtime.resolve_attachments(&claims);
        assert_eq!(placement_of(&resolved[0]), "dead");
        assert_eq!(liveness_of(&resolved[0].liveness), "dead");
        assert_eq!(resolved[0].attachment.pane, None);
        assert!(!resolved[0].liveness.may_be_live());
    }

    #[test]
    fn an_unreadable_table_verifies_nothing() {
        let runtime = Runtime {
            processes: None,
            panes: inventory(vec![]),
            observed_at: SystemTime::now(),
        };
        let claim = ProcessClaim {
            session: None,
            pid: 1,
            pid_start: ProcessStart::Unavailable,
            expected_exe: None,
            published_pane: None,
            source: EvidenceSource::Derived,
            observed_at: SystemTime::now(),
        };
        assert_eq!(liveness_of(&runtime.liveness(&claim)), "unverifiable");
        let resolved = runtime.resolve_attachments(&[claim]);
        assert_eq!(placement_of(&resolved[0]), "unknown");
    }
}
