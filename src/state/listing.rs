use std::{num::NonZeroU64, path::Path};

use anyhow::{Context, Result};
use jiff::Timestamp;
use sea_orm::{ConnectionTrait, DbBackend, EntityTrait, FromQueryResult, Statement};
use uuid::Uuid;

use super::{
    PlaygroundRecord, State,
    database::{connect, existing_path},
    entities::playground,
};

const LIST_PAGE_SIZE: i64 = 50;
// Timestamp::to_string() writes canonical UTC timestamps with optional fractional
// seconds. Removing the terminal Z lets an exact second sort before its fractions.
const ORDERED_PLAYGROUNDS: &str = "FROM playgrounds
    ORDER BY rtrim(last_visit_time, 'Z') DESC, id ASC";

pub struct PlaygroundListEntry {
    pub id: Uuid,
    pub name: String,
    pub last_visit_time: Timestamp,
}

#[derive(FromQueryResult)]
struct PlaygroundListRow {
    id: Uuid,
    name: String,
    last_visit_time: String,
}

impl State {
    pub async fn list(home: &Path, offset: u64) -> Result<Vec<PlaygroundListEntry>> {
        let path = existing_path(home).await?;
        let db = connect(&path, true, false).await?;
        let Ok(offset) = i64::try_from(offset) else {
            return Ok(Vec::new());
        };
        let rows = PlaygroundListRow::find_by_statement(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            format!("SELECT id, name, last_visit_time {ORDERED_PLAYGROUNDS} LIMIT ? OFFSET ?"),
            [LIST_PAGE_SIZE.into(), offset.into()],
        ))
        .all(&db)
        .await
        .context("读取演练场列表失败")?;
        rows.into_iter()
            .map(|row| {
                let last_visit_time = row
                    .last_visit_time
                    .parse::<Timestamp>()
                    .with_context(|| format!("演练场 {} 的上次访问时间无效", row.id))?;
                Ok(PlaygroundListEntry {
                    id: row.id,
                    name: row.name,
                    last_visit_time,
                })
            })
            .collect()
    }

    pub async fn oldest_archives(&self) -> Result<Vec<PlaygroundRecord>> {
        // The reverse of the list order, served by the same visit index.
        playground::Entity::find()
            .from_raw_sql(Statement::from_string(
                DbBackend::Sqlite,
                "SELECT id, name, archived_path, last_visit_time FROM playgrounds
                 WHERE archived_path IS NOT NULL
                   AND id NOT IN (SELECT playground_id FROM current_playground)
                 ORDER BY rtrim(last_visit_time, 'Z') ASC, id DESC",
            ))
            .all(&self.0)
            .await
            .context("读取待清理的归档列表失败")
    }

    // The caller holds the workspace lock while resolving a position to a UUID.
    pub async fn id_at_position(&self, position: NonZeroU64) -> Result<Option<Uuid>> {
        let Ok(offset) = i64::try_from(position.get() - 1) else {
            return Ok(None);
        };
        self.0
            .query_one(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                format!("SELECT id {ORDERED_PLAYGROUNDS} LIMIT 1 OFFSET ?"),
                [offset.into()],
            ))
            .await
            .and_then(|row| row.map(|row| row.try_get("", "id")).transpose())
            .with_context(|| format!("查询演练场序号 {position} 失败"))
    }
}
