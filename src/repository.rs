use std::{
    env,
    ffi::{OsStr, OsString},
    fs::{self, File, OpenOptions, TryLockError},
    io::{self, Write},
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};

use crate::{Result, config::resolve_home};

const TARGET_PATHSPEC: &str = ":(top,icase)target";
const EXCLUDE_TARGET: &str = ":(top,icase,exclude)target";

pub struct Repository {
    pub root: PathBuf,
    _lock: File,
}

impl Repository {
    pub fn open(
        home: Option<OsString>,
        create: bool,
        prepare_config: impl FnOnce(&Path, bool) -> Result<()>,
    ) -> Result<Self> {
        let home = resolve_home(home)?;
        let root = home.join("play");
        let git_dir = root.join(".git");
        if create {
            fs::create_dir_all(&root)
                .map_err(|error| format!("创建 {} 失败：{error}", root.display()))?;
            match fs::create_dir(&git_dir) {
                Ok(()) => (),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => (),
                Err(error) => return Err(format!("创建 {} 失败：{error}", git_dir.display())),
            }
        }
        if !git_dir.is_dir() {
            return Err(format!(
                "{} 必须包含独立的 .git 目录。首次使用请运行 cargo-play init。",
                root.display()
            ));
        }

        let lock = lock_worktree(&git_dir)?;
        prepare_config(&home, create)?;
        let has_git_head = git_dir
            .join("HEAD")
            .try_exists()
            .map_err(|error| format!("检查 Git HEAD 失败：{error}"))?;
        if create && !has_git_head {
            git_ok(&root, &["init", "--initial-branch=archive"])?;
        }
        let top = git_line(&root, &["rev-parse", "--show-toplevel"])
            .map_err(|error| format!("{} 不是可用的 Git 工作区：{error}", root.display()))?;
        let canonical = |path: &Path| {
            path.canonicalize()
                .map_err(|error| format!("解析 {} 失败：{error}", path.display()))
        };
        if canonical(&root)? != canonical(Path::new(&top))? {
            return Err(format!("{} 必须是独立的 Git 仓库根目录。", root.display()));
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
                .map_err(|error| format!("检查 Git 状态失败：{error}"))?
            {
                return Err(format!(
                    "有未完成的 Git 操作（{marker}），请先完成或中止该操作。"
                ));
            }
        }
        let head = git(&root, ["symbolic-ref", "--quiet", "HEAD"])?;
        match head.status.code() {
            Some(0) => (),
            Some(1) => return Err("当前处于分离 HEAD，请先切换到本地分支。".into()),
            _ => return Err(command_error("git symbolic-ref", &head)),
        }
        let staged_target = !git_ok(&root, &["ls-files", "-z", "--", TARGET_PATHSPEC])?.is_empty();
        let committed = git(&root, ["rev-parse", "--verify", "--quiet", "HEAD"])?;
        let committed_target = match committed.status.code() {
            Some(0) => git_ok(&root, &["ls-tree", "-z", "--name-only", "HEAD"])?
                .split(|&byte| byte == 0)
                .any(|entry| entry.eq_ignore_ascii_case(b"target")),
            Some(1) => false,
            _ => return Err(command_error("git rev-parse", &committed)),
        };
        if staged_target || committed_target {
            return Err("Git 正在跟踪 target。请先通过 git rm -r --cached -- target 取消跟踪并提交，以保留磁盘文件。".into());
        }
        Ok(Self { root, _lock: lock })
    }

