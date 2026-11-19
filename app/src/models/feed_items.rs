use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "feed_items")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub feed_id: Uuid,
    /// `token::hash` of the item's guid, else its link, else its title.
    #[sea_orm(primary_key, auto_increment = false, column_type = "Text")]
    pub guid_hash: String,
    /// Creates of its posts that were refused; at `feeds::MAX_TRIES` it is `seen`.
    pub failed: i32,
    /// Posted, or there when the feed was added: never posted (again).
    pub seen: bool,
    pub created_at: DateTimeWithTimeZone,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
