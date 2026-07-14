//! termctl CLI 入口。
//!
//! 业务实现放在模块中，main 只负责参数解析、脱敏错误输出和退出码。

mod cli;
mod client;
mod crypto;
mod error;
mod state;

use std::ffi::OsStr;

use clap::Parser;

#[tokio::main]
async fn main() {
    install_rustls_crypto_provider();

    let cli = match cli::Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) => {
            let json = args_request_json(std::env::args_os());
            let exit_code = error.exit_code();
            if json {
                eprintln!("{}", clap_parse_error_message(&error, true));
            } else {
                // 中文注释：非 JSON 模式继续交给 clap 打印原生帮助/用法，尽量保持人类输出兼容。
                let _ = error.print();
            }
            std::process::exit(exit_code);
        }
    };

    if let Err(error) = cli::run(cli).await {
        eprintln!("{}", error.user_message());
        std::process::exit(error.exit_code());
    }
}

fn install_rustls_crypto_provider() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
}

fn args_request_json<I, S>(args: I) -> bool
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    args.into_iter()
        .skip(1)
        .take_while(|arg| arg.as_ref() != OsStr::new("--"))
        .any(|arg| arg.as_ref() == OsStr::new("--json"))
}

fn clap_parse_error_message(error: &clap::Error, json: bool) -> String {
    if json {
        return serde_json::json!({
            "error": {
                "code": "cli_parse_error",
                "message": error.to_string().trim(),
            }
        })
        .to_string();
    }

    error.to_string()
}

#[cfg(test)]
mod tests {
    use clap::error::ErrorKind;

    use super::*;

    #[test]
    fn detects_json_flag_before_clap_parse_finishes() {
        assert!(args_request_json(["termctl", "--json", "bogus"]));
        assert!(args_request_json(["termctl", "list", "--json"]));
        assert!(!args_request_json(["termctl", "new", "--", "--json"]));
    }

    #[test]
    fn clap_parse_error_can_be_rendered_as_json() {
        let error = clap::Error::raw(ErrorKind::UnknownArgument, "unexpected --bad");
        let rendered = clap_parse_error_message(&error, true);
        let value: serde_json::Value =
            serde_json::from_str(&rendered).expect("JSON clap error should parse");

        assert_eq!(value["error"]["code"], "cli_parse_error");
        assert!(
            value["error"]["message"]
                .as_str()
                .expect("message should be a string")
                .contains("--bad")
        );
    }
}
