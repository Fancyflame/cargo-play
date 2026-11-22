use std::{
    env,
    ffi::{OsStr, OsString},
    fs::{File, OpenOptions, TryLockError},
    io,
    num::NonZeroU64,
    path::{Path, PathBuf},
    process::{Output, Stdio},
};

use anyhow::{Context, Error, Result, anyhow, bail, ensure};
use jiff::Timestamp;
use tempfile::TempDir;
use tokio::{fs, io::AsyncWriteExt, process::Command, task::spawn_blocking};
use uuid::Uuid;

use crate::{
    config::{prepare_config, resolve_home},
    state::{ArchiveRecord, PlaygroundRecord, State},
};

const MANAGED_MARKER: &str = "cargo-play.bundle-worktree";
const TARGET_PATHSPEC: &str = ":(top,icase)target";
const EXCLUDE_TARGET: &str = ":(top,icase,exclude)target";

#[derive(Clone, Debug)]
pub enum ResumeTarget {
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

    pub async fn resume(&self, target: ResumeTarget, force: bool) -> Result<()> {
        let state = State::open_existing(&self.home).await?;
        let current = state.active().await?;
        let id = match target {
            ResumeTarget::Id(id) => id,
            ResumeTarget::Position(position) => {
                state.id_at_position(position).await?.with_context(|| {
                    format!("找不到序号为 {position} 的游乐场，请运行 cgpl list 查看。")
                })?
            }
        };
        if current.as_ref().is_some_and(|record| record.id == id) {
            return state.activate(id).await;
        }
        let target = state
            .by_id(id)
            .await?
            .with_context(|| format!("找不到游乐场 {id}"))?;
        let bundle = target
            .archived_path
            .with_context(|| format!("游乐场 {id} 没有归档"))?;
        let existing = directory_exists(&self.root).await?;
        self.check_replacement(existing, current.as_ref(), force, "resume")
            .await?;
        self.prepare_history().await?;

        let staging = temporary_dir(Some(&self.home), ".cargo-play-resume-").await?;
        fs::create_dir(staging.path().join("next"))
            .await
            .context("创建空工作区失败")?;
        self.replace_worktree(&state, staging, existing, current.as_ref(), None)
            .await?;

        // Clearing the active record first keeps the previous project available in history
        // if Git cannot clone the requested bundle.
        let bundle = self.home.join(bundle);
        checked(
            "git clone",
            git(
                &self.home,
                [
                    OsStr::new("clone"),
                    OsStr::new("--"),
                    bundle.as_os_str(),
                    self.root.as_os_str(),
                ],
            )
            .await?,
        )
        .with_context(|| format!("从 Bundle 恢复游乐场 {id} 失败"))?;
        write_new(&marker_path(&self.root), format!("{id}\n").as_bytes())
            .await
            .context("写入恢复工作区 UUID 失败")?;
        state.activate(id).await?;
        println!("已恢复试验 {:?} ({id})", target.name);
        Ok(())
    }

    async fn check_replacement(
        &self,
        existing: bool,
        current: Option<&PlaygroundRecord>,
        force: bool,
        action: &str,
    ) -> Result<()> {
        if let Some(record) = current {
            self.verify_current(record.id).await?;
        } else if existing && !force && !worktree_is_empty(&self.root).await? {
            bail!(
                "{} 包含未登记的项目。确认丢弃后可运行 cgpl {action} --force。",
                self.root.display()
            );
        }
        Ok(())
    }

