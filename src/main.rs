mod config;
mod editor;
mod repository;

use std::{env, path::Path, process::ExitCode};

use chrono::Utc;
use clap::{Parser, Subcommand};
use repository::Repository;

#[derive(Parser)]
#[command(version, about = "用 Git 分支管理 Rust 试验")]
struct Cli {
    #[command(subcommand)]
    command: Action,
}

#[derive(Subcommand)]
enum Action {
    /// 初始化 CARGOPLAY_HOME 下的配置和 play 仓库
    Init,
    /// 备份上一次试验，用 cargo init 创建新试验并打开 src/main.rs
    New {
        /// 依赖列表，支持 crate 名和 crate@版本
        #[arg(short = 'd', long, value_name = "依赖", num_args = 1..)]
        deps: Vec<String>,
    },
    /// 提交当前试验的改动
    Backup,
}

type Result<T> = std::result::Result<T, String>;

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = Repository::open(
        env::var_os("CARGOPLAY_HOME"),
        matches!(&cli.command, Action::Init | Action::New { .. }),
        config::prepare_global,
    )
    .and_then(|repo| run(cli.command, repo, editor::open));
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("错误：{error}");
            ExitCode::FAILURE
        }
    }
}

fn run(action: Action, repo: Repository, open: impl FnOnce(&Path) -> Result<()>) -> Result<()> {
    let now = Utc::now();
    let timestamp = now.format("%Y-%m-%dT%H:%M:%SZ").to_string();
    match action {
        Action::Init => println!("试验仓库已就绪：{}", repo.root.display()),
        Action::Backup => {
            repo.backup(&timestamp)?;
        }
        Action::New { deps } => {
            repo.backup(&timestamp)?;
            if repo.has_changes()? {
                return Err("备份后工作区仍有改动，请运行 git status 检查后重试。".into());
            }
            let branch = repo.new_branch(&now.format("play/%Y%m%d-%H%M%SZ").to_string())?;
            let recover = |error| {
                format!(
                    "{error}\n当前仍在 {branch}。请在 {} 中运行 git status 检查现场，修复后可运行 cargo-play backup 保存改动。",
                    repo.root.display()
                )
            };
            repo.init_project().map_err(recover)?;
            println!("已创建试验分支 {branch}");
            if !deps.is_empty() {
                repo.add_dependencies(&deps).map_err(recover)?;
                println!("已提交依赖 {}", deps.join(", "));
            }

            let main = repo.root.join("src").join("main.rs");
            drop(repo);
            if let Err(error) = open(&main) {
                eprintln!("警告：{error}\n请手动打开 {}", main.display());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
