//! 切换工作区、发布 Git Bundle，并在操作失败时回滚文件变更。

use std::{
    io,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use tempfile::TempDir;
use tokio::fs;
use uuid::Uuid;

use crate::state::{ArchiveRecord, PlaygroundRecord, State};

use super::{
    Playground, archive_path,
    filesystem::{close_tempdir, entry_names, is_reparse_point, move_entry, rename_entry},
    worktree::{Worktree, worktree_is_empty},
};

impl Playground {
    pub(super) async fn check_replacement(
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

    pub(super) async fn replace_worktree(
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
                    path: archive_path(record.id),
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
}

#[derive(Default)]
pub(super) struct WorkspaceChanges {
    moved: Vec<(PathBuf, PathBuf)>,
    archive: Option<(PathBuf, Option<PathBuf>)>,
}

impl WorkspaceChanges {
    pub(super) async fn switch_worktree(
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

    pub(super) async fn move_entry(
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

    pub(super) async fn finish(self, staging: TempDir, result: Result<()>) -> Result<()> {
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
