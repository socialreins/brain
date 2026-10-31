use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "post_lists")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: Uuid,
    pub team_id: Uuid,
    /// `null` for a team-wide list.
    pub social_set_id: Option<Uuid>,
    #[sea_orm(column_type = "Text")]
    pub name: String,
    /// Platform playlist ids by platform; reserved, nothing writes it yet.
    #[sea_orm(column_type = "JsonBinary")]
    pub remote_ids: Json,
    pub created_at: DateTimeWithTimeZone,
    pub updated_at: DateTimeWithTimeZone,
    pub deleted_at: Option<DateTimeWithTimeZone>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
