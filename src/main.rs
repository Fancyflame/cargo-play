mod config;
mod editor;
mod repository;

use std::{env, ffi::OsString, path::Path, process::ExitCode};

use clap::{Parser, Subcommand};
use jiff::{Timestamp, Zoned};
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
    /// 备份上一次试验，用 cargo init 创建新试验并打开项目文件夹
    New {
        /// 依赖列表，支持 crate 名和 crate@版本
        #[arg(short = 'd', long, value_name = "依赖", num_args = 1..)]
        deps: Vec<String>,
    },
    /// 打开当前试验文件夹
    Open,
    /// 提交当前试验的改动
    Backup,
}

type Result<T> = std::result::Result<T, String>;

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = run(
        cli.command,
        env::var_os("CARGOPLAY_HOME"),
        config::prepare_global,
        editor::open_folder,
    );
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("错误：{error}");
            ExitCode::FAILURE
        }
    }
}

fn default_branch_name(now: &Zoned) -> String {
    now.strftime("play/new-%Y%m%d-%H%M%S").to_string()
}

fn run(
    action: Action,
    home: Option<OsString>,
    prepare_config: impl FnOnce(&Path, bool) -> Result<()>,
    open: impl FnOnce(&Path) -> Result<()>,
) -> Result<()> {
    let timestamp = || Timestamp::now().strftime("%Y-%m-%dT%H:%M:%SZ").to_string();
    let repo = |home, create| Repository::open(home, create, prepare_config);
    match action {
        Action::Init => println!("试验仓库已就绪：{}", repo(home, true)?.root.display()),
        Action::Open => {
            let folder = config::resolve_home(home)?.join("play");
            if !folder.is_dir() {
                return Err(format!(
                    "找不到试验文件夹 {}，请检查 CARGOPLAY_HOME 或运行 cargo-play init。",
                    folder.display()
                ));
            }
            open_with_warning(&folder, open);
        }
        Action::Backup => {
            repo(home, false)?.backup(&timestamp())?;
        }
        Action::New { deps } => {
            let repo = repo(home, true)?;
            repo.backup(&timestamp())?;
            if repo.has_changes()? {
                return Err("备份后工作区仍有改动，请运行 git status 检查后重试。".into());
            }
            let branch = repo.new_branch(&default_branch_name(&Zoned::now()))?;
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

            let folder = repo.root.clone();
            drop(repo);
            open_with_warning(&folder, open);
        }
    }
    Ok(())
}

fn open_with_warning(folder: &Path, open: impl FnOnce(&Path) -> Result<()>) {
    if let Err(error) = open(folder) {
        eprintln!("警告：{error}\n请手动打开 {}", folder.display());
    }
}

#[cfg(test)]
mod tests;
