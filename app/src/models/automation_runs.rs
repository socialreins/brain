use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "automation_runs")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: Uuid,
    pub automation_id: Uuid,
    pub inbox_item_id: Uuid,
    pub team_id: Uuid,
    /// `running`, `waiting` (a flow at a `wait` step), `sent`, `failed` or `skipped`.
    #[sea_orm(column_type = "Text")]
    pub status: String,
    #[sea_orm(column_type = "Text", nullable)]
    pub reason: Option<String>,
    /// One `{type, status, reason, remote_id}` per action tried.
    #[sea_orm(column_type = "JsonBinary")]
    pub results: Json,
    /// A flow's step last tried, from 0; null for an automation with `actions`.
    pub step_index: Option<i32>,
    /// When a `waiting` run goes on.
    pub resume_at: Option<DateTimeWithTimeZone>,
    pub created_at: DateTimeWithTimeZone,
    pub updated_at: DateTimeWithTimeZone,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
