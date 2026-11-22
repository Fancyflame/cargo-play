mod config;
mod editor;
mod repository;
mod state;

use std::{env, ffi::OsString, path::Path, process::ExitCode};

use anyhow::{Result, ensure};
use clap::{Parser, Subcommand};
use comfy_table::{Table, presets::UTF8_FULL_CONDENSED};
use jiff::{Zoned, tz::TimeZone};
use repository::{Playground, directory_exists};

#[derive(Parser)]
#[command(version, about = "用 Git Bundle 归档 Rust 试验")]
struct Cli {
    #[command(subcommand)]
    command: Action,
}

#[derive(Subcommand)]
enum Action {
    /// 初始化 CARGOPLAY_HOME 下的配置、数据库、play 和 history
    Init,
    /// 归档上一个试验，用 cargo init 创建新试验并打开项目文件夹
    ///
    /// Git 忽略的文件随旧工作区清理。
    New {
        /// 依赖列表，支持 crate 名和 crate@版本
        #[arg(short = 'd', long, value_name = "依赖", num_args = 1..)]
        deps: Vec<String>,
        /// 游乐场名称，默认使用生成的 Cargo 包名
        #[arg(long, value_name = "名称", value_parser = clap::builder::NonEmptyStringValueParser::new())]
        name: Option<String>,
        /// 允许替换未登记的旧工作区
        #[arg(short, long)]
        force: bool,
    },
    /// 打开当前试验文件夹
    Open,
    /// 提交当前试验的改动
    Backup,
    /// 列出当前的游乐场和归档记录，归档记录按时间倒序排列
    List,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = run(
        cli.command,
        env::var_os("CARGOPLAY_HOME"),
        editor::open_folder,
    )
    .await;
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

async fn run(
    action: Action,
    home: Option<OsString>,
    open: impl AsyncFnOnce(&Path) -> Result<()>,
) -> Result<()> {
    match action {
        Action::Init => {
            let playground = Playground::open(home, true).await?;
            playground.init().await?;
            println!("试验目录已就绪：{}", playground.root.display());
        }
        Action::Open => {
            let folder = config::resolve_home(home)?.join("play");
            ensure!(
                directory_exists(&folder).await?,
                "找不到试验文件夹 {}，请检查 CARGOPLAY_HOME 或运行 cargo-play init。",
                folder.display()
            );
            open_with_warning(&folder, open).await;
        }
        Action::Backup => Playground::open(home, false).await?.backup().await?,
        Action::List => {
            let home = config::resolve_home(home)?;
            let table = list_playgrounds(&home, &TimeZone::system()).await?;
            println!("{table}");
        }
        Action::New { deps, name, force } => {
            let playground = Playground::open(home, true).await?;
            playground
                .new_project(
                    &default_package_name(&Zoned::now()),
                    name.as_deref(),
                    &deps,
                    force,
                )
                .await?;
            let folder = playground.root.clone();
            drop(playground);
            open_with_warning(&folder, open).await;
        }
    }
    Ok(())
}

async fn list_playgrounds(home: &Path, time_zone: &TimeZone) -> Result<String> {
    let entries = state::State::list(home).await?;
    let mut table = Table::new();
    table
        .load_preset(UTF8_FULL_CONDENSED)
        .set_header(["名称", "ID", "日期"]);
    for entry in entries {
        let mut alias = String::new();
        for ch in entry.name.chars() {
            if ch.is_control() {
                alias.extend(ch.escape_debug());
            } else {
                alias.push(ch);
            }
        }
        let date = match entry.archived_at {
            Some(timestamp) => timestamp
                .to_zoned(time_zone.clone())
                .strftime("%Y-%m-%d %H:%M:%S %:z")
                .to_string(),
            None => "当前".to_owned(),
        };
        table.add_row([alias, entry.id.to_string(), date]);
    }
    Ok(table.to_string())
}

async fn open_with_warning(folder: &Path, open: impl AsyncFnOnce(&Path) -> Result<()>) {
    if let Err(error) = open(folder).await {
        eprintln!("警告：{error:#}\n请手动打开 {}", folder.display());
    }
}

#[cfg(test)]
mod tests;
