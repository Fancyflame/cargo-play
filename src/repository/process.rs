//! 执行 Git 和 Cargo 子进程，配置运行环境并处理命令输出。

use std::{env, ffi::OsStr, path::Path, process::Output};

use anyhow::{Context, Error, Result, anyhow};
use tokio::process::Command;

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

pub(super) fn command(
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

pub(super) async fn run_command(
    root: &Path,
    program: &str,
    args: impl IntoIterator<Item = impl AsRef<OsStr>>,
) -> Result<Output> {
    command(root, program, args)
        .output()
        .await
        .with_context(|| format!("无法执行 {program}"))
}

pub(super) async fn git(
    root: &Path,
    args: impl IntoIterator<Item = impl AsRef<OsStr>>,
) -> Result<Output> {
    run_command(root, "git", args).await
}

pub(super) async fn git_ok(root: &Path, args: &[&str]) -> Result<Vec<u8>> {
    checked(&format!("git {}", args[0]), git(root, args).await?)
}

pub(super) async fn git_line(root: &Path, args: &[&str]) -> Result<String> {
    line(git_ok(root, args).await?)
}

fn line(bytes: Vec<u8>) -> Result<String> {
    String::from_utf8(bytes)
        .map(|text| text.trim_end_matches(['\r', '\n']).to_owned())
        .context("Git 文本输出不是 UTF-8")
}

pub(super) fn checked(label: &str, output: Output) -> Result<Vec<u8>> {
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(command_error(label, &output))
    }
}

pub(super) fn command_error(label: &str, output: &Output) -> Error {
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
