mod config;
mod editor;
mod repository;
mod state;

use std::{
    env,
    ffi::OsString,
    io::{self, BufRead, IsTerminal, Write},
    path::Path,
    process::ExitCode,
    str::FromStr,
};

use anyhow::{Context, Result, bail, ensure};
use clap::{Parser, Subcommand};
use comfy_table::{Table, presets::UTF8_FULL_CONDENSED};
use jiff::{ZonedDateTime, tz::TimeZone};
use repository::{Playground, PlaygroundTarget, directory_exists};
use state::PlaygroundRecord;
use uuid::Uuid;

#[derive(Parser)]
#[command(name = env!("CARGO_BIN_NAME"), version, about = "用 Git Bundle 归档 Rust 试验")]
struct Cli {
    #[command(subcommand)]
    command: Action,
}

#[derive(Subcommand)]
enum Action {
    /// 初始化 CGPL_HOME 下的配置、数据库、play 和 history
    Init,
    /// 归档上一个试验，用 cargo init 创建新试验并打开项目文件夹
    ///
    /// 清场后按 auto_delete 配置清理旧归档。Git 忽略的文件随旧工作区清理。
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
    /// 永久删除游乐场，省略目标时删除当前游乐场
    Delete {
        /// 从 1 开始的全库序号，或游乐场 UUID，可通过 list 查看
        #[arg(value_name = "序号或UUID", allow_negative_numbers = true)]
        target: Option<PlaygroundTarget>,
        /// 跳过交互确认
        #[arg(short = 'y', long)]
        yes: bool,
    },
    /// 归档当前试验并清空 play 文件夹
    ///
    /// 清场后按 auto_delete 配置清理旧归档。Git 忽略的文件随旧工作区清理。
    Clear {
        /// 允许丢弃未登记的工作区内容
        #[arg(short, long)]
        force: bool,
    },
    /// 打开当前试验；指定游乐场时先归档当前试验，再切换并打开
    Open {
        /// 从 1 开始的全库序号，或游乐场 UUID，可通过 list 查看
        #[arg(value_name = "序号或UUID", allow_negative_numbers = true)]
        target: Option<PlaygroundTarget>,
        /// 切换时允许替换未登记的旧工作区
        #[arg(short, long, requires = "target")]
        force: bool,
    },
    /// 提交当前试验的改动
    Backup,
    /// 按上次访问时间倒序列出游乐场，每次最多显示 50 条
    List {
        /// 跳过的游乐场条数
        #[arg(long, default_value_t = 0, value_name = "条数")]
        offset: u64,
    },
}

impl FromStr for PlaygroundTarget {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        // Preserve UUID forms without hyphens, which can consist entirely of digits.
        if let Ok(id) = Uuid::parse_str(value) {
            return Ok(Self::Id(id));
        }
        let digits = value.trim_start_matches(['+', '-']);
        if !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()) {
            return value
                .parse()
                .map(Self::Position)
                .context("序号必须是正整数");
        }
        bail!("请输入游乐场序号（从 1 开始）或 UUID")
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = run(cli.command, env::var_os("CGPL_HOME"), editor::open_folder).await;
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("错误：{error:#}");
            ExitCode::FAILURE
        }
    }
}

fn default_package_name(now: &ZonedDateTime) -> String {
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
        Action::Open { target, force } => {
            let folder = if let Some(target) = target {
                let playground = Playground::open(home, false).await?;
                playground.resume(target, force).await?;
                playground.root.clone()
            } else {
                config::resolve_home(home)?.join("play")
            };
            open_play_folder(&folder, open).await?;
        }
        Action::Backup => Playground::open(home, false).await?.backup().await?,
        Action::Clear { force } => Playground::open(home, false).await?.clear(force).await?,
        Action::List { offset } => {
            let home = config::resolve_home(home)?;
            let table = list_playgrounds(&home, &TimeZone::system(), offset).await?;
            println!("{table}");
        }
        Action::Delete { target, yes } => {
            let by_position = matches!(&target, Some(PlaygroundTarget::Position(_)));
            Playground::open(home, false)
                .await?
                .delete(target, |record, is_current| {
                    if yes {
                        Ok(true)
                    } else {
                        confirm_delete(record, is_current, by_position)
                    }
                })
                .await?;
        }
        Action::New { deps, name, force } => {
            let playground = Playground::open(home, true).await?;
            playground
                .new_project(
                    &default_package_name(&ZonedDateTime::now()),
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

fn confirm_delete(record: &PlaygroundRecord, is_current: bool, by_position: bool) -> Result<bool> {
    let stdin = io::stdin();
    ensure!(
        stdin.is_terminal(),
        "非交互环境下请传入 -y 或 --yes 确认删除。"
    );

    let mut stderr = io::stderr().lock();
    writeln!(
        stderr,
        "- 将永久删除游乐场 {:?} ({})。",
        record.name, record.id
    )?;

    if is_current {
        writeln!(
            stderr,
            "- 当前 play 目录中的所有内容都会被删除，包括未提交和被 Git 忽略的文件。"
        )?;
    }

    if record.archived_path.is_some() {
        writeln!(stderr, "- 对应的 Git Bundle 也会被删除。")?;
    }

    if by_position {
        writeln!(
            stderr,
            "\n- **注意**：删除游乐场后其他游乐场的序号可能会发生改变，连续使用序号删除时请务必检查！\n"
        )?;
    }
    write!(stderr, "确认删除？[y/N] ")?;
    stderr.flush()?;
    read_delete_confirmation(stdin.lock())
}

fn read_delete_confirmation(mut input: impl BufRead) -> Result<bool> {
    let mut answer = String::new();
    input.read_line(&mut answer).context("读取删除确认失败")?;
    let answer = answer.trim();
    Ok(answer.eq_ignore_ascii_case("y") || answer.eq_ignore_ascii_case("yes"))
}

async fn list_playgrounds(home: &Path, time_zone: &TimeZone, offset: u64) -> Result<String> {
    let entries = state::State::list(home, offset).await?;
    let mut table = Table::new();
    table
        .load_preset(UTF8_FULL_CONDENSED)
        .set_header(["序号", "名称", "ID", "上次访问"]);
    for (index, entry) in entries.into_iter().enumerate() {
        let mut alias = String::new();
        for ch in entry.name.chars() {
            if ch.is_control() {
                alias.extend(ch.escape_debug());
            } else {
                alias.push(ch);
            }
        }
        let date = entry
            .last_visit_time
            .to_zoned_datetime(time_zone.clone())
            .strftime("%Y-%m-%d %H:%M:%S %:z")
            .to_string();
        let position = offset + index as u64 + 1;
        table.add_row([position.to_string(), alias, entry.id.to_string(), date]);
    }
    Ok(table.to_string())
}

async fn open_play_folder(
    folder: &Path,
    open: impl AsyncFnOnce(&Path) -> Result<()>,
) -> Result<()> {
    ensure!(
        directory_exists(folder).await?,
        "找不到试验文件夹 {}，请检查 CGPL_HOME 或运行 cgpl init。",
        folder.display()
    );
    open_with_warning(folder, open).await;
    Ok(())
}

async fn open_with_warning(folder: &Path, open: impl AsyncFnOnce(&Path) -> Result<()>) {
    if let Err(error) = open(folder).await {
        eprintln!("警告：{error:#}\n请手动打开 {}", folder.display());
    }
}

#[cfg(test)]
mod tests;
