//! 管理 Git 工作树，包括状态校验、备份和归档，以及 Cargo 项目初始化和依赖添加。

use std::{ffi::OsStr, io::ErrorKind, path::Path, process::Stdio};

use anyhow::{Context, Result, bail};
use jiff::Timestamp;
use tokio::{fs, io::AsyncWriteExt, process::Command};

use super::{
    filesystem::{
        close_tempdir, directory_exists, entry_names, is_reparse_point, temporary_dir, write_new,
    },
    process::{checked, command, command_error, git, git_line, git_ok, line, run_command},
};

const TARGET_PATHSPEC: &str = ":(top,icase)target";
const EXCLUDE_TARGET: &str = ":(top,icase,exclude)target";
const SNAPSHOT_REF: &str = "refs/cargo-play/worktree";

pub(super) async fn restore_bundle(home: &Path, bundle: &Path, destination: &Path) -> Result<()> {
    let bundle = bundle.as_os_str();
    checked(
        "git clone",
        git(
            home,
            [
                OsStr::new("clone"),
                OsStr::new("--"),
                bundle,
                destination.as_os_str(),
            ],
        )
        .await?,
    )?;
    let snapshot = checked(
        "git bundle list-heads",
        git(
            destination,
            [
                OsStr::new("bundle"),
                OsStr::new("list-heads"),
                bundle,
                OsStr::new(SNAPSHOT_REF),
            ],
        )
        .await?,
    )?;
    if !snapshot.is_empty() {
        checked(
            "git fetch",
            git(
                destination,
                [
                    OsStr::new("fetch"),
                    OsStr::new("--no-tags"),
                    OsStr::new("--"),
                    bundle,
                    OsStr::new(SNAPSHOT_REF),
                ],
            )
            .await?,
        )?;
        git_ok(destination, &["stash", "apply", "--index", "FETCH_HEAD"]).await?;
    }
    Ok(())
}

pub(super) async fn worktree_is_empty(root: &Path) -> Result<bool> {
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

pub(super) struct Worktree<'a> {
    root: &'a Path,
}

impl<'a> Worktree<'a> {
    pub(super) async fn init(root: &'a Path) -> Result<Self> {
        git_ok(root, &["init", "--initial-branch=archive"]).await?;
        Self::open(root).await
    }

    pub(super) async fn open(root: &'a Path) -> Result<Self> {
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

    pub(super) async fn bundle(&self, path: &Path, verify_in: &Path, index: &Path) -> Result<()> {
        if git_line(self.root, &["rev-parse", "--is-shallow-repository"]).await? == "true" {
            bail!("浅克隆仓库无法归档为完整 Bundle，请先补全 Git 历史。");
        }
        git_ok(self.root, &["rev-parse", "--verify", "HEAD^{commit}"])
            .await
            .context("当前分支没有有效的 HEAD，无法归档。请先提交一次。")?;
        let branch = git_line(self.root, &["symbolic-ref", "HEAD"]).await?;
        let snapshot = if self.has_changes().await? {
            let oid = self.snapshot(index).await.context("生成工作区快照失败")?;
            // An empty old value allows creation only if the ref does not already exist.
            git_ok(
                self.root,
                &["update-ref", "--no-deref", SNAPSHOT_REF, &oid, ""],
            )
            .await
            .context("创建临时归档引用失败")?;
            Some(oid)
        } else {
            None
        };
        let bundle = |operation| {
            [
                OsStr::new("bundle"),
                OsStr::new(operation),
                path.as_os_str(),
            ]
        };
        let created = git(
            self.root,
            bundle("create")
                .into_iter()
                .chain([OsStr::new("HEAD"), OsStr::new(&branch)])
                .chain(snapshot.as_ref().map(|_| OsStr::new(SNAPSHOT_REF))),
        )
        .await
        .and_then(|output| checked("git bundle create", output));
        if let Some(oid) = snapshot {
            let removed = git_ok(
                self.root,
                &["update-ref", "--no-deref", "-d", SNAPSHOT_REF, &oid],
            )
            .await
            .context("删除临时归档引用失败");
            if let Err(cleanup) = removed {
                return Err(match created {
                    Ok(_) => cleanup,
                    Err(error) => error.context(format!("{cleanup:#}")),
                });
            }
        }
        created?;
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

    async fn snapshot(&self, index: &Path) -> Result<String> {
        match fs::copy(self.root.join(".git/index"), index).await {
            Ok(_) => (),
            Err(error) if error.kind() == ErrorKind::NotFound => (),
            Err(error) => return Err(error).context("复制 Git 索引失败"),
        }
        self.stage_backup(Some(index)).await?;
        // A dirty submodule can still have the same gitlink after staging.
        let unstaged = self
            .indexed_git(
                &[
                    "diff",
                    "--quiet",
                    "--ignore-submodules=none",
                    "--",
                    ".",
                    EXCLUDE_TARGET,
                ],
                Some(index),
            )
            .output()
            .await
            .context("无法执行 git diff")?;
        if unstaged.status.code() == Some(1) {
            bail!("暂存后仍有改动，请检查子模块和工作区后重试。");
        }
        checked("git diff", unstaged)?;
        let output = self
            .indexed_git(&["stash", "create", "cargo-play archive"], Some(index))
            .output()
            .await
            .context("无法执行 git stash create")?;
        let oid = line(checked("git stash create", output)?)?;
        if oid.is_empty() {
            bail!("工作区有改动，但未生成 stash 快照。请运行 git status 检查后重试。");
        }
        Ok(oid)
    }

    pub(super) async fn has_changes(&self) -> Result<bool> {
        Ok(!checked(
            "git status",
            git(
                self.root,
                [
                    "--no-optional-locks",
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
            .await?,
        )?
        .is_empty())
    }

    pub(super) async fn backup(&self) -> Result<()> {
        if !self.has_changes().await? {
            println!("工作区干净，无需备份。");
            return Ok(());
        }
        let timestamp = Timestamp::now().strftime("%Y-%m-%dT%H:%M:%SZ");
        let message = format!("backup: {timestamp}");
        async {
            self.stage_backup(None).await?;
            git_ok(self.root, &["commit", "-m", &message]).await
        }
        .await
        .context("备份失败，已保留当前工作区。请运行 git status 检查现场。")?;
        println!("已提交 {message}");
        Ok(())
    }

    fn indexed_git(&self, args: &[&str], index: Option<&Path>) -> Command {
        let mut command = command(self.root, "git", args);
        if let Some(index) = index {
            command.env("GIT_INDEX_FILE", index);
        }
        command
    }

    async fn stage_backup(&self, index: Option<&Path>) -> Result<()> {
        // Enumerate paths first: git add reports an ignored target as an error even with an exclusion pathspec.
        let output = self
            .indexed_git(
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
                index,
            )
            .output()
            .await
            .context("无法执行 git ls-files")?;
        let paths = checked("git ls-files", output)?;
        if paths.is_empty() {
            return Ok(());
        }
        let mut child = self
            .indexed_git(
                &[
                    "--literal-pathspecs",
                    "add",
                    "-A",
                    "--pathspec-from-file=-",
                    "--pathspec-file-nul",
                ],
                index,
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

    pub(super) async fn init_project(&self, package: &str) -> Result<()> {
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

    pub(super) async fn add_dependencies(&self, deps: &[String]) -> Result<()> {
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
