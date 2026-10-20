use std::fmt;

pub const MANIFEST: u8 = 1;
pub const AUTH: u8 = 2;
pub const NETWORK: u8 = 3;
pub const HASH: u8 = 4;
pub const IO: u8 = 5;

#[derive(Debug)]
pub struct Error {
    pub code: u8,
    pub msg: String,
    /// The next step, printed on its own line under the error.
    pub hint: Option<String>,
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub fn new(code: u8, msg: impl Into<String>) -> Self {
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

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::new(IO, e.to_string())
    }
}

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
}
