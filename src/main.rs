mod config;
mod editor;
mod history;
mod repository;

use std::{env, ffi::OsString, path::Path, process::ExitCode};

use anyhow::{Result, ensure};
use clap::{Parser, Subcommand};
use jiff::Zoned;
use repository::Playground;

#[derive(Parser)]
#[command(version, about = "用 Git Bundle 归档 Rust 试验")]
struct Cli {
    #[command(subcommand)]
    command: Action,
}

#[derive(Subcommand)]
enum Action {
    /// 初始化 CARGOPLAY_HOME 下的配置、play 和 history
    Init,
    /// 归档上一个试验，用 cargo init 创建新试验并打开项目文件夹
    ///
    /// 首次使用新版时会清理旧版工作区。Git 忽略的文件随旧工作区清理。
    New {
        /// 依赖列表，支持 crate 名和 crate@版本
        #[arg(short = 'd', long, value_name = "依赖", num_args = 1..)]
        deps: Vec<String>,
        /// 无法读取旧项目包名时，跳过归档并强制清空
        #[arg(short, long)]
        force: bool,
    },
    /// 打开当前试验文件夹
    Open,
    /// 提交当前试验的改动
    Backup,
}

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
            eprintln!("错误：{error:#}");
            ExitCode::FAILURE
        }
    }
}

fn default_package_name(now: &Zoned) -> String {
    now.strftime("play-new-%Y%m%d-%H%M%S").to_string()
}

fn run(
    action: Action,
    home: Option<OsString>,
    prepare_config: impl FnOnce(&Path, bool) -> Result<()>,
    open: impl FnOnce(&Path) -> Result<()>,
) -> Result<()> {
    let playground = |home, create| Playground::open(home, create, prepare_config);
    match action {
        Action::Init => {
            let playground = playground(home, true)?;
            playground.init()?;
            println!("试验目录已就绪：{}", playground.root.display());
        }
        Action::Open => {
            let folder = config::resolve_home(home)?.join("play");
            ensure!(
                folder.is_dir(),
                "找不到试验文件夹 {}，请检查 CARGOPLAY_HOME 或运行 cargo-play init。",
                folder.display()
            );
            open_with_warning(&folder, open);
        }
        Action::Backup => playground(home, false)?.backup()?,
        Action::New { deps, force } => {
            let playground = playground(home, true)?;
            playground.new_project(&default_package_name(&Zoned::now()), &deps, force)?;
            let folder = playground.root.clone();
            drop(playground);
            open_with_warning(&folder, open);
        }
    }
    Ok(())
}

fn open_with_warning(folder: &Path, open: impl FnOnce(&Path) -> Result<()>) {
    if let Err(error) = open(folder) {
        eprintln!("警告：{error:#}\n请手动打开 {}", folder.display());
    }
}

#[cfg(test)]
mod tests;
