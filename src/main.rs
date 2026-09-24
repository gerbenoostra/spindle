//! Argument dispatch and exit codes. All behaviour lives in the library.

use std::process::ExitCode;

use pico_args::Arguments;

/// A wrong invocation: loud, so a guessed subcommand or a stale hook entry is
/// corrected rather than silently ignored.
const USAGE_ERROR: u8 = 2;

const HELP: &str = "\
agent-sessions - one dashboard for every agentic session and worktree

usage:
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

    // Subcommands arrive together with the behaviour behind them; anything
    // that has not shipped yet - `list`, `hook`, `register`, `doctor`, the
    // dashboard itself - is a usage error here, never a silent no-op.
    let free = pargs
        .finish()
        .into_iter()
        .map(|arg| {
            arg.into_string()
                .map_err(|_| "argument is not valid UTF-8".to_owned())
        })
        .collect::<Result<Vec<_>, _>>()?;
    Err(match free.as_slice() {
        [] => "no command given".to_owned(),
        _ => format!("unexpected arguments: {}", free.join(" ")),
    })
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