    async fn replace_worktree(
        &self,
        state: &State,
        staging: TempDir,
        existing: bool,
        current: Option<&PlaygroundRecord>,
        new: Option<(Uuid, &str)>,
    ) -> Result<()> {
        let next = staging.path().join("next");
        let mut changes = WorkspaceChanges::default();
        let result = async {
            let archive = if let Some(record) = current {
                let old = Worktree::open(&self.root).await?;
                old.backup().await?;
                if old.has_changes().await? {
                    bail!("备份后工作区仍有改动，请运行 git status 检查后重试。");
                }
                let verify_in = if new.is_some() { &next } else { &self.root };
                old.bundle(&staging.path().join("next.bundle"), verify_in)
                    .await?;
                Some(ArchiveRecord {
                    id: record.id,
                    path: format!("history/{}.bundle", record.id),
                })
            } else {
                None
            };

            let txn = match new {
                Some((id, name)) => state.stage_switch(id, name, archive.as_ref()).await?,
                None => state.stage_clear(archive.as_ref()).await?,
            };
            changes
                .switch_worktree(&self.root, staging.path(), &next, existing, rename_entry)
                .await?;
            if let Some(archive) = &archive {
                changes
                    .publish_archive(staging.path(), self.home.join(&archive.path))
                    .await?;
            }
            txn.commit().await.context("提交工作区状态失败")?;
            if let (Some(archive), Some(record)) = (archive, current) {
                println!(
                    "已归档 {:?} ({}) 到 {}",
                    record.name,
                    archive.id,
                    self.home.join(archive.path).display()
                );
            }
            Ok(())
        }
        .await;
        changes.finish(staging, result).await
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

fn marker_path(root: &Path) -> PathBuf {
    root.join(".git").join(MANAGED_MARKER)
}

#[derive(Default)]
struct WorkspaceChanges {
    moved: Vec<(PathBuf, PathBuf)>,
    archive: Option<(PathBuf, Option<PathBuf>)>,
}

impl WorkspaceChanges {
    async fn switch_worktree(
        &mut self,
        root: &Path,
        staging: &Path,
        next: &Path,
        existing: bool,
        mut rename: impl AsyncFnMut(&Path, &Path) -> io::Result<()>,
    ) -> Result<()> {
        if !existing {
            self.move_entry(next, root, &mut rename)
                .await
                .context("启用新工作区失败")?;
            return Ok(());
        }
        let previous = staging.join("previous");
        let old_entries = entry_names(root).await.context("读取原工作区失败")?;
        let new_entries = entry_names(next).await.context("启用新工作区失败")?;
        fs::create_dir(&previous)
            .await
            .context("创建原工作区的临时备份目录失败")?;

        // Leave `play` in place because it may be another process's current directory.
        for (source, destination, entries, action) in [
            (
                root,
                previous.as_path(),
                old_entries,
                "移开原工作区内容失败",
            ),
            (next, root, new_entries, "启用新工作区失败"),
        ] {
            for name in entries {
                self.move_entry(&source.join(&name), &destination.join(&name), &mut rename)
                    .await
                    .context(action)?;
            }
        }
        Ok(())
    }

    async fn move_entry(
        &mut self,
        from: &Path,
        to: &Path,
        rename: &mut impl AsyncFnMut(&Path, &Path) -> io::Result<()>,
    ) -> Result<()> {
        move_entry(from, to, rename).await?;
        self.moved.push((from.to_owned(), to.to_owned()));
        Ok(())
    }

    async fn publish_archive(&mut self, staging: &Path, path: PathBuf) -> Result<()> {
        let previous = match fs::symlink_metadata(&path).await {
            Ok(info) if info.is_file() && !is_reparse_point(&info) => {
                let previous = staging.join("previous.bundle");
                fs::copy(&path, &previous)
                    .await
                    .with_context(|| format!("保留旧 Bundle {} 失败", path.display()))?;
                Some(previous)
            }
            Ok(_) => bail!("归档路径 {} 必须是普通文件。", path.display()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => {
                return Err(error).with_context(|| format!("检查归档路径 {} 失败", path.display()));
            }
        };
        fs::rename(staging.join("next.bundle"), &path)
            .await
            .with_context(|| format!("发布 Bundle {} 失败", path.display()))?;
        self.archive = Some((path, previous));
        Ok(())
    }

    async fn finish(self, staging: TempDir, result: Result<()>) -> Result<()> {
        if let Err(error) = result {
            let mut restore_errors = Vec::new();
            if let Some((path, previous)) = self.archive {
                let restore = match previous {
                    Some(previous) => fs::rename(previous, &path).await,
                    None => fs::remove_file(&path).await,
                };
                if let Err(error) = restore {
                    restore_errors.push(format!("恢复归档 {} 失败：{error}", path.display()));
                }
            }
            for (from, to) in self.moved.into_iter().rev() {
                if let Err(error) = move_entry(&to, &from, &mut rename_entry).await {
                    restore_errors.push(format!("{error:#}"));
                }
            }
            if !restore_errors.is_empty() {
                let kept = staging.keep();
                return Err(error.context(format!(
                    "恢复原工作区失败，暂存内容保留在 {}（{}）",
                    kept.display(),
                    restore_errors.join("?")
                )));
            }
            close_tempdir(staging).await;
            return Err(error);
        }
        close_tempdir(staging).await;
        Ok(())
    }
}

async fn temporary_dir(parent: Option<&Path>, prefix: &'static str) -> Result<TempDir> {
    let parent = parent.map(Path::to_owned);
    spawn_blocking(move || {
        let mut builder = tempfile::Builder::new();
        builder.prefix(prefix);
        match parent {
            Some(parent) => builder.tempdir_in(parent),
            None => builder.tempdir(),
        }
    })
    .await?
    .context("创建临时目录失败")
}

async fn close_tempdir(dir: TempDir) {
    let path = dir.path().to_owned();
    if let Err(error) = spawn_blocking(move || dir.close().map_err(Error::from))
        .await
        .map_err(Error::from)
        .and_then(|result| result)
    {
        eprintln!("警告：清理 {} 失败：{error}", path.display());
    }
}

async fn entry_names(path: &Path) -> Result<Vec<OsString>> {
    async {
        let mut entries = fs::read_dir(path).await?;
        let mut names = Vec::new();
        while let Some(entry) = entries.next_entry().await? {
            names.push(entry.file_name());
        }
        Ok::<_, io::Error>(names)
    }
    .await
    .with_context(|| format!("读取 {} 失败", path.display()))
}

async fn rename_entry(from: &Path, to: &Path) -> io::Result<()> {
    fs::rename(from, to).await
}

async fn move_entry(
    from: &Path,
    to: &Path,
    rename: &mut impl AsyncFnMut(&Path, &Path) -> io::Result<()>,
) -> Result<()> {
    let result = match fs::symlink_metadata(to).await {
        Ok(_) => Err(io::Error::new(io::ErrorKind::AlreadyExists, "目标已存在")),
        Err(error) if error.kind() == io::ErrorKind::NotFound => rename(from, to).await,
        Err(error) => Err(error),
    };
    result.with_context(|| format!("移动 {} 到 {} 失败", from.display(), to.display()))
}

fn is_reparse_point(info: &std::fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        info.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        info.file_type().is_symlink()
    }
}

pub async fn directory_exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path).await {
        Ok(info) if info.is_dir() && !is_reparse_point(&info) => Ok(true),
        Ok(_) => bail!("{} 必须是独立的目录。", path.display()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| format!("检查 {} 失败", path.display())),
    }
}

