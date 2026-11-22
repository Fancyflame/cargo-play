//! 游乐场仓库的入口，负责打开和初始化仓库，以及新建、清场和恢复游乐场。

mod deletion;
mod filesystem;
mod process;
mod workspace;
mod worktree;

use std::{
    ffi::OsString,
    fs::{File, OpenOptions, TryLockError},
    num::NonZeroU64,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail, ensure};
use tokio::{fs, task::spawn_blocking};
use uuid::Uuid;

use crate::{
    config::{prepare_config, resolve_home},
    state::{PlaygroundRecord, State},
};

pub use filesystem::directory_exists;
use filesystem::{close_tempdir, rename_entry, temporary_dir, write_new};
use workspace::WorkspaceChanges;
use worktree::{Worktree, restore_bundle};

const MANAGED_MARKER: &str = "cargo-play.bundle-worktree";

#[derive(Clone, Debug)]
pub enum PlaygroundTarget {
    Id(Uuid),
    Position(NonZeroU64),
}

pub struct Playground {
    home: PathBuf,
    pub root: PathBuf,
    _lock: File,
}

impl Playground {
    pub async fn open(home: Option<OsString>, create: bool) -> Result<Self> {
        let home = resolve_home(home)?;
        if create {
            fs::create_dir_all(&home)
                .await
                .with_context(|| format!("创建 {} 失败", home.display()))?;
        } else if !fs::metadata(&home).await.is_ok_and(|info| info.is_dir()) {
            bail!("首次使用请运行 cgpl init。");
        }
        let lock_path = home.join(".cargo-play.lock");
        let _lock = spawn_blocking(move || lock_home(&lock_path)).await??;
        prepare_config(&home, create).await?;
        let root = home.join("play");
        Ok(Self { home, root, _lock })
    }

    pub async fn init(&self) -> Result<()> {
        self.prepare_history().await?;
        let state = State::open(&self.home).await?;
        if let Some(current) = state.active().await? {
            self.verify_current(current.id).await?;
            Worktree::open(&self.root).await?;
            return Ok(());
        }
        if !directory_exists(&self.root).await? {
            fs::create_dir(&self.root)
                .await
                .with_context(|| format!("创建 {} 失败", self.root.display()))?;
        }
        let git_dir = self.root.join(".git");
        if directory_exists(&git_dir).await? && fs::try_exists(git_dir.join("HEAD")).await? {
            Worktree::open(&self.root).await?;
        } else {
            Worktree::init(&self.root).await?;
        }
        Ok(())
    }

    pub async fn backup(&self) -> Result<()> {
        Worktree::open(&self.root).await?.backup().await
    }

    pub async fn new_project(
        &self,
        base: &str,
        name: Option<&str>,
        deps: &[String],
        force: bool,
    ) -> Result<()> {
        let existing = directory_exists(&self.root).await?;
        self.prepare_history().await?;
        let state = State::open(&self.home).await?;
        let current = state.active().await?;
        self.check_replacement(existing, current.as_ref(), force, "new")
            .await?;
        let package = state.next_default_name(base).await?;
        let display_name = name.unwrap_or(&package);
        let id = Uuid::new_v4();
        let staging = temporary_dir(Some(&self.home), ".cargo-play-new-").await?;
        let next = staging.path().join("next");
        let result = async {
            fs::create_dir(&next).await.context("创建新工作区失败")?;
            let fresh = Worktree::init(&next).await?;
            write_new(&marker_path(&next), format!("{id}\n").as_bytes())
                .await
                .context("写入新工作区 UUID 失败")?;
            fresh.init_project(&package).await?;
            if !deps.is_empty() {
                fresh.add_dependencies(deps).await?;
            }
            Ok(())
        }
        .await;
        if let Err(error) = result {
            close_tempdir(staging).await;
            return Err(error);
        }
        self.replace_worktree(
            &state,
            staging,
            existing,
            current.as_ref(),
            Some((id, display_name)),
        )
        .await?;
        println!("已创建试验 {display_name:?} ({id})");
        if !deps.is_empty() {
            println!("已提交依赖 {}", deps.join(", "));
        }
        Ok(())
    }

