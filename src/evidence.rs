//! A collector either proves a fact or says why it could not.
//!
//! `Unknown` is a value, never a default: every unproven field carries the
//! reason it could not be read, so a verdict can fail closed and a view can
//! show `?` with an explanation instead of a plausible guess.

/// A read that produced a value, or the reason it did not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Evidence<T> {
    Known(T),
    Unknown(String),
}

impl<T> Evidence<T> {
    pub fn is_known(&self) -> bool {
        matches!(self, Evidence::Known(_))
    }

    /// The carried value, or `None` when unproven.
    pub fn known(&self) -> Option<&T> {
        match self {
            Evidence::Known(value) => Some(value),
            Evidence::Unknown(_) => None,
        }
    }

    /// Why the value could not be proven, when it could not.
    pub fn reason(&self) -> Option<&str> {
        match self {
            Evidence::Known(_) => None,
            Evidence::Unknown(reason) => Some(reason),
        }
    }

    /// Map the value, keeping an `Unknown` reason untouched.
    pub fn map<U>(self, f: impl FnOnce(T) -> U) -> Evidence<U> {
        match self {
            Evidence::Known(value) => Evidence::Known(f(value)),
            Evidence::Unknown(reason) => Evidence::Unknown(reason),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_carries_its_reason_and_maps_through() {
        let unknown: Evidence<u32> = Evidence::Unknown("no remote".to_owned());
        assert!(!unknown.is_known());
        assert_eq!(unknown.reason(), Some("no remote"));
        assert!(unknown.known().is_none());
        assert_eq!(
            unknown.map(|n| n + 1), // coverage: off - the closure never runs on Unknown
            Evidence::Unknown("no remote".to_owned())
        );

        let known = Evidence::Known(1u32);
        assert!(known.is_known());
        assert_eq!(known.reason(), None);
        assert_eq!(known.known(), Some(&1));
        assert_eq!(known.map(|n| n + 1), Evidence::Known(2));
    }
}
