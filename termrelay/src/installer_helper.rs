use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{self, Read, Write};

use thiserror::Error;

#[derive(Debug, Error)]
pub enum InstallerHelperError {
    #[error("unknown internal termrelay installer helper operation")]
    UnknownOperation,
    #[error("invalid arguments for internal termrelay installer helper")]
    InvalidArguments,
    #[error("failed to obtain secure randomness for relay setup token")]
    Randomness(#[source] io::Error),
    #[error("failed to write internal termrelay installer helper output")]
    Output(#[source] io::Error),
}

pub fn run(request: terminstall::InternalHelperRequest) -> Result<(), InstallerHelperError> {
    let output = dispatch(request.operation(), request.args())?;
    io::stdout()
        .lock()
        .write_all(output.as_bytes())
        .map_err(InstallerHelperError::Output)
}

fn dispatch(operation: &OsStr, args: &[OsString]) -> Result<String, InstallerHelperError> {
    match operation.to_str() {
        Some("self-check") => {
            if !args.is_empty() {
                return Err(InstallerHelperError::InvalidArguments);
            }
            Ok(String::new())
        }
        Some("generate-secret-token") => {
            if !args.is_empty() {
                return Err(InstallerHelperError::InvalidArguments);
            }
            generate_secret_token()
        }
        _ => Err(InstallerHelperError::UnknownOperation),
    }
}

fn generate_secret_token() -> Result<String, InstallerHelperError> {
    let mut random = [0_u8; 32];
    File::open("/dev/urandom")
        .and_then(|mut source| source.read_exact(&mut random))
        .map_err(InstallerHelperError::Randomness)?;
    let mut token = String::with_capacity(65);
    for byte in random {
        use std::fmt::Write as _;
        write!(&mut token, "{byte:02x}").expect("writing to a String cannot fail");
    }
    token.push('\n');
    Ok(token)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_secret_token_is_random_and_shell_safe() {
        let first = dispatch(OsStr::new("generate-secret-token"), &[]).unwrap();
        let second = dispatch(OsStr::new("generate-secret-token"), &[]).unwrap();

        assert_ne!(first, second);
        for token in [first.trim_end(), second.trim_end()] {
            assert_eq!(token.len(), 64);
            assert!(token.bytes().all(|byte| byte.is_ascii_hexdigit()));
        }
    }

    #[test]
    fn helper_dispatch_rejects_arguments_and_unknown_operations() {
        assert_eq!(dispatch(OsStr::new("self-check"), &[]).unwrap(), "");
        assert!(matches!(
            dispatch(OsStr::new("self-check"), &[OsString::from("unexpected")]),
            Err(InstallerHelperError::InvalidArguments)
        ));
        assert!(matches!(
            dispatch(
                OsStr::new("generate-secret-token"),
                &[OsString::from("unexpected")]
            ),
            Err(InstallerHelperError::InvalidArguments)
        ));
        assert!(matches!(
            dispatch(OsStr::new("other"), &[]),
            Err(InstallerHelperError::UnknownOperation)
        ));
    }
}
