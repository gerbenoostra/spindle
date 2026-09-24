//! Authored configuration: an optional TOML file, `AGENT_SESSIONS_*`
//! environment overrides and built-in defaults, in that precedence order.
//!
//! Nothing here is required to run; a missing file is the normal case and a
//! malformed one is reported, not fatal.

use std::path::PathBuf;
use std::time::Duration;

/// Unfinished work with no live process enters `Forgotten` after this much
/// time without meaningful activity.
const DEFAULT_FORGOTTEN_AFTER: Duration = Duration::from_secs(14 * 24 * 60 * 60);

#[derive(Debug, PartialEq, Eq)]
pub struct Config {
    pub forgotten_after: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            forgotten_after: DEFAULT_FORGOTTEN_AFTER,
        }
    }
}

/// The configuration that was resolved plus anything the user should hear
/// once. A warning never fails a load: the defaults stay in effect for the
/// parts that could not be read.
#[derive(Debug)]
pub struct Loaded {
    pub config: Config,
    pub warnings: Vec<String>,
}

impl Config {
    /// `$XDG_CONFIG_HOME/agent-sessions/config.toml`, falling back to
    /// `~/.config/agent-sessions/config.toml` when XDG is unset. `None` when
    /// neither variable can place it.
    pub fn path(env: &dyn Fn(&str) -> Option<String>) -> Option<PathBuf> {
        let base = env("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| env("HOME").map(|home| PathBuf::from(home).join(".config")))?;
        Some(base.join("agent-sessions").join("config.toml"))
    }

    /// `$XDG_STATE_HOME/agent-sessions/`, falling back to
    /// `~/.local/state/agent-sessions/`.
    pub fn state_dir(env: &dyn Fn(&str) -> Option<String>) -> Option<PathBuf> {
        let base = env("XDG_STATE_HOME")
            .map(PathBuf::from)
            .or_else(|| env("HOME").map(|home| PathBuf::from(home).join(".local").join("state")))?;
        Some(base.join("agent-sessions"))
    }

    /// Resolve against the real environment.
    pub fn load() -> Loaded {
        Self::load_with(&|name| std::env::var(name).ok())
    }

    /// Resolve against an injected environment, so tests never touch process
    /// state: file location, override variables and `$HOME` all come from the
    /// same closure.
    fn load_with(env: &dyn Fn(&str) -> Option<String>) -> Loaded {
        let mut warnings = Vec::new();
        let mut config = Config::default();

        if let Some(path) = Self::path(env) {
            match std::fs::read_to_string(&path) {
                Ok(text) => match Self::parse(&text) {
                    Ok(from_file) => config = from_file,
                    Err(reason) => {
                        warnings.push(format!("{}: {reason}; using defaults", path.display()))
                    }
                },
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => warnings.push(format!("{}: {error}; using defaults", path.display())),
            }
        }

        if let Some(value) = env("AGENT_SESSIONS_FORGOTTEN_AFTER") {
            match parse_duration(&value) {
                Ok(duration) => config.forgotten_after = duration,
                Err(reason) => warnings.push(format!(
                    "AGENT_SESSIONS_FORGOTTEN_AFTER={value:?}: {reason}; keeping {}",
                    format_duration(config.forgotten_after)
                )),
            }
        }

        Loaded { config, warnings }
    }

    fn parse(text: &str) -> Result<Config, String> {
        let table: toml::Table = text
            .parse()
            .map_err(|error: toml::de::Error| error.to_string())?;
        let mut config = Config::default();
        for (key, value) in &table {
            // Unknown keys are ignored so a file written for a newer
            // release still loads under this one.
            if key.as_str() == "forgotten_after" {
                let raw = value
                    .as_str()
                    .ok_or("`forgotten_after` must be a duration string like \"14d\"")?;
                config.forgotten_after = parse_duration(raw)?;
            }
        }
        Ok(config)
    }
}

/// `<n><unit>` with `s`, `m`, `h` or `d` - the one spelling the config file
/// and the `age:<duration>` list filter share.
pub fn parse_duration(text: &str) -> Result<Duration, String> {
    let text = text.trim();
    let invalid = || format!("`{text}` is not a duration: use <n><unit> with s, m, h or d");
    // The unit is a `char`, not a trailing byte: slicing off one byte panics
    // on a multi-byte tail, and malformed input is an error, never a panic.
    let unit = text.chars().next_back().ok_or_else(invalid)?;
    let multiplier = match unit {
        's' => 1,
        'm' => 60,
        'h' => 60 * 60,
        'd' => 24 * 60 * 60,
        _ => return Err(invalid()),
    };
    let digits = &text[..text.len() - unit.len_utf8()];
    let seconds = digits
        .parse::<u64>()
        .ok()
        .and_then(|n| n.checked_mul(multiplier))
        .ok_or_else(invalid)?;
    Ok(Duration::from_secs(seconds))
}

/// The inverse of [`parse_duration`], for messages that name a duration back.
fn format_duration(duration: Duration) -> String {
    let seconds = duration.as_secs();
    for (unit, name) in [(86400, "d"), (3600, "h"), (60, "m")] {
        if seconds >= unit && seconds % unit == 0 {
            return format!("{}{}", seconds / unit, name);
        }
    }
    format!("{seconds}s")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |name| map.get(name).cloned()
    }

