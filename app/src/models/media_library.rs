use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "media_library")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: Uuid,
    pub team_id: Uuid,
    /// A public `https://` URL; one row per team and URL.
    #[sea_orm(column_type = "Text")]
    pub url: String,
    #[sea_orm(column_type = "Text")]
    pub name: String,
    /// The member who added it; `None` for an API token or a removed user.
    pub created_by: Option<Uuid>,
    pub created_at: DateTimeWithTimeZone,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