    pub fn has_changes(&self) -> Result<bool> {
        Ok(!git_ok(
            &self.root,
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

    pub fn backup(&self, timestamp: &str) -> Result<()> {
        if !self.has_changes()? {
            println!("工作区干净，无需备份。");
            return Ok(());
        }
        let message = format!("backup: {timestamp}");
        self.stage_backup()
            .and_then(|_| git_ok(&self.root, &["commit", "-m", &message]))
            .map_err(|error| {
                format!("备份失败，仍在原分支。请运行 git status 检查现场。\n{error}")
            })?;
        println!("已提交 {message}");
        Ok(())
    }

    fn stage_backup(&self) -> Result<()> {
        // Enumerate paths first: git add reports an ignored target as an error even with an exclusion pathspec.
        let paths = git_ok(
            &self.root,
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
            &self.root,
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
        .map_err(|error| format!("无法执行 git add：{error}"))?;
        let sent = child
            .stdin
            .take()
            .expect("piped git stdin")
            .write_all(&paths);
        let output = child
            .wait_with_output()
            .map_err(|error| format!("等待 git add 失败：{error}"))?;
        checked("git add", output)?;
        sent.map_err(|error| format!("发送备份文件列表失败：{error}"))
    }

    pub fn new_branch(&self, base: &str) -> Result<String> {
        let mut branch = base.to_owned();
        for suffix in 2.. {
            let branch_ref = format!("refs/heads/{branch}");
            let exists = git(&self.root, ["show-ref", "--verify", "--quiet", &branch_ref])?;
            match exists.status.code() {
                Some(0) => branch = format!("{base}-{suffix}"),
                Some(1) => break,
                _ => return Err(command_error("git show-ref", &exists)),
            }
        }
        git_ok(
            &self.root,
            &["switch", "--no-overwrite-ignore", "--orphan", &branch],
        )
        .map_err(|error| format!("创建试验分支 {branch} 失败：{error}"))?;
        Ok(branch)
    }

    pub fn init_project(&self) -> Result<()> {
        // Generate in an isolated directory so Cargo cannot adopt leftovers or edit a parent workspace.
        let scaffold =
            tempfile::tempdir().map_err(|error| format!("创建临时工程目录失败：{error}"))?;
        let args = [
            "init",
            "--vcs",
            "none",
            "--bin",
            "--name",
            "play",
            "--edition",
            "2024",
            ".",
        ];
        run_command(scaffold.path(), "cargo", args)
            .and_then(|output| checked("cargo init", output))?;

        for entry in ["Cargo.toml", "Cargo.lock", "src", ".gitignore"] {
            let path = self.root.join(entry);
            match fs::symlink_metadata(&path) {
                Ok(_) => {
                    return Err(format!(
                        "{} 已存在，请移开该文件或目录后重试。",
                        path.display()
                    ));
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => (),
                Err(error) => return Err(format!("检查 {} 失败：{error}", path.display())),
            }
        }
        let mut manifest = fs::read(scaffold.path().join("Cargo.toml"))
            .map_err(|error| format!("读取新工程的 Cargo.toml 失败：{error}"))?;
        manifest.extend_from_slice(b"\n[workspace]\n");
        let main = fs::read(scaffold.path().join("src/main.rs"))
            .map_err(|error| format!("读取新工程的 src/main.rs 失败：{error}"))?;

        fs::create_dir(self.root.join("src"))
            .map_err(|error| format!("创建工程的 src 目录失败：{error}"))?;
        for (name, data) in [
            ("Cargo.toml", manifest.as_slice()),
            ("src/main.rs", main.as_slice()),
            (".gitignore", b"/target/\n"),
        ] {
            write_new(&self.root.join(name), data)
                .map_err(|error| format!("写入 {name} 失败：{error}"))?;
        }
        git_ok(
            &self.root,
            &["add", "-f", "--", ".gitignore", "Cargo.toml", "src/main.rs"],
        )?;
        git_ok(&self.root, &["commit", "-m", "init: cargo init"])
            .map_err(|error| format!("提交初始工程失败：{error}"))?;
        Ok(())
    }

    pub fn add_dependencies(&self, deps: &[String]) -> Result<()> {
        let args = ["add", "--manifest-path", "Cargo.toml", "--"]
            .into_iter()
            .chain(deps.iter().map(String::as_str));
        run_command(&self.root, "cargo", args).and_then(|output| checked("cargo add", output))?;
        let lock_tracked = !git_ok(&self.root, &["ls-files", "-z", "--", "Cargo.lock"])?.is_empty();
        let lock_on_disk = self
            .root
            .join("Cargo.lock")
            .try_exists()
            .map_err(|error| format!("检查 Cargo.lock 失败：{error}"))?;
        let mut paths = vec!["Cargo.toml"];
        if lock_on_disk || lock_tracked {
            paths.push("Cargo.lock");
        }
        let mut stage = vec!["add", "-A", "-f", "--"];
        stage.extend(&paths);
        git_ok(&self.root, &stage).map_err(|error| format!("暂存依赖文件失败：{error}"))?;

        let message = format!("deps: add {}", deps.join(" "));
        let mut commit = vec!["commit", "--only", "--allow-empty", "-m", &message, "--"];
        commit.extend(paths);
        git_ok(&self.root, &commit).map_err(|error| format!("依赖提交失败：{error}"))?;
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

fn lock_worktree(git_dir: &Path) -> Result<File> {
    let path = git_dir.join("cargo-play.lock");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .map_err(|error| format!("打开锁文件 {} 失败：{error}", path.display()))?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(TryLockError::WouldBlock) => {
            Err("试验仓库正在使用，请等另一条 cargo-play 命令结束后重试。".into())
        }
        Err(TryLockError::Error(error)) => Err(format!("锁定试验仓库失败：{error}")),
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
        .map_err(|error| format!("无法执行 {program}：{error}"))
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
        .map_err(|error| format!("Git 文本输出不是 UTF-8：{error}"))
}

fn checked(label: &str, output: Output) -> Result<Vec<u8>> {
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(command_error(label, &output))
    }
}

fn command_error(label: &str, output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let details = [stderr.trim(), stdout.trim()]
        .into_iter()
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>();
    if details.is_empty() {
        format!("{label} 失败（{}）", output.status)
    } else {
        format!("{label} 失败（{}）\n{}", output.status, details.join("\n"))
    }
}
