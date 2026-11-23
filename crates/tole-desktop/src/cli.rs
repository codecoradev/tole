//! The command line of the `tole-desktop` binary: `--help` and `--version`,
//! nothing else. The folder the app starts in is the folder it is run from.

/// What the command line asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Open the window.
    Run,
    Help,
    Version,
}

/// The text of `--help`.
pub const USAGE: &str = "\
usage: tole-desktop [--help | --version]

Opens the Tole window. The folder you run it from is the first working
folder, and Tole is started in the background for it (or an already running
one is used). Closing the window leaves Tole running.";

/// The text of `--version`.
pub fn version() -> String {
    format!("tole-desktop {}", env!("CARGO_PKG_VERSION"))
}

/// Read the arguments after the program name. `Err` is what to tell the owner.
pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Command, String> {
    let mut args = args.into_iter();
    let Some(first) = args.next() else {
        return Ok(Command::Run);
    };
    if let Some(extra) = args.next() {
        return Err(format!("unexpected argument `{extra}`"));
    }
    match first.as_str() {
        "-h" | "--help" => Ok(Command::Help),
        "-V" | "--version" => Ok(Command::Version),
        other => Err(format!("unknown argument `{other}`")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_of(args: &[&str]) -> Result<Command, String> {
        parse(args.iter().map(|a| (*a).to_owned()))
    }

    #[test]
    fn no_argument_opens_the_window() {
        assert_eq!(parse_of(&[]), Ok(Command::Run));
    }

    #[test]
    fn help_and_version_in_both_spellings() {
        assert_eq!(parse_of(&["--help"]), Ok(Command::Help));
        assert_eq!(parse_of(&["-h"]), Ok(Command::Help));
        assert_eq!(parse_of(&["--version"]), Ok(Command::Version));
        assert_eq!(parse_of(&["-V"]), Ok(Command::Version));
        assert!(version().starts_with("tole-desktop 0."));
    }

    #[test]
    fn anything_else_is_refused_by_name() {
        assert!(parse_of(&["--nope"]).unwrap_err().contains("--nope"));
        assert!(parse_of(&["--help", "x"]).unwrap_err().contains("`x`"));
        assert!(parse_of(&["folder"]).is_err());
    }
}
