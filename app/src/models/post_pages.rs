use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "post_pages")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: Uuid,
    pub team_id: Uuid,
    /// The post's `group_id` when it has one, else its `id`: a group has one page.
    #[sea_orm(unique)]
    pub subject: Uuid,
    /// The page's public address: 10 characters of the team code alphabet, permanent.
    #[sea_orm(column_type = "Text", unique)]
    pub code: String,
    #[sea_orm(column_type = "Text")]
    pub details: String,
    pub enabled: bool,
    pub created_at: DateTimeWithTimeZone,
    pub updated_at: DateTimeWithTimeZone,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
