//! termctl CLI 入口。
//!
//! 业务实现放在模块中，main 只负责参数解析、脱敏错误输出和退出码。

mod cli;
mod client;
mod crypto;
mod error;
mod state;

use clap::Parser;

#[tokio::main]
async fn main() {
    let cli = cli::Cli::parse();

    if let Err(error) = cli::run(cli).await {
        eprintln!("{}", error.user_message());
        std::process::exit(error.exit_code());
    }
}
