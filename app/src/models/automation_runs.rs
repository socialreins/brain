use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "automation_runs")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: Uuid,
    pub automation_id: Uuid,
    pub inbox_item_id: Uuid,
    pub team_id: Uuid,
    /// `running`, `sent`, `failed` or `skipped`.
    #[sea_orm(column_type = "Text")]
    pub status: String,
    #[sea_orm(column_type = "Text", nullable)]
    pub reason: Option<String>,
    /// One `{type, status, reason, remote_id}` per action tried.
    #[sea_orm(column_type = "JsonBinary")]
    pub results: Json,
    pub created_at: DateTimeWithTimeZone,
    pub updated_at: DateTimeWithTimeZone,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