async fn worktree_is_empty(root: &Path) -> Result<bool> {
    let entries = entry_names(root).await?;
    if entries.is_empty() {
        return Ok(true);
    }
    if !entries.iter().any(|name| name == ".git") {
        return Ok(false);
    }
    // An editor can recreate the ignored Cargo cache in a freshly initialized worktree.
    for name in entries.iter().filter(|name| *name != ".git") {
        if !name.eq_ignore_ascii_case("target")
            || !fs::symlink_metadata(root.join(name))
                .await
                .is_ok_and(|info| info.is_dir() && !is_reparse_point(&info))
        {
            return Ok(false);
        }
    }
    let Ok(worktree) = Worktree::open(root).await else {
        return Ok(false);
    };
    Ok(
        git_ok(root, &["for-each-ref", "--count=1", "--format=%(refname)"])
            .await?
            .is_empty()
            && !worktree.has_changes().await?,
    )
}

struct Worktree<'a> {
    root: &'a Path,
}

impl<'a> Worktree<'a> {
    async fn init(root: &'a Path) -> Result<Self> {
        git_ok(root, &["init", "--initial-branch=archive"]).await?;
        Self::open(root).await
    }

    async fn open(root: &'a Path) -> Result<Self> {
        let git_dir = root.join(".git");
        if !directory_exists(root).await? || !directory_exists(&git_dir).await? {
            bail!(
                "{} 必须包含独立的 .git 目录。首次使用请运行 cgpl init。",
                root.display()
            );
        }
        let top = git_line(root, &["rev-parse", "--show-toplevel"])
            .await
            .with_context(|| format!("{} 不是可用的 Git 工作区", root.display()))?;
        let canonical = async |path: &Path| {
            fs::canonicalize(path)
                .await
                .with_context(|| format!("解析 {} 失败", path.display()))
        };
        if canonical(root).await? != canonical(Path::new(&top)).await? {
            bail!("{} 必须是独立的 Git 仓库根目录。", root.display());
        }

        for marker in [
            "MERGE_HEAD",
            "CHERRY_PICK_HEAD",
            "REVERT_HEAD",
            "rebase-merge",
            "rebase-apply",
            "sequencer",
            "BISECT_START",
        ] {
            if fs::try_exists(git_dir.join(marker))
                .await
                .context("检查 Git 状态失败")?
            {
                bail!("有未完成的 Git 操作（{marker}），请先完成或中止该操作。");
            }
        }
        let head = git(root, ["symbolic-ref", "--quiet", "HEAD"]).await?;
        match head.status.code() {
            Some(0) => (),
            Some(1) => bail!("当前处于分离 HEAD，请先切换到本地分支。"),
            _ => return Err(command_error("git symbolic-ref", &head)),
        }
        let staged_target = !git_ok(root, &["ls-files", "-z", "--", TARGET_PATHSPEC])
            .await?
            .is_empty();
        let committed = git(root, ["rev-parse", "--verify", "--quiet", "HEAD"]).await?;
        let committed_target = match committed.status.code() {
            Some(0) => git_ok(root, &["ls-tree", "-z", "--name-only", "HEAD"])
                .await?
                .split(|&byte| byte == 0)
                .any(|entry| entry.eq_ignore_ascii_case(b"target")),
            Some(1) => false,
            _ => return Err(command_error("git rev-parse", &committed)),
        };
        if staged_target || committed_target {
            bail!(
                "Git 正在跟踪 target。请先通过 git rm -r --cached -- target 取消跟踪并提交，以保留磁盘文件。"
            );
        }
        Ok(Self { root })
    }

