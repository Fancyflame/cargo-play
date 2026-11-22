use std::{
    cmp::Reverse,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use jiff::Timestamp;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, DatabaseConnection, DatabaseTransaction,
    DbBackend, EntityTrait, FromQueryResult, QueryFilter, QuerySelect, Set, SqlxSqliteConnector,
    Statement, TransactionTrait,
    sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions},
};
use uuid::Uuid;

mod playground {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "playgrounds")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: Uuid,
        pub name: String,
        pub archived_path: Option<String>,
        pub archived_at: Option<String>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

mod current {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "current_playground")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub slot: i32,
        pub playground_id: Uuid,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

pub use playground::Model as PlaygroundRecord;

const DB_FILE: &str = "workspace.sqlite3";
const SCHEMA: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS playgrounds (
        id BLOB NOT NULL PRIMARY KEY,
        name TEXT NOT NULL COLLATE BINARY,
        archived_path TEXT,
        archived_at TEXT,
        CHECK ((archived_path IS NULL) = (archived_at IS NULL))
    )",
    "CREATE INDEX IF NOT EXISTS playgrounds_name ON playgrounds (name)",
    "CREATE TABLE IF NOT EXISTS current_playground (
        slot INTEGER NOT NULL PRIMARY KEY CHECK (slot = 1),
        playground_id BLOB NOT NULL REFERENCES playgrounds(id)
    )",
];

pub struct State(DatabaseConnection);

#[derive(Debug, PartialEq, Eq)]
pub enum NameMatch {
    Missing,
    Unique(Uuid),
    Ambiguous,
}

pub struct ArchiveRecord {
    pub id: Uuid,
    pub path: String,
    pub timestamp: String,
}

pub struct PlaygroundListEntry {
    pub id: Uuid,
    pub name: String,
    pub archived_at: Option<Timestamp>,
}

#[derive(FromQueryResult)]
struct PlaygroundListRow {
    id: Uuid,
    name: String,
    archived_at: Option<String>,
}

impl State {
    pub async fn open(home: &Path) -> Result<Self> {
        let path = home.join(DB_FILE);
        let db = Self::connect(&path, false, true).await?;
        async {
            let txn = db.begin().await?;
            for stmt in SCHEMA {
                txn.execute_unprepared(stmt).await?;
            }
            txn.commit().await
        }
        .await
        .with_context(|| format!("初始化状态数据库 {} 失败", path.display()))?;
        Ok(Self(db))
    }

    pub async fn open_existing(home: &Path) -> Result<Self> {
        let path = Self::existing_path(home).await?;
        Ok(Self(Self::connect(&path, false, false).await?))
    }

    async fn connect(path: &Path, read_only: bool, create: bool) -> Result<DatabaseConnection> {
        // A filename passed directly to SQLite avoids URL escaping of the home path.
        let options = SqliteConnectOptions::new()
            .filename(path)
            .read_only(read_only)
            .create_if_missing(create)
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .with_context(|| format!("打开状态数据库 {} 失败", path.display()))?;
        Ok(SqlxSqliteConnector::from_sqlx_sqlite_pool(pool))
    }

    async fn existing_path(home: &Path) -> Result<PathBuf> {
        let path = home.join(DB_FILE);
        ensure!(
            tokio::fs::try_exists(&path)
                .await
                .with_context(|| format!("检查状态数据库 {} 失败", path.display()))?,
            "首次使用请运行 cargo-play init。"
        );
        Ok(path)
    }

    pub async fn list(home: &Path) -> Result<Vec<PlaygroundListEntry>> {
        let path = Self::existing_path(home).await?;
        let db = Self::connect(&path, true, false).await?;
        let rows = PlaygroundListRow::find_by_statement(Statement::from_string(
            DbBackend::Sqlite,
            "SELECT p.id, p.name, CASE WHEN c.slot IS NULL THEN p.archived_at END AS archived_at
             FROM playgrounds AS p
             LEFT JOIN current_playground AS c ON c.slot = 1 AND c.playground_id = p.id
             WHERE c.slot IS NOT NULL OR p.archived_at IS NOT NULL",
        ))
        .all(&db)
        .await
        .context("读取游乐场列表失败")?;

        let mut entries = Vec::with_capacity(rows.len());
        for row in rows {
            let archived_at = row
                .archived_at
                .as_deref()
                .map(str::parse::<Timestamp>)
                .transpose()
                .with_context(|| format!("游乐场 {} 的归档时间无效", row.id))?;
            entries.push(PlaygroundListEntry {
                id: row.id,
                name: row.name,
                archived_at,
            });
        }
        entries.sort_unstable_by_key(|entry| {
            (
                entry.archived_at.is_some(),
                Reverse(entry.archived_at),
                entry.id,
            )
        });
        Ok(entries)
    }

