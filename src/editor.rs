use std::{
    env,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use anyhow::{Context, Result, ensure};

pub async fn open_folder(folder: &Path) -> Result<()> {
    let folder = folder.to_owned();
    tokio::task::spawn_blocking(move || open_folder_blocking(&folder))
        .await
        .context("等待编辑器启动失败")?
}

fn open_folder_blocking(folder: &Path) -> Result<()> {
    if let Some(code) = find_code() {
        match Command::new(code)
            .env_remove("ELECTRON_RUN_AS_NODE")
            .env_remove("VSCODE_DEV")
            .arg(folder)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(_) => return Ok(()),
            Err(error) => {
                return open_association(folder)
                    .with_context(|| format!("启动 VS Code 失败（{error}）"));
            }
        }
    }
    open_association(folder)
}

fn find_code() -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    env::split_paths(&path)
        .filter(|dir| dir.is_absolute())
        .find_map(|dir| {
            #[cfg(windows)]
            {
                let code = dir.join("Code.exe");
                if code.is_file() {
                    return Some(code);
                }
                if dir.join("code.cmd").is_file() {
                    let code = dir.parent()?.join("Code.exe");
                    if code.is_file() {
                        return Some(code);
                    }
                }
                None
            }
            #[cfg(not(windows))]
            {
                let code = dir.join("code");
                code.is_file().then_some(code)
            }
        })
}

#[cfg(windows)]
fn open_association(folder: &Path) -> Result<()> {
    use std::{os::windows::ffi::OsStrExt, ptr};
    use windows_sys::Win32::UI::{Shell::ShellExecuteW, WindowsAndMessaging::SW_SHOWNORMAL};

    let folder: Vec<u16> = folder
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    // The path is nul-terminated and remains valid for the duration of the call.
    let result = unsafe {
        ShellExecuteW(
            ptr::null_mut(),
            ptr::null(),
            folder.as_ptr(),
            ptr::null(),
            ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    ensure!(
        result as isize > 32,
        "无法使用 Windows 默认应用打开文件夹（ShellExecuteW 返回 {}）。",
        result as isize
    );
    Ok(())
}

#[cfg(not(windows))]
fn open_association(folder: &Path) -> Result<()> {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let status = Command::new(opener)
        .arg(folder)
        .status()
        .with_context(|| format!("无法启动 {opener}"))?;
    ensure!(status.success(), "{opener} 退出失败（{status}）。");
    Ok(())
}