    #[test]
    fn durations_parse_in_every_unit() {
        assert_eq!(parse_duration("30s"), Ok(Duration::from_secs(30)));
        assert_eq!(parse_duration("5m"), Ok(Duration::from_secs(300)));
        assert_eq!(parse_duration("2h"), Ok(Duration::from_secs(7200)));
        assert_eq!(parse_duration("14d"), Ok(DEFAULT_FORGOTTEN_AFTER));
        assert_eq!(parse_duration(" 7d "), Ok(Duration::from_secs(7 * 86400)));
    }

    #[test]
    fn durations_reject_everything_else() {
        for bad in [
            "", "14", "d", "1.5h", "-3d", "1w", "ten d", "10x",
            // A multi-byte tail used to panic on a non-char-boundary slice.
            "14€", "€", "10ü",
        ] {
            assert!(parse_duration(bad).is_err(), "{bad} parsed");
        }
        // Overflows report as invalid rather than wrapping.
        assert!(parse_duration("18446744073709551615d").is_err());
    }

    #[test]
    fn durations_format_back() {
        assert_eq!(format_duration(DEFAULT_FORGOTTEN_AFTER), "14d");
        assert_eq!(format_duration(Duration::from_secs(90)), "90s");
        assert_eq!(format_duration(Duration::from_secs(3600)), "1h");
        assert_eq!(format_duration(Duration::ZERO), "0s");
    }

    #[test]
    fn load_reads_the_process_environment() {
        // Nothing is asserted about the result: the machine's real
        // configuration decides it. The point is only that the real-env entry
        // point runs - `load_with` is what the tests drive.
        let _ = Config::load();
    }

    #[test]
    fn without_xdg_or_home_there_is_no_file_to_read() {
        let loaded = Config::load_with(&env(&[]));
        assert_eq!(loaded.config, Config::default());
        assert!(loaded.warnings.is_empty());
    }

    #[test]
    fn the_config_path_prefers_xdg_and_falls_back_to_home() {
        assert_eq!(
            Config::path(&env(&[("XDG_CONFIG_HOME", "/xdg"), ("HOME", "/home")])),
            Some(PathBuf::from("/xdg/agent-sessions/config.toml"))
        );
        assert_eq!(
            Config::path(&env(&[("HOME", "/home")])),
            Some(PathBuf::from("/home/.config/agent-sessions/config.toml"))
        );
        assert_eq!(Config::path(&env(&[])), None);
    }

    #[test]
    fn the_state_dir_prefers_xdg_and_falls_back_to_home() {
        assert_eq!(
            Config::state_dir(&env(&[("XDG_STATE_HOME", "/state")])),
            Some(PathBuf::from("/state/agent-sessions"))
        );
        assert_eq!(
            Config::state_dir(&env(&[("HOME", "/home")])),
            Some(PathBuf::from("/home/.local/state/agent-sessions"))
        );
        assert_eq!(Config::state_dir(&env(&[])), None);
    }