    pub async fn active(&self) -> Result<Option<PlaygroundRecord>> {
        let Some(active) = current::Entity::find_by_id(1).one(&self.0).await? else {
            return Ok(None);
        };
        let record = self
            .by_id(active.playground_id)
            .await?
            .context("当前游乐场的数据库记录不存在")?;
        Ok(Some(record))
    }

    pub async fn by_id(&self, id: Uuid) -> Result<Option<PlaygroundRecord>> {
        Ok(playground::Entity::find_by_id(id).one(&self.0).await?)
    }

    // Names are not unique; commands that select a playground use its UUID.
    pub async fn find_by_name(&self, name: &str) -> Result<NameMatch> {
        let matches = playground::Entity::find()
            .filter(playground::Column::Name.eq(name))
            .select_only()
            .column(playground::Column::Id)
            .limit(2)
            .into_tuple::<Uuid>()
            .all(&self.0)
            .await?;
        Ok(match matches.as_slice() {
            [] => NameMatch::Missing,
            [id] => NameMatch::Unique(*id),
            _ => NameMatch::Ambiguous,
        })
    }

    pub async fn next_default_name(&self, base: &str) -> Result<String> {
        for name in std::iter::once(base.to_owned()).chain((2..).map(|n| format!("{base}-{n}"))) {
            if self.find_by_name(&name).await? == NameMatch::Missing {
                return Ok(name);
            }
        }
        unreachable!("the sequence of default names is unbounded")
    }

    pub async fn stage_switch(
        &self,
        id: Uuid,
        name: &str,
        archive: Option<&ArchiveRecord>,
    ) -> Result<DatabaseTransaction> {
        async {
            let txn = self.0.begin().await?;
            Self::record_archive(&txn, archive).await?;
            playground::ActiveModel {
                id: Set(id),
                name: Set(name.to_owned()),
                archived_path: Set(None),
                archived_at: Set(None),
            }
            .insert(&txn)
            .await?;
            Self::set_current(&txn, id).await?;
            Ok::<_, sea_orm::DbErr>(txn)
        }
        .await
        .context("写入工作区状态失败")
    }

    pub async fn stage_clear(
        &self,
        archive: Option<&ArchiveRecord>,
    ) -> Result<DatabaseTransaction> {
        async {
            let txn = self.0.begin().await?;
            Self::record_archive(&txn, archive).await?;
            current::Entity::delete_by_id(1).exec(&txn).await?;
            Ok::<_, sea_orm::DbErr>(txn)
        }
        .await
        .context("清除当前游乐场状态失败")
    }

    pub async fn activate(&self, id: Uuid) -> Result<()> {
        Self::set_current(&self.0, id)
            .await
            .with_context(|| format!("设置当前游乐场 {id} 失败"))
    }

    async fn record_archive(
        txn: &DatabaseTransaction,
        archive: Option<&ArchiveRecord>,
    ) -> Result<(), sea_orm::DbErr> {
        if let Some(archive) = archive {
            playground::ActiveModel {
                id: Set(archive.id),
                archived_path: Set(Some(archive.path.clone())),
                archived_at: Set(Some(archive.timestamp.clone())),
                ..Default::default()
            }
            .update(txn)
            .await?;
        }
        Ok(())
    }

    async fn set_current(db: &impl ConnectionTrait, id: Uuid) -> Result<(), sea_orm::DbErr> {
        current::Entity::insert(current::ActiveModel {
            slot: Set(1),
            playground_id: Set(id),
        })
        .on_conflict(
            sea_orm::sea_query::OnConflict::column(current::Column::Slot)
                .update_column(current::Column::PlaygroundId)
                .to_owned(),
        )
        .exec(db)
        .await?;
        Ok(())
    }

    #[cfg(test)]
    pub fn connection(&self) -> &DatabaseConnection {
        &self.0
    }
}
