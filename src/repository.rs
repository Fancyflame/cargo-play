use std::{
    env,
    ffi::{OsStr, OsString},
    fs::{self, File, OpenOptions, TryLockError},
    io::{self, Write},
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};

use anyhow::{Context, Error, Result, anyhow, bail};
use jiff::Timestamp;
use serde::Deserialize;
use tempfile::TempDir;

use crate::{
    config::resolve_home,
    history::{History, check_package_name},
};

const MANAGED_MARKER: &str = "cargo-play.bundle-worktree";
const TARGET_PATHSPEC: &str = ":(top,icase)target";
const EXCLUDE_TARGET: &str = ":(top,icase,exclude)target";

pub struct Playground {
    home: PathBuf,
    pub root: PathBuf,
    _lock: File,
}

impl Playground {
    pub fn open(
        home: Option<OsString>,
        create: bool,
        prepare_config: impl FnOnce(&Path, bool) -> Result<()>,
    ) -> Result<Self> {
        let home = resolve_home(home)?;
        if create {
            fs::create_dir_all(&home).with_context(|| format!("创建 {} 失败", home.display()))?;
        } else if !home.is_dir() {
            bail!("首次使用请运行 cargo-play init。");
        }
        let _lock = lock_home(&home)?;
        prepare_config(&home, create)?;
        let root = home.join("play");
        Ok(Self { home, root, _lock })
    }

    pub fn init(&self) -> Result<()> {
        History::open(&self.home)?;
        if !directory_exists(&self.root)? {
            fs::create_dir(&self.root)
                .with_context(|| format!("创建 {} 失败", self.root.display()))?;
        }
        let git_dir = self.root.join(".git");
        if directory_exists(&git_dir)? && git_dir.join("HEAD").is_file() {
            Worktree::open(&self.root)?;
        } else {
            Worktree::init(&self.root)?;
        }
        Ok(())
    }

    pub fn backup(&self) -> Result<()> {
        Worktree::open(&self.root)?.backup()
    }

    pub fn new_project(&self, base: &str, deps: &[String], force: bool) -> Result<()> {
        let existing = directory_exists(&self.root)?;
        let previous_name = if existing {
            match package_name(&self.root) {
                Ok(name) => Some(name),
                Err(_) if force || worktree_is_empty(&self.root)? => None,
                Err(error) => {
                    return Err(error.context(format!(
                        "{} 包含旧项目内容。确认丢弃后可运行 cargo-play new --force。",
                        self.root.display()
                    )));
                }
            }
        } else {
            None
        };
        let git_dir = self.root.join(".git");
        let managed = previous_name.is_some()
            && directory_exists(&git_dir)?
            && git_dir.join(MANAGED_MARKER).is_file();
        let old = managed.then(|| Worktree::open(&self.root)).transpose()?;
        let mut history = History::open(&self.home)?;
        let package = history.next_package(base, previous_name.as_deref());

        let staging = tempfile::Builder::new()
            .prefix(".cargo-play-new-")
            .tempdir_in(&self.home)
            .context("创建新工程临时目录失败")?;
        let next = staging.path().join("next");
        fs::create_dir(&next).context("创建新工作区失败")?;
        let fresh = Worktree::init(&next)?;
        fresh.init_project(&package)?;
        if !deps.is_empty() {
            fresh.add_dependencies(deps)?;
        }

        if let (Some(old), Some(name)) = (old, previous_name) {
            old.backup()?;
            if old.has_changes()? {
                bail!("备份后工作区仍有改动，请运行 git status 检查后重试。");
            }
            let bundle = history.archive(&name, |path| old.bundle(path, &next))?;
            println!("已归档 {name} 到 {}", bundle.display());
        }
        self.replace(staging, &next, existing)?;
        println!("已创建试验 {package}");
        if !deps.is_empty() {
            println!("已提交依赖 {}", deps.join(", "));
        }
        Ok(())
    }

    fn replace(&self, staging: TempDir, next: &Path, existing: bool) -> Result<()> {
        let previous = staging.path().join("previous");
        if existing {
            fs::rename(&self.root, &previous)
                .with_context(|| format!("移开原工作区 {} 失败", self.root.display()))?;
        }
        if let Err(error) = fs::rename(next, &self.root) {
            if existing && let Err(restore) = fs::rename(&previous, &self.root) {
                // Keep the old directory even when Windows refuses to move it back.
                let kept = staging.keep().join("previous");
                return Err(error).with_context(|| {
                    format!(
                        "启用新工作区失败，恢复原工作区也失败（{restore}），原工作区保留在 {}",
                        kept.display()
                    )
                });
            }
            return Err(error).context("启用新工作区失败");
        }
        let cleanup_path = staging.path().to_owned();
        if let Err(error) = staging.close() {
            eprintln!("警告：清理 {} 失败：{error}", cleanup_path.display());
        }
        Ok(())
    }
}