    async fn bundle(&self, path: &Path, verify_in: &Path) -> Result<()> {
        if git_line(self.root, &["rev-parse", "--is-shallow-repository"]).await? == "true" {
            bail!("浅克隆仓库无法归档为完整 Bundle，请先补全 Git 历史。");
        }
        let branch = git_line(self.root, &["symbolic-ref", "HEAD"]).await?;
        let bundle = |operation| {
            [
                OsStr::new("bundle"),
                OsStr::new(operation),
                path.as_os_str(),
            ]
        };
        checked(
            "git bundle create",
            git(
                self.root,
                bundle("create")
                    .into_iter()
                    .chain([OsStr::new("HEAD"), OsStr::new(&branch)]),
            )
            .await?,
        )?;
        checked("git bundle verify", git(verify_in, bundle("verify")).await?)?;
        async {
            fs::OpenOptions::new()
                .write(true)
                .open(path)
                .await?
                .sync_all()
                .await
        }
        .await
        .with_context(|| format!("同步 Bundle {} 失败", path.display()))?;
        Ok(())
    }

    async fn has_changes(&self) -> Result<bool> {
        Ok(!git_ok(
            self.root,
            &[
                "status",
                "--porcelain=v1",
                "-z",
                "--untracked-files=all",
                "--ignore-submodules=none",
                "--",
                ".",
                EXCLUDE_TARGET,
            ],
        )
        .await?
        .is_empty())
    }

