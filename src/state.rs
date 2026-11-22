use std::path::Path;

use anyhow::{Context, Result};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, DatabaseConnection, DatabaseTransaction,
    EntityTrait, QueryFilter, QuerySelect, Set, SqlxSqliteConnector, TransactionTrait,
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

impl State {
    pub async fn open(home: &Path) -> Result<Self> {
        let path = home.join(DB_FILE);
        // A filename passed directly to SQLite avoids URL escaping of the home path.
        let options = SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true)
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .with_context(|| format!("打开状态数据库 {} 失败", path.display()))?;
        let db = SqlxSqliteConnector::from_sqlx_sqlite_pool(pool);
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

    // Used by the future restore command. Ambiguous names require a UUID from the caller.
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
            if let Some(archive) = archive {
                playground::ActiveModel {
                    id: Set(archive.id),
                    archived_path: Set(Some(archive.path.clone())),
                    archived_at: Set(Some(archive.timestamp.clone())),
                    ..Default::default()
                }
                .update(&txn)
                .await?;
            }
            playground::ActiveModel {
                id: Set(id),
                name: Set(name.to_owned()),
                archived_path: Set(None),
                archived_at: Set(None),
            }
            .insert(&txn)
            .await?;
            current::Entity::insert(current::ActiveModel {
                slot: Set(1),
                playground_id: Set(id),
            })
            .on_conflict(
                sea_orm::sea_query::OnConflict::column(current::Column::Slot)
                    .update_column(current::Column::PlaygroundId)
                    .to_owned(),
            )
            .exec(&txn)
            .await?;
            Ok::<_, sea_orm::DbErr>(txn)
        }
        .await
        .context("写入工作区状态失败")
    }

    #[cfg(test)]
    pub fn connection(&self) -> &DatabaseConnection {
        &self.0
    }
}