pub fn directory_exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(info) if info.is_dir() => {
            #[cfg(windows)]
            {
                use std::os::windows::fs::MetadataExt;
                if info.file_attributes() & 0x400 != 0 {
                    bail!("{} 必须是独立的目录。", path.display());
                }
            }
            Ok(true)
        }
        Ok(_) => bail!("{} 必须是独立的目录。", path.display()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| format!("检查 {} 失败", path.display())),
    }
}

fn worktree_is_empty(root: &Path) -> Result<bool> {
    let entries = fs::read_dir(root).with_context(|| format!("读取 {} 失败", root.display()))?;
    let mut has_git = false;
    for entry in entries {
        let entry = entry.with_context(|| format!("读取 {} 失败", root.display()))?;
        if entry.file_name() != ".git" {
            return Ok(false);
        }
        has_git = true;
    }
    if !has_git {
        return Ok(true);
    }

    // `init` creates a .git directory before there are any project files.
    let Ok(worktree) = Worktree::open(root) else {
        return Ok(false);
    };
    Ok(
        git_ok(root, &["for-each-ref", "--count=1", "--format=%(refname)"])?.is_empty()
            && !worktree.has_changes()?,
    )
}

fn package_name(root: &Path) -> Result<String> {
    #[derive(Deserialize)]
    struct Manifest {
        package: Package,
    }
    #[derive(Deserialize)]
    struct Package {
        name: String,
    }

    let path = root.join("Cargo.toml");
    let manifest = fs::read_to_string(&path)
        .with_context(|| format!("读取 {} 的 [package].name 失败", path.display()))?;
    let manifest: Manifest = toml::from_str(manifest.trim_start_matches('\u{feff}'))
        .with_context(|| format!("解析 {} 的 [package].name 失败", path.display()))?;
    check_package_name(&manifest.package.name)?;
    Ok(manifest.package.name)
}

struct Worktree<'a> {
    root: &'a Path,
}

impl<'a> Worktree<'a> {
    fn init(root: &'a Path) -> Result<Self> {
        git_ok(root, &["init", "--initial-branch=archive"])?;
        write_new(&root.join(".git").join(MANAGED_MARKER), b"1\n")
            .context("写入新工作区标记失败")?;
        Self::open(root)
    }

    fn open(root: &'a Path) -> Result<Self> {
        let git_dir = root.join(".git");
        if !directory_exists(root)? || !directory_exists(&git_dir)? {
            bail!(
                "{} 必须包含独立的 .git 目录。首次使用请运行 cargo-play init。",
                root.display()
            );
        }
        let top = git_line(root, &["rev-parse", "--show-toplevel"])
            .with_context(|| format!("{} 不是可用的 Git 工作区", root.display()))?;
        let canonical = |path: &Path| {
            path.canonicalize()
                .with_context(|| format!("解析 {} 失败", path.display()))
        };
        if canonical(root)? != canonical(Path::new(&top))? {
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
            if git_dir
                .join(marker)
                .try_exists()
                .context("检查 Git 状态失败")?
            {
                bail!("有未完成的 Git 操作（{marker}），请先完成或中止该操作。");
            }
        }
        let head = git(root, ["symbolic-ref", "--quiet", "HEAD"])?;
        match head.status.code() {
            Some(0) => (),
            Some(1) => bail!("当前处于分离 HEAD，请先切换到本地分支。"),
            _ => return Err(command_error("git symbolic-ref", &head)),
        }
        let staged_target = !git_ok(root, &["ls-files", "-z", "--", TARGET_PATHSPEC])?.is_empty();
        let committed = git(root, ["rev-parse", "--verify", "--quiet", "HEAD"])?;
        let committed_target = match committed.status.code() {
            Some(0) => git_ok(root, &["ls-tree", "-z", "--name-only", "HEAD"])?
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

    fn bundle(&self, path: &Path, verify_in: &Path) -> Result<()> {
        if git_line(self.root, &["rev-parse", "--is-shallow-repository"])? == "true" {
            bail!("浅克隆仓库无法归档为完整 Bundle，请先补全 Git 历史。");
        }
        let branch = git_line(self.root, &["symbolic-ref", "HEAD"])?;
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
            )?,
        )?;
        checked("git bundle verify", git(verify_in, bundle("verify"))?)?;
        OpenOptions::new()
            .write(true)
            .open(path)
            .and_then(|file| file.sync_all())
            .with_context(|| format!("同步 Bundle {} 失败", path.display()))?;
        Ok(())
    }