    pub async fn clear(&self, force: bool) -> Result<()> {
        let state = State::open_existing(&self.home).await?;
        let current = state.active().await?;
        self.archive_and_clear(&state, current.as_ref(), force, "clear")
            .await?;
        println!("已清空工作区：{}", self.root.display());
        Ok(())
    }

    pub async fn resume(&self, target: PlaygroundTarget, force: bool) -> Result<()> {
        let state = State::open_existing(&self.home).await?;
        let current = state.active().await?;
        let target = Self::resolve_target(&state, target).await?;
        let id = target.id;
        if current.as_ref().is_some_and(|record| record.id == id) {
            return state.activate(id).await;
        }
        let bundle = target
            .archived_path
            .with_context(|| format!("游乐场 {id} 没有归档"))?;
        self.archive_and_clear(&state, current.as_ref(), force, "resume")
            .await?;

        let bundle = self.home.join(bundle);
        let staging = temporary_dir(Some(&self.home), ".cargo-play-resume-").await?;
        let next = staging.path().join("next");
        let mut changes = WorkspaceChanges::default();
        let result = async {
            restore_bundle(&self.home, &bundle, &next)
                .await
                .with_context(|| format!("从 Bundle 恢复游乐场 {id} 失败"))?;
            write_new(&marker_path(&next), format!("{id}\n").as_bytes())
                .await
                .context("写入恢复工作区 UUID 失败")?;
            changes
                .switch_worktree(&self.root, staging.path(), &next, true, rename_entry)
                .await?;
            state.activate(id).await
        }
        .await;
        changes.finish(staging, result).await?;
        println!("已恢复试验 {:?} ({id})", target.name);
        Ok(())
    }

    async fn archive_and_clear(
        &self,
        state: &State,
        current: Option<&PlaygroundRecord>,
        force: bool,
        action: &str,
    ) -> Result<()> {
        let existing = directory_exists(&self.root).await?;
        self.check_replacement(existing, current, force, action)
            .await?;
        self.prepare_history().await?;

        let staging = temporary_dir(Some(&self.home), ".cargo-play-clear-").await?;
        fs::create_dir(staging.path().join("next"))
            .await
            .context("创建空工作区失败")?;
        self.replace_worktree(state, staging, existing, current, None)
            .await
    }

    async fn resolve_target(state: &State, target: PlaygroundTarget) -> Result<PlaygroundRecord> {
        let id = match target {
            PlaygroundTarget::Id(id) => id,
            PlaygroundTarget::Position(position) => {
                state.id_at_position(position).await?.with_context(|| {
                    format!("找不到序号为 {position} 的游乐场，请运行 cgpl list 查看。")
                })?
            }
        };
        state
            .by_id(id)
            .await?
            .with_context(|| format!("找不到游乐场 {id}"))
    }

    async fn prepare_history(&self) -> Result<()> {
        let path = self.home.join("history");
        if !directory_exists(&path).await? {
            fs::create_dir(&path)
                .await
                .with_context(|| format!("创建 {} 失败", path.display()))?;
        }
        Ok(())
    }

    async fn verify_current(&self, id: Uuid) -> Result<()> {
        let git_dir = self.root.join(".git");
        ensure!(
            directory_exists(&self.root).await? && directory_exists(&git_dir).await?,
            "当前游乐场 {id} 的 Git 工作区不存在。"
        );
        let path = marker_path(&self.root);
        let marker = fs::read_to_string(&path)
            .await
            .with_context(|| format!("读取当前游乐场 UUID（{}）失败", path.display()))?;
        ensure!(
            marker.trim().parse::<Uuid>().ok() == Some(id),
            "工作区的 UUID 与数据库中当前游乐场 {id} 不一致。"
        );
        Ok(())
    }
}

fn archive_path(id: Uuid) -> String {
    format!("history/{id}.bundle")
}

fn marker_path(root: &Path) -> PathBuf {
    root.join(".git").join(MANAGED_MARKER)
}

fn lock_home(path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .with_context(|| format!("打开锁文件 {} 失败", path.display()))?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(TryLockError::WouldBlock) => {
            bail!("试验仓库正在使用，请等另一条 cgpl 命令结束后重试。")
        }
        Err(TryLockError::Error(error)) => Err(error).context("锁定试验仓库失败"),
    }
}
