//! bento's exit codes (contracts.md, Exit codes) and the error that carries one.
//! Any other error ends a command with 125.

use std::fmt;

pub const USAGE: u8 = 120;
pub const NO_BOX: u8 = 121;
pub const NOT_RUNNING: u8 = 122;
pub const EXISTS: u8 = 123;
pub const OTHER: u8 = 125;

/// An error with the exit code it ends bento with.
#[derive(Debug)]
pub struct Fail {
    pub code: u8,
    pub msg: String,
}

impl fmt::Display for Fail {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.msg)
    }
}

impl std::error::Error for Fail {}

pub fn fail(code: u8, msg: impl Into<String>) -> anyhow::Error {
    Fail {
        code,
        msg: msg.into(),
    }
    .into()
}

pub fn usage(msg: impl Into<String>) -> anyhow::Error {
    fail(USAGE, msg)
}

pub fn no_box(name: &str) -> anyhow::Error {
    fail(NO_BOX, format!("no box '{name}'"))
}

/// The exit code for an error: its own when it carries one (anywhere in its
/// chain), else 125.
pub fn code_of(err: &anyhow::Error) -> u8 {
    err.chain()
        .find_map(|e| e.downcast_ref::<Fail>())
        .map_or(OTHER, |f| f.code)
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Context as _;

    #[test]
    fn codes_survive_context() {
        let err = Err::<(), _>(no_box("x")).context("starting").unwrap_err();
        assert_eq!(code_of(&err), NO_BOX);
        assert_eq!(code_of(&anyhow::anyhow!("boom")), OTHER);
        assert_eq!(code_of(&usage("bad")), USAGE);
    }
}