    async fn backup(&self) -> Result<()> {
        if !self.has_changes().await? {
            println!("工作区干净，无需备份。");
            return Ok(());
        }
        let timestamp = Timestamp::now().strftime("%Y-%m-%dT%H:%M:%SZ");
        let message = format!("backup: {timestamp}");
        async {
            self.stage_backup().await?;
            git_ok(self.root, &["commit", "-m", &message]).await
        }
        .await
        .context("备份失败，已保留当前工作区。请运行 git status 检查现场。")?;
        println!("已提交 {message}");
        Ok(())
    }

    async fn stage_backup(&self) -> Result<()> {
        // Enumerate paths first: git add reports an ignored target as an error even with an exclusion pathspec.
        let paths = git_ok(
            self.root,
            &[
                "ls-files",
                "-z",
                "--cached",
                "--others",
                "--exclude-standard",
                "--",
                ".",
                EXCLUDE_TARGET,
            ],
        )
        .await?;
        if paths.is_empty() {
            return Ok(());
        }
        let mut child = command(
            self.root,
            "git",
            [
                "--literal-pathspecs",
                "add",
                "-A",
                "--pathspec-from-file=-",
                "--pathspec-file-nul",
            ],
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("无法执行 git add")?;
        let mut stdin = child.stdin.take().expect("piped git stdin");
        let (sent, output) = tokio::join!(
            async move { stdin.write_all(&paths).await },
            child.wait_with_output(),
        );
        let output = output.context("等待 git add 失败")?;
        checked("git add", output)?;
        sent.context("发送备份文件列表失败")
    }

    async fn init_project(&self, package: &str) -> Result<()> {
        // Generate in an isolated directory so Cargo cannot adopt leftovers or edit a parent workspace.
        let scaffold = temporary_dir(None, "cargo-play-scaffold-").await?;
        let result = self.populate_project(scaffold.path(), package).await;
        close_tempdir(scaffold).await;
        result
    }

    async fn populate_project(&self, scaffold: &Path, package: &str) -> Result<()> {
        let args = [
            "init",
            "--vcs",
            "none",
            "--bin",
            "--name",
            package,
            "--edition",
            "2024",
            ".",
        ];
        run_command(scaffold, "cargo", args)
            .await
            .and_then(|output| checked("cargo init", output))?;

        let mut manifest = fs::read(scaffold.join("Cargo.toml"))
            .await
            .context("读取新工程的 Cargo.toml 失败")?;
        manifest.extend_from_slice(b"\n[workspace]\n");
        let main = fs::read(scaffold.join("src/main.rs"))
            .await
            .context("读取新工程的 src/main.rs 失败")?;

        fs::create_dir(self.root.join("src"))
            .await
            .context("创建工程的 src 目录失败")?;
        for (name, data) in [
            ("Cargo.toml", manifest.as_slice()),
            ("src/main.rs", main.as_slice()),
            (".gitignore", b"/target/\n"),
        ] {
            write_new(&self.root.join(name), data)
                .await
                .with_context(|| format!("写入 {name} 失败"))?;
        }
        git_ok(
            self.root,
            &["add", "-f", "--", ".gitignore", "Cargo.toml", "src/main.rs"],
        )
        .await?;
        git_ok(self.root, &["commit", "-m", "init: cargo init"])
            .await
            .context("提交初始工程失败")?;
        Ok(())
    }

    async fn add_dependencies(&self, deps: &[String]) -> Result<()> {
        let args = ["add", "--manifest-path", "Cargo.toml", "--"]
            .into_iter()
            .chain(deps.iter().map(String::as_str));
        run_command(self.root, "cargo", args)
            .await
            .and_then(|output| checked("cargo add", output))?;
        let lock_tracked = !git_ok(self.root, &["ls-files", "-z", "--", "Cargo.lock"])
            .await?
            .is_empty();
        let lock_on_disk = fs::try_exists(self.root.join("Cargo.lock"))
            .await
            .context("检查 Cargo.lock 失败")?;
        let mut paths = vec!["Cargo.toml"];
        if lock_on_disk || lock_tracked {
            paths.push("Cargo.lock");
        }
        let mut stage = vec!["add", "-A", "-f", "--"];
        stage.extend(&paths);
        git_ok(self.root, &stage)
            .await
            .context("暂存依赖文件失败")?;

        let message = format!("deps: add {}", deps.join(" "));
        let mut commit = vec!["commit", "--only", "--allow-empty", "-m", &message, "--"];
        commit.extend(paths);
        git_ok(self.root, &commit).await.context("依赖提交失败")?;
        Ok(())
    }
}

async fn write_new(path: &Path, data: &[u8]) -> io::Result<()> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .await?;
    file.write_all(data).await?;
    file.flush().await
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

// Ignore Git's repository overrides inherited from hooks and other callers.
const GIT_LOCAL_ENV: &[&str] = &[
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_COMMON_DIR",
    "GIT_CONFIG",
    "GIT_CONFIG_COUNT",
    "GIT_CONFIG_PARAMETERS",
    "GIT_DIR",
    "GIT_GRAFT_FILE",
    "GIT_IMPLICIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_NO_REPLACE_OBJECTS",
    "GIT_OBJECT_DIRECTORY",
    "GIT_PREFIX",
    "GIT_REPLACE_REF_BASE",
    "GIT_SHALLOW_FILE",
    "GIT_WORK_TREE",
];

fn command(
    root: &Path,
    program: &str,
    args: impl IntoIterator<Item = impl AsRef<OsStr>>,
) -> Command {
    let mut command = Command::new(program);
    command.args(args).current_dir(root);
    for variable in GIT_LOCAL_ENV {
        command.env_remove(variable);
    }
    #[cfg(test)]
    crate::tests::configure_git(command.as_std_mut());
    // A nonempty value disables terminal color in the subprocess output.
    if env::var_os("NO_COLOR").is_none() {
        command.env("NO_COLOR", "1");
    }
    command
}

async fn run_command(
    root: &Path,
    program: &str,
    args: impl IntoIterator<Item = impl AsRef<OsStr>>,
) -> Result<Output> {
    command(root, program, args)
        .output()
        .await
        .with_context(|| format!("无法执行 {program}"))
}

async fn git(root: &Path, args: impl IntoIterator<Item = impl AsRef<OsStr>>) -> Result<Output> {
    run_command(root, "git", args).await
}

async fn git_ok(root: &Path, args: &[&str]) -> Result<Vec<u8>> {
    checked(&format!("git {}", args[0]), git(root, args).await?)
}

async fn git_line(root: &Path, args: &[&str]) -> Result<String> {
    line(git_ok(root, args).await?)
}

fn line(bytes: Vec<u8>) -> Result<String> {
    String::from_utf8(bytes)
        .map(|text| text.trim_end_matches(['\r', '\n']).to_owned())
        .context("Git 文本输出不是 UTF-8")
}

fn checked(label: &str, output: Output) -> Result<Vec<u8>> {
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(command_error(label, &output))
    }
}

