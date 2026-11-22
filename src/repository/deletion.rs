//! 删除当前或已归档的演练场，协调确认、文件暂存和数据库记录清理。

use super::{
    Playground, PlaygroundTarget, archive_path,
    filesystem::{directory_exists, is_reparse_point, rename_entry, temporary_dir},
    workspace::WorkspaceChanges,
};
use anyhow::{Context, Result, bail, ensure};
use jiff::Timestamp;
use std::{io, path::PathBuf};
use tokio::fs;

use crate::{
    config::AutoDeleteConfig,
    state::{PlaygroundRecord, State},
};

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
                current.context("没有当前演练场，请运行 cgpl list 查看。")?,
                true,
            ),
        };

        if !confirm(&record, is_current)? {
            println!("已取消删除。");
            return Ok(());
        }

        self.delete_record(&state, &record, is_current).await
    }

    pub(super) async fn prune_archives(
        &self,
        state: &State,
        limits: &AutoDeleteConfig,
        archived_at: Timestamp,
    ) -> Result<()> {
        let max_bytes = limits.after_kbytes.map(|units| u128::from(units) * 1024);
        if limits.after_count.is_none() && limits.after_hours.is_none() && max_bytes.is_none() {
            return Ok(());
        }

        let archives = state.oldest_archives().await?;
        let mut sizes = vec![0; archives.len()];
        let mut total_bytes = 0;
        if max_bytes.is_some() {
            for (record, size) in archives.iter().zip(&mut sizes) {
                if let Some((_, bytes)) = self.bundle_metadata(record).await? {
                    *size = u128::from(bytes);
                    total_bytes += *size;
                }
            }
        }

        let mut remaining = archives.len();
        for (record, size) in archives.iter().zip(sizes) {
            let over_count = limits
                .after_count
                .is_some_and(|max| remaining > max as usize);
            let over_size = max_bytes.is_some_and(|max| total_bytes > max);
            let expired = if let Some(max_hours) = limits.after_hours {
                let last_visit = record
                    .last_visit_time
                    .parse::<Timestamp>()
                    .with_context(|| format!("演练场 {} 的上次访问时间无效", record.id))?;
                archived_at.duration_since(last_visit).as_secs_f64() / 3600.0 > max_hours
            } else {
                false
            };
            if !over_count && !over_size && !expired {
                break;
            }
            self.delete_record(state, record, false).await?;
            remaining -= 1;
            total_bytes -= size;
        }
        Ok(())
    }

    async fn delete_record(
        &self,
        state: &State,
        record: &PlaygroundRecord,
        is_current: bool,
    ) -> Result<()> {
        if is_current {
            self.verify_current(record.id).await?;
        }
        let bundle = self.bundle_metadata(record).await?;
        let staging = temporary_dir(Some(&self.home), ".cargo-play-delete-").await?;
        let mut changes = WorkspaceChanges::default();
        let result = async {
            let txn = state.stage_delete(record.id).await?;
            if let Some((bundle, _)) = bundle {
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
            txn.commit().await.context("提交演练场删除事务失败")
        }
        .await;
        changes.finish(staging, result).await?;
        println!("已删除演练场 {:?} ({})", record.name, record.id);
        Ok(())
    }

    async fn bundle_metadata(&self, record: &PlaygroundRecord) -> Result<Option<(PathBuf, u64)>> {
        let Some(stored) = &record.archived_path else {
            return Ok(None);
        };
        let expected = archive_path(record.id);
        ensure!(
            *stored == expected,
            "演练场 {} 的归档路径不匹配：{stored:?}",
            record.id
        );
        if !directory_exists(&self.home.join("history")).await? {
            return Ok(None);
        }
        let path = self.home.join(expected);
        match fs::symlink_metadata(&path).await {
            Ok(info) if info.is_file() && !is_reparse_point(&info) => Ok(Some((path, info.len()))),
            Ok(_) => bail!("归档路径 {} 必须是普通文件。", path.display()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => {
                Err(error).with_context(|| format!("检查归档路径 {} 失败", path.display()))
            }
        }
    }
}
