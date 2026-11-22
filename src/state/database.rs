use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use sea_orm::{
    ConnectionTrait, DatabaseConnection, SqlxSqliteConnector, TransactionTrait,
    sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions},
};

use super::State;

const DB_FILE: &str = "workspace.sqlite3";
const SCHEMA: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS playgrounds (
        id BLOB NOT NULL PRIMARY KEY,
        name TEXT NOT NULL COLLATE BINARY,
        archived_path TEXT,
        last_visit_time TEXT NOT NULL
    )",
    "CREATE INDEX IF NOT EXISTS playgrounds_name ON playgrounds (name)",
    "CREATE INDEX IF NOT EXISTS playgrounds_visit_order
        ON playgrounds (rtrim(last_visit_time, 'Z') DESC, id ASC)",
    "CREATE TABLE IF NOT EXISTS current_playground (
        slot INTEGER NOT NULL PRIMARY KEY CHECK (slot = 1),
        playground_id BLOB NOT NULL REFERENCES playgrounds(id)
    )",
];

impl State {
    pub async fn open(home: &Path) -> Result<Self> {
        let path = home.join(DB_FILE);
        let db = connect(&path, false, true).await?;
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
        let path = existing_path(home).await?;
        Ok(Self(connect(&path, false, false).await?))
    }
}

pub(super) async fn connect(
    path: &Path,
    read_only: bool,
    create: bool,
) -> Result<DatabaseConnection> {
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

pub(super) async fn existing_path(home: &Path) -> Result<PathBuf> {
    let path = home.join(DB_FILE);
    ensure!(
        tokio::fs::try_exists(&path)
            .await
            .with_context(|| format!("检查状态数据库 {} 失败", path.display()))?,
        "首次使用请运行 cgpl init。"
    );
    Ok(path)
}