    fn has_changes(&self) -> Result<bool> {
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
        )?
        .is_empty())
    }

    fn backup(&self) -> Result<()> {
        if !self.has_changes()? {
            println!("工作区干净，无需备份。");
            return Ok(());
        }
        let timestamp = Timestamp::now().strftime("%Y-%m-%dT%H:%M:%SZ");
        let message = format!("backup: {timestamp}");
        self.stage_backup()
            .and_then(|_| git_ok(self.root, &["commit", "-m", &message]))
            .context("备份失败，已保留当前工作区。请运行 git status 检查现场。")?;
        println!("已提交 {message}");
        Ok(())
    }

    fn stage_backup(&self) -> Result<()> {
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
        )?;
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
        let sent = child
            .stdin
            .take()
            .expect("piped git stdin")
            .write_all(&paths);
        let output = child.wait_with_output().context("等待 git add 失败")?;
        checked("git add", output)?;
        sent.context("发送备份文件列表失败")
    }

    fn init_project(&self, package: &str) -> Result<()> {
        // Generate in an isolated directory so Cargo cannot adopt leftovers or edit a parent workspace.
        let scaffold = tempfile::tempdir().context("创建临时工程目录失败")?;
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
        run_command(scaffold.path(), "cargo", args)
            .and_then(|output| checked("cargo init", output))?;

        let mut manifest =
            fs::read(scaffold.path().join("Cargo.toml")).context("读取新工程的 Cargo.toml 失败")?;
        manifest.extend_from_slice(b"\n[workspace]\n");
        let main = fs::read(scaffold.path().join("src/main.rs"))
            .context("读取新工程的 src/main.rs 失败")?;

        fs::create_dir(self.root.join("src")).context("创建工程的 src 目录失败")?;
        for (name, data) in [
            ("Cargo.toml", manifest.as_slice()),
            ("src/main.rs", main.as_slice()),
            (".gitignore", b"/target/\n"),
        ] {
            write_new(&self.root.join(name), data).with_context(|| format!("写入 {name} 失败"))?;
        }
        git_ok(
            self.root,
            &["add", "-f", "--", ".gitignore", "Cargo.toml", "src/main.rs"],
        )?;
        git_ok(self.root, &["commit", "-m", "init: cargo init"]).context("提交初始工程失败")?;
        Ok(())
    }

    fn add_dependencies(&self, deps: &[String]) -> Result<()> {
        let args = ["add", "--manifest-path", "Cargo.toml", "--"]
            .into_iter()
            .chain(deps.iter().map(String::as_str));
        run_command(self.root, "cargo", args).and_then(|output| checked("cargo add", output))?;
        let lock_tracked = !git_ok(self.root, &["ls-files", "-z", "--", "Cargo.lock"])?.is_empty();
        let lock_on_disk = self
            .root
            .join("Cargo.lock")
            .try_exists()
            .context("检查 Cargo.lock 失败")?;
        let mut paths = vec!["Cargo.toml"];
        if lock_on_disk || lock_tracked {
            paths.push("Cargo.lock");
        }
        let mut stage = vec!["add", "-A", "-f", "--"];
        stage.extend(&paths);
        git_ok(self.root, &stage).context("暂存依赖文件失败")?;

        let message = format!("deps: add {}", deps.join(" "));
        let mut commit = vec!["commit", "--only", "--allow-empty", "-m", &message, "--"];
        commit.extend(paths);
        git_ok(self.root, &commit).context("依赖提交失败")?;
        Ok(())
    }
}

fn write_new(path: &Path, data: &[u8]) -> io::Result<()> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?
        .write_all(data)
}

fn lock_home(home: &Path) -> Result<File> {
    let path = home.join(".cargo-play.lock");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("打开锁文件 {} 失败", path.display()))?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(TryLockError::WouldBlock) => {
            bail!("试验仓库正在使用，请等另一条 cargo-play 命令结束后重试。")
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
    crate::tests::configure_git(&mut command);
    // A nonempty value disables terminal color in the subprocess output.
    if env::var_os("NO_COLOR").is_none() {
        command.env("NO_COLOR", "1");
    }
    command
}

fn run_command(
    root: &Path,
    program: &str,
    args: impl IntoIterator<Item = impl AsRef<OsStr>>,
) -> Result<Output> {
    command(root, program, args)
        .output()
        .with_context(|| format!("无法执行 {program}"))
}

fn git(root: &Path, args: impl IntoIterator<Item = impl AsRef<OsStr>>) -> Result<Output> {
    run_command(root, "git", args)
}

fn git_ok(root: &Path, args: &[&str]) -> Result<Vec<u8>> {
    checked(&format!("git {}", args[0]), git(root, args)?)
}

fn git_line(root: &Path, args: &[&str]) -> Result<String> {
    line(git_ok(root, args)?)
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

    #[test]
    fn restores_play_when_the_replacement_cannot_be_installed() {
        let home = tempfile::tempdir().unwrap();
        let playground = Playground::open(
            Some(home.path().as_os_str().to_owned()),
            true,
            |_, _| Ok(()),
        )
        .unwrap();
        fs::create_dir(&playground.root).unwrap();
        fs::write(playground.root.join("keep.txt"), "old project").unwrap();
        let staging = tempfile::tempdir_in(home.path()).unwrap();
        let missing = staging.path().join("next");
        let error = playground.replace(staging, &missing, true).unwrap_err();
        assert!(error.to_string().contains("启用新工作区失败"), "{error:#}");
        assert_eq!(
            error.downcast_ref::<io::Error>().map(io::Error::kind),
            Some(io::ErrorKind::NotFound)
        );
        assert_eq!(
            fs::read_to_string(playground.root.join("keep.txt")).unwrap(),
            "old project"
        );
    }
}
