use std::io;
use std::path::PathBuf;

pub fn text(exe: io::Result<PathBuf>) -> String {
    let exe = exe
        .map(|path| path.display().to_string())
        .unwrap_or("<unknown>".to_owned());
    format!(
        "{} {}\nrunning from {}",
        env!("CARGO_PKG_NAME"),
        env!("CARGO_PKG_VERSION"),
        exe
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unknown_executable_is_reported() {
        let text = text(Err(io::Error::other("unavailable")));
        assert!(text.ends_with("running from <unknown>"));
    }
}
