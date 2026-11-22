//! 提供目录检查、文件移动、独占写入和暂存目录清理等文件系统操作。

use std::{ffi::OsString, io, path::Path};

use anyhow::{Context, Error, Result, bail};
use tempfile::TempDir;
use tokio::{fs, io::AsyncWriteExt, task::spawn_blocking};

pub(super) async fn temporary_dir(parent: Option<&Path>, prefix: &'static str) -> Result<TempDir> {
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

pub(super) async fn close_tempdir(dir: TempDir) {
    let path = dir.path().to_owned();
    if let Err(error) = spawn_blocking(move || dir.close().map_err(Error::from))
        .await
        .map_err(Error::from)
        .and_then(|result| result)
    {
        eprintln!("警告：清理 {} 失败：{error}", path.display());
    }
}

pub(super) async fn entry_names(path: &Path) -> Result<Vec<OsString>> {
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

pub(super) async fn rename_entry(from: &Path, to: &Path) -> io::Result<()> {
    fs::rename(from, to).await
}

pub(super) async fn move_entry(
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

pub(super) fn is_reparse_point(info: &std::fs::Metadata) -> bool {
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

pub(super) async fn write_new(path: &Path, data: &[u8]) -> io::Result<()> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .await?;
    file.write_all(data).await?;
    file.flush().await
}
