use std::env;
use std::ffi::OsString;
use std::net::{AddrParseError, SocketAddr};

use thiserror::Error;

const DEFAULT_LISTEN: &str = "127.0.0.1:8080";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Args {
    pub listen: SocketAddr,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ArgsError {
    #[error("unknown argument: {0}")]
    UnknownArgument(String),
    #[error("{0} requires a value")]
    MissingValue(&'static str),
    #[error("invalid listen address")]
    InvalidListenAddress(#[from] AddrParseError),
}

impl Args {
    pub fn from_env() -> Result<Self, ArgsError> {
        Self::parse_from(env::args_os())
    }

    pub fn parse_from<I, S>(args: I) -> Result<Self, ArgsError>
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        let mut listen = DEFAULT_LISTEN.parse()?;
        let mut args = args.into_iter().map(Into::into);

        // 第一个参数是程序名；不要求存在，方便单元测试直接传空迭代器。
        let _program = args.next();

        while let Some(arg) = args.next() {
            let arg = arg.to_string_lossy().into_owned();
            match arg.as_str() {
                "--listen" | "-l" => {
                    let value = args.next().ok_or(ArgsError::MissingValue("--listen"))?;
                    listen = value.to_string_lossy().parse()?;
                }
                other => return Err(ArgsError::UnknownArgument(other.to_owned())),
            }
        }

        Ok(Self { listen })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uses_localhost_default_listen_address() {
        let args = Args::parse_from(["termrelay"]).unwrap();

        assert_eq!(args.listen, "127.0.0.1:8080".parse().unwrap());
    }

    #[test]
    fn parses_listen_argument() {
        let args = Args::parse_from(["termrelay", "--listen", "127.0.0.1:9000"]).unwrap();

        assert_eq!(args.listen, "127.0.0.1:9000".parse().unwrap());
    }

    #[test]
    fn rejects_missing_listen_value() {
        let error = Args::parse_from(["termrelay", "--listen"]).unwrap_err();

        assert_eq!(error, ArgsError::MissingValue("--listen"));
    }

    #[test]
    fn rejects_unknown_argument() {
        let error = Args::parse_from(["termrelay", "--auth"]).unwrap_err();

        assert_eq!(error, ArgsError::UnknownArgument("--auth".to_owned()));
    }
}