fn command_error(label: &str, output: &Output) -> Error {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let details = [stderr.trim(), stdout.trim()]
        .into_iter()
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>();
    if details.is_empty() {
        anyhow!("{label} 失败（{}）", output.status)
    } else {
        anyhow!("{label} 失败（{}）\n{}", output.status, details.join("\n"))
    }
}

#[cfg(test)]
mod replacement_tests {
    use super::*;
    use std::fs;

    async fn replace(
        playground: &Playground,
        staging: TempDir,
        next: &Path,
        rename: impl AsyncFnMut(&Path, &Path) -> io::Result<()>,
    ) -> Result<()> {
        let mut changes = WorkspaceChanges::default();
        let result = changes
            .switch_worktree(&playground.root, staging.path(), next, true, rename)
            .await;
        changes.finish(staging, result).await
    }

    async fn replacement_fixture() -> (TempDir, Playground, TempDir, PathBuf) {
        let home = tempfile::tempdir().unwrap();
        let playground = Playground::open(Some(home.path().as_os_str().to_owned()), true)
            .await
            .unwrap();
        fs::create_dir(&playground.root).unwrap();
        fs::write(playground.root.join("keep.txt"), "old project").unwrap();
        fs::write(playground.root.join(".old"), "old hidden file").unwrap();
        let staging = tempfile::tempdir_in(home.path()).unwrap();
        let next = staging.path().join("next");
        fs::create_dir(&next).unwrap();
        fs::write(next.join("keep.txt"), "new project").unwrap();
        fs::write(next.join("new.txt"), "new file").unwrap();
        (home, playground, staging, next)
    }

