//! Argument dispatch and exit codes. All behaviour lives in the library.

use std::process::ExitCode;

use pico_args::Arguments;

/// A wrong invocation: loud, so a guessed subcommand or a stale hook entry is
/// corrected rather than silently ignored.
const USAGE_ERROR: u8 = 2;

const HELP: &str = "\
agent-sessions - one dashboard for every agentic session and worktree

usage:
  agent-sessions              the dashboard (needs a terminal)
  agent-sessions list --json  the complete snapshot, unfiltered
  agent-sessions --version    version, and the executable that is actually running
  agent-sessions --help       this text
";

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(message) => usage_error(&message),
    }
}

fn run() -> Result<ExitCode, String> {
    let mut pargs = Arguments::from_vec(std::env::args_os().skip(1).collect());

    if pargs.contains(["-h", "--help"]) {
        print!("{HELP}");
        return Ok(ExitCode::SUCCESS);
    }
    if pargs.contains(["-V", "--version"]) {
        println!("{}", version());
        return Ok(ExitCode::SUCCESS);
    }

    let json = pargs.contains("--json");
    let free = pargs
        .finish()
        .into_iter()
        .map(|arg| {
            arg.into_string()
                .map_err(|_| "argument is not valid UTF-8".to_owned())
        })
        .collect::<Result<Vec<_>, _>>()?;
    // Bare `agent-sessions` is the dashboard itself. Subcommands arrive
    // together with the behaviour behind them; anything that has not shipped
    // yet - `hook`, `register`, `doctor` - is a usage error, never a silent
    // no-op.
    match free.as_slice() {
        [] if !json => match agent_sessions::tui::tui() {
            Ok(()) => Ok(ExitCode::SUCCESS), // coverage: off - tui() only succeeds with a real terminal
            Err(e) => {
                eprintln!("agent-sessions: {e}");
                Ok(ExitCode::FAILURE)
            }
        },
        [cmd] if cmd == "list" && json => match agent_sessions::tui::list_json() {
            Ok(json) => {
                println!("{json}");
                Ok(ExitCode::SUCCESS)
            }
            Err(e) => {
                eprintln!("agent-sessions: {e}");
                Ok(ExitCode::FAILURE)
            }
        },
        // `--json` was already consumed out of `free`; put it back in the
        // complaint when it is the argument being rejected.
        _ => Err(format!(
            "unexpected arguments: {}",
            free.iter()
                .cloned()
                .chain(json.then(|| "--json".to_owned()))
                .collect::<Vec<_>>()
                .join(" ")
        )),
    }
}

fn version() -> String {
    // A dev checkout deliberately shadows the installed binary through PATH,
    // and a shadow you cannot see is a shadow that wastes an afternoon.
    agent_sessions::version::text(std::env::current_exe())
}

fn usage_error(message: &str) -> ExitCode {
    eprintln!("agent-sessions: {message}");
    eprint!("{HELP}");
    ExitCode::from(USAGE_ERROR)
}
