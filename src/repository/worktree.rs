//! 管理 Git 工作树，包括状态校验、备份和归档，以及 Cargo 项目初始化和依赖添加。

use std::{ffi::OsStr, path::Path, process::Stdio};

use anyhow::{Context, Result, bail};
use jiff::Timestamp;
use tokio::{fs, io::AsyncWriteExt};

use super::{
    filesystem::{
        close_tempdir, directory_exists, entry_names, is_reparse_point, temporary_dir, write_new,
    },
    process::{checked, command, command_error, git, git_line, git_ok, run_command},
};

const TARGET_PATHSPEC: &str = ":(top,icase)target";
const EXCLUDE_TARGET: &str = ":(top,icase,exclude)target";

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

    pub(super) async fn bundle(&self, path: &Path, verify_in: &Path) -> Result<()> {
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

    pub(super) async fn has_changes(&self) -> Result<bool> {
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

    pub(super) async fn backup(&self) -> Result<()> {
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
