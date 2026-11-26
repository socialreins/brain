use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "inboxes")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: Uuid,
    /// The number web addresses use: one counter over every team, set by the column's identity, so an
    /// insert leaves it `NotSet`.
    pub number: i64,
    pub team_id: Uuid,
    #[sea_orm(column_type = "Text")]
    pub name: String,
    /// The team's "All" inbox: no filters, cannot be changed or deleted.
    pub is_all: bool,
    /// `inbox::Filters` as JSON.
    #[sea_orm(column_type = "JsonBinary")]
    pub filters: Json,
    pub created_at: DateTimeWithTimeZone,
    pub updated_at: DateTimeWithTimeZone,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