    /// A throwaway directory, unique per call because tests run in threads.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "agent-sessions-config-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&path).expect("a temp directory can be created");
            TempDir(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_missing_file_loads_defaults_silently() {
        let temp = TempDir::new();
        let loaded = Config::load_with(&env(&[(
            "XDG_CONFIG_HOME",
            temp.0.to_str().expect("temp paths are UTF-8"),
        )]));
        assert_eq!(loaded.config, Config::default());
        assert!(loaded.warnings.is_empty());
    }

    #[test]
    fn a_malformed_file_warns_and_keeps_defaults() {
        let temp = TempDir::new();
        let dir = temp.0.join("agent-sessions");
        std::fs::create_dir_all(&dir).expect("the config directory can be created");
        std::fs::write(dir.join("config.toml"), "forgotten_after = [oops").expect("writable");
        let loaded = Config::load_with(&env(&[(
            "XDG_CONFIG_HOME",
            temp.0.to_str().expect("temp paths are UTF-8"),
        )]));
        assert_eq!(loaded.config, Config::default());
        assert_eq!(loaded.warnings.len(), 1);
        assert!(loaded.warnings[0].contains("using defaults"));
    }

    #[test]
    fn an_unreadable_file_warns_and_keeps_defaults() {
        let temp = TempDir::new();
        let dir = temp.0.join("agent-sessions");
        std::fs::create_dir_all(&dir).expect("the config directory can be created");
        // A directory where the file should be reads as an error, not as missing.
        std::fs::create_dir(dir.join("config.toml")).expect("writable");
        let loaded = Config::load_with(&env(&[(
            "XDG_CONFIG_HOME",
            temp.0.to_str().expect("temp paths are UTF-8"),
        )]));
        assert_eq!(loaded.config, Config::default());
        assert_eq!(loaded.warnings.len(), 1);
    }

    #[test]
    fn the_file_overrides_defaults_and_environment_overrides_the_file() {
        let temp = TempDir::new();
        let dir = temp.0.join("agent-sessions");
        std::fs::create_dir_all(&dir).expect("the config directory can be created");
        std::fs::write(
            dir.join("config.toml"),
            "forgotten_after = \"30d\"\nunknown_future_key = 1\n",
        )
        .expect("writable");
        let home = temp.0.to_str().expect("temp paths are UTF-8").to_owned();
        let loaded = Config::load_with(&env(&[("XDG_CONFIG_HOME", &home)]));
        assert_eq!(
            loaded.config.forgotten_after,
            Duration::from_secs(30 * 86400)
        );
        assert!(loaded.warnings.is_empty());

        let loaded = Config::load_with(&env(&[
            ("XDG_CONFIG_HOME", &home),
            ("AGENT_SESSIONS_FORGOTTEN_AFTER", "2h"),
        ]));
        assert_eq!(loaded.config.forgotten_after, Duration::from_secs(7200));
    }

    #[test]
    fn a_bad_environment_override_warns_and_keeps_the_file_value() {
        let temp = TempDir::new();
        let dir = temp.0.join("agent-sessions");
        std::fs::create_dir_all(&dir).expect("the config directory can be created");
        std::fs::write(dir.join("config.toml"), "forgotten_after = \"30d\"\n").expect("writable");
        let home = temp.0.to_str().expect("temp paths are UTF-8").to_owned();
        let loaded = Config::load_with(&env(&[
            ("XDG_CONFIG_HOME", &home),
            ("AGENT_SESSIONS_FORGOTTEN_AFTER", "soon"),
        ]));
        assert_eq!(
            loaded.config.forgotten_after,
            Duration::from_secs(30 * 86400)
        );
        assert_eq!(loaded.warnings.len(), 1);
    }

    #[test]
    fn a_non_string_value_is_malformed() {
        assert!(Config::parse("forgotten_after = 14").is_err());
        assert!(Config::parse("forgotten_after = \"fortnight\"").is_err());
    }
}
