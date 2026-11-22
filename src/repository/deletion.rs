//! 删除当前或已归档的游乐场，协调确认、文件暂存和数据库记录清理。

use super::{
    Playground, PlaygroundTarget, archive_path,
    filesystem::{directory_exists, is_reparse_point, rename_entry, temporary_dir},
    workspace::WorkspaceChanges,
};
use anyhow::{Context, Result, bail, ensure};
use std::{io, path::PathBuf};
use tokio::fs;

use crate::state::{PlaygroundRecord, State};

impl Playground {
    pub async fn delete(
        &self,
        target: Option<PlaygroundTarget>,
        confirm: impl FnOnce(&PlaygroundRecord, bool) -> Result<bool>,
    ) -> Result<()> {
        let state = State::open_existing(&self.home).await?;
        let current = state.active().await?;
        let (record, is_current) = match target {
            Some(target) => {
                let record = Self::resolve_target(&state, target).await?;
                let is_current = current.is_some_and(|current| current.id == record.id);
                (record, is_current)
            }
            None => (
                current.context("没有当前游乐场，请运行 cgpl list 查看。")?,
                true,
            ),
        };

        if !confirm(&record, is_current)? {
            println!("已取消删除。");
            return Ok(());
        }

        if is_current {
            self.verify_current(record.id).await?;
        }
        let bundle = self.bundle_to_delete(&record).await?;
        let staging = temporary_dir(Some(&self.home), ".cargo-play-delete-").await?;
        let mut changes = WorkspaceChanges::default();
        let result = async {
            let txn = state.stage_delete(record.id).await?;
            if let Some(bundle) = bundle {
                changes
                    .move_entry(
                        &bundle,
                        &staging.path().join("old.bundle"),
                        &mut rename_entry,
                    )
                    .await?;
            }
            if is_current {
                let empty = staging.path().join("empty");
                fs::create_dir(&empty).await.context("创建空工作区失败")?;
                changes
                    .switch_worktree(&self.root, staging.path(), &empty, true, rename_entry)
                    .await?;
            }
            txn.commit().await.context("提交游乐场删除事务失败")
        }
        .await;
        changes.finish(staging, result).await?;
        println!("已删除游乐场 {:?} ({})", record.name, record.id);
        Ok(())
    }

    async fn bundle_to_delete(&self, record: &PlaygroundRecord) -> Result<Option<PathBuf>> {
        let Some(stored) = &record.archived_path else {
            return Ok(None);
        };
        let expected = archive_path(record.id);
        ensure!(
            *stored == expected,
            "游乐场 {} 的归档路径不匹配：{stored:?}",
            record.id
        );
        if !directory_exists(&self.home.join("history")).await? {
            return Ok(None);
        }
        let path = self.home.join(expected);
        match fs::symlink_metadata(&path).await {
            Ok(info) if info.is_file() && !is_reparse_point(&info) => Ok(Some(path)),
            Ok(_) => bail!("归档路径 {} 必须是普通文件。", path.display()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => {
                Err(error).with_context(|| format!("检查归档路径 {} 失败", path.display()))
            }
        }
    }
}
