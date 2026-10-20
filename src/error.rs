use std::fmt;

/// What went wrong, as the exit code reports it (README: exit codes).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Code {
    /// The command line; shares exit 1 with `Manifest`, as the README's exit code table says.
    Usage,
    /// skills.toml, skills.lock, a repository's metadata or a package's content.
    Manifest,
    Auth,
    Network,
    Hash,
    Io,
}

/// The spelling call sites use: `bail!(MANIFEST, …)`.
pub const USAGE: Code = Code::Usage;
pub const MANIFEST: Code = Code::Manifest;
pub const AUTH: Code = Code::Auth;
pub const NETWORK: Code = Code::Network;
pub const HASH: Code = Code::Hash;
pub const IO: Code = Code::Io;

impl Code {
    pub fn exit(self) -> i32 {
        match self {
            Code::Usage | Code::Manifest => 1,
            Code::Auth => 2,
            Code::Network => 3,
            Code::Hash => 4,
            Code::Io => 5,
        }
    }
}

#[derive(Debug)]
pub struct Error {
    pub code: Code,
    pub msg: String,
    /// The next step, printed on its own line under the error.
    pub hint: Option<String>,
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub fn new(code: Code, msg: impl Into<String>) -> Self {
        Error { code, msg: msg.into(), hint: None }
    }

    pub fn hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    /// Rewrites the message; the code and the hint stay.
    pub fn map_msg(mut self, f: impl FnOnce(String) -> String) -> Self {
        self.msg = f(self.msg);
        self
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.msg)
    }
}

/// No `source`: the cause is already part of `msg`, which is all the user sees.
/// Only where no path exists; a read or write of a known path names it with [`Context::ctx`].
impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::new(IO, e.to_string())
    }
}

/// Only where no path exists, as for [`std::io::Error`].
impl From<zip::result::ZipError> for Error {
    fn from(e: zip::result::ZipError) -> Self {
        Error::new(IO, format!("archive: {e}"))
    }
}

/// `bail!(CODE, "fmt", args…)` returns an `Error` with that exit code.
#[macro_export]
macro_rules! bail {
    ($code:expr, $($arg:tt)*) => {
        return Err($crate::error::Error::new($code, format!($($arg)*)))
    };
}

/// Prefixes the message with `what: `, keeping the code and the hint.
pub trait Context<T> {
    fn ctx(self, what: impl fmt::Display) -> Result<T>;
}

impl<T, E: Into<Error>> Context<T> for std::result::Result<T, E> {
    fn ctx(self, what: impl fmt::Display) -> Result<T> {
        self.map_err(|e| e.into().map_msg(|m| format!("{what}: {m}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes() {
        for (code, exit) in [(USAGE, 1), (MANIFEST, 1), (AUTH, 2), (NETWORK, 3), (HASH, 4), (IO, 5)] {
            assert_eq!(code.exit(), exit, "{code:?}");
        }
    }

    #[test]
    fn ctx_prefixes_the_message_and_keeps_code_and_hint() {
        let e = Err::<(), _>(Error::new(AUTH, "denied").hint("log in")).ctx("host").unwrap_err();
        assert_eq!((e.code, e.msg.as_str(), e.hint.as_deref()), (AUTH, "host: denied", Some("log in")));
        let io = std::io::Error::new(std::io::ErrorKind::NotFound, "gone");
        let e = Err::<(), _>(io).ctx("skills.toml").unwrap_err();
        assert_eq!((e.code, e.msg.as_str(), e.hint), (IO, "skills.toml: gone", None));
    }

    #[test]
    fn map_msg_keeps_code_and_hint() {
        let e = Error::new(HASH, "a").hint("h").map_msg(|m| m + "b");
        assert_eq!((e.code, e.msg.as_str(), e.hint.as_deref()), (HASH, "ab", Some("h")));
    }

    #[test]
    fn bail_returns_the_code() {
        fn f(n: u8) -> Result<()> {
            bail!(NETWORK, "n = {n}")
        }
        let e = f(7).unwrap_err();
        assert_eq!((e.code, e.msg.as_str()), (NETWORK, "n = 7"));
    }

    #[test]
    fn conversions_and_display() {
        let e: Error = std::io::Error::other("disk").into();
        assert_eq!((e.code, e.to_string().as_str()), (IO, "disk"));
        let e: Error = zip::result::ZipError::FileNotFound.into();
        assert_eq!(e.code, IO);
        assert!(e.msg.starts_with("archive: "), "{}", e.msg);
    }
}
