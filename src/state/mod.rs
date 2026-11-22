mod database;
mod entities;
mod listing;

use anyhow::{Context, Result, ensure};
use jiff::Timestamp;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, DatabaseConnection, DatabaseTransaction,
    EntityTrait, QueryFilter, QuerySelect, Set, TransactionTrait,
};
use uuid::Uuid;

pub use entities::playground::Model as PlaygroundRecord;
use entities::{current, playground};

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
}

impl State {
    pub async fn active(&self) -> Result<Option<PlaygroundRecord>> {
        let Some(active) = current::Entity::find_by_id(1).one(&self.0).await? else {
            return Ok(None);
        };
        let record = self
            .by_id(active.playground_id)
            .await?
            .context("当前演练场的数据库记录不存在")?;
        Ok(Some(record))
    }

    pub async fn by_id(&self, id: Uuid) -> Result<Option<PlaygroundRecord>> {
        Ok(playground::Entity::find_by_id(id).one(&self.0).await?)
    }

    // Names are not unique and are only used to generate available default names.
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

    pub async fn stage_new(&self, id: Uuid, name: &str) -> Result<DatabaseTransaction> {
        async {
            let txn = self.0.begin().await?;
            playground::ActiveModel {
                id: Set(id),
                name: Set(name.to_owned()),
                archived_path: Set(None),
                last_visit_time: Set(Timestamp::now().to_string()),
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
        archived_at: Timestamp,
    ) -> Result<DatabaseTransaction> {
        async {
            let txn = self.0.begin().await?;
            if let Some(archive) = archive {
                playground::ActiveModel {
                    id: Set(archive.id),
                    archived_path: Set(Some(archive.path.clone())),
                    last_visit_time: Set(archived_at.to_string()),
                    ..Default::default()
                }
                .update(&txn)
                .await?;
            }
            current::Entity::delete_by_id(1).exec(&txn).await?;
            Ok::<_, sea_orm::DbErr>(txn)
        }
        .await
        .context("清除当前演练场状态失败")
    }

    pub async fn stage_delete(&self, id: Uuid) -> Result<DatabaseTransaction> {
        let txn = self.0.begin().await.context("开始删除事务失败")?;
        current::Entity::delete_many()
            .filter(current::Column::PlaygroundId.eq(id))
            .exec(&txn)
            .await
            .context("清除当前演练场状态失败")?;
        let result = playground::Entity::delete_by_id(id)
            .exec(&txn)
            .await
            .with_context(|| format!("删除演练场 {id} 的记录失败"))?;
        ensure!(result.rows_affected == 1, "找不到演练场 {id}");
        Ok(txn)
    }

    pub async fn activate(&self, id: Uuid) -> Result<()> {
        async {
            let txn = self.0.begin().await?;
            playground::ActiveModel {
                id: Set(id),
                last_visit_time: Set(Timestamp::now().to_string()),
                ..Default::default()
            }
            .update(&txn)
            .await?;
            Self::set_current(&txn, id).await?;
            txn.commit().await
        }
        .await
        .with_context(|| format!("记录演练场 {id} 的访问失败"))
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