    fn assert_old_project(root: &Path) {
        assert_eq!(
            fs::read_to_string(root.join("keep.txt")).unwrap(),
            "old project"
        );
        assert_eq!(
            fs::read_to_string(root.join(".old")).unwrap(),
            "old hidden file"
        );
        assert!(!root.join("new.txt").exists());
    }

    #[tokio::test]
    async fn preserves_play_when_staged_project_is_missing() {
        let (_home, playground, staging, next) = replacement_fixture().await;
        let missing = next.join("missing");
        let error = replace(&playground, staging, &missing, rename_entry)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("启用新工作区失败"), "{error:#}");
        assert_eq!(
            error.downcast_ref::<io::Error>().map(io::Error::kind),
            Some(io::ErrorKind::NotFound)
        );
        assert_old_project(&playground.root);
    }

    #[tokio::test]
    async fn restores_play_when_moving_old_entries_fails() {
        let (_home, playground, staging, next) = replacement_fixture().await;
        let staging_path = staging.path().to_owned();
        let mut old_moves = 0;
        let error = replace(&playground, staging, &next, async |from, to| {
            if from.parent() == Some(&playground.root) {
                old_moves += 1;
                if old_moves == 2 {
                    return Err(io::ErrorKind::PermissionDenied.into());
                }
            }
            fs::rename(from, to)
        })
        .await
        .unwrap_err();
        assert!(
            error.to_string().contains("移开原工作区内容失败"),
            "{error:#}"
        );
        assert_old_project(&playground.root);
        assert!(!staging_path.exists());
    }

    #[tokio::test]
    async fn restores_play_when_installation_fails_after_moving_a_new_entry() {
        let (_home, playground, staging, next) = replacement_fixture().await;
        let staging_path = staging.path().to_owned();
        let mut new_moves = 0;
        let error = replace(&playground, staging, &next, async |from, to| {
            if from.parent() == Some(&next) {
                new_moves += 1;
                if new_moves == 2 {
                    return Err(io::ErrorKind::PermissionDenied.into());
                }
            }
            fs::rename(from, to)
        })
        .await
        .unwrap_err();
        assert!(error.to_string().contains("启用新工作区失败"), "{error:#}");
        assert_old_project(&playground.root);
        assert!(!staging_path.exists());
    }

    #[tokio::test]
    async fn keeps_backup_when_a_new_file_blocks_rollback() {
        let (_home, playground, staging, next) = replacement_fixture().await;
        let staging_path = staging.path().to_owned();
        let obstructed = playground.root.join("keep.txt");
        let error = replace(&playground, staging, &next, async |from, to| {
            if from.parent() == Some(&next) {
                fs::write(&obstructed, "created by another process")?;
                return Err(io::ErrorKind::PermissionDenied.into());
            }
            fs::rename(from, to)
        })
        .await
        .unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("恢复原工作区失败"), "{message}");
        assert!(
            message.contains(&staging_path.display().to_string()),
            "{message}"
        );
        assert!(
            message.contains(&obstructed.display().to_string()),
            "{message}"
        );
        assert_eq!(
            fs::read_to_string(&obstructed).unwrap(),
            "created by another process"
        );
        assert_eq!(
            fs::read_to_string(playground.root.join(".old")).unwrap(),
            "old hidden file"
        );
        assert_eq!(
            fs::read_to_string(staging_path.join("previous/keep.txt")).unwrap(),
            "old project"
        );
    }
}
