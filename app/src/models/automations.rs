use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "automations")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: Uuid,
    pub team_id: Uuid,
    #[sea_orm(column_type = "Text")]
    pub name: String,
    pub active: bool,
    /// When it was last switched on; items received before it never run it.
    pub activated_at: DateTimeWithTimeZone,
    /// `comment_received`.
    #[sea_orm(column_type = "Text")]
    pub trigger: String,
    /// `automations::Conditions` as JSON.
    #[sea_orm(column_type = "JsonBinary")]
    pub conditions: Json,
    /// A JSON list of `automations::Action`.
    #[sea_orm(column_type = "JsonBinary")]
    pub actions: Json,
    #[sea_orm(column_type = "Text", nullable)]
    pub fallback_public_reply: Option<String>,
    pub created_by: Option<Uuid>,
    pub created_at: DateTimeWithTimeZone,
    pub updated_at: DateTimeWithTimeZone,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
