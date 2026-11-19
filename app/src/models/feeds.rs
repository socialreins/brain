use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "feeds")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: Uuid,
    pub team_id: Uuid,
    pub social_set_id: Uuid,
    #[sea_orm(column_type = "Text")]
    pub url: String,
    /// The platforms each new item is posted to, as a JSON array of names.
    #[sea_orm(column_type = "JsonBinary")]
    pub platforms: Json,
    #[sea_orm(column_type = "Text")]
    pub template: String,
    /// `draft`, `queue` or `now`.
    #[sea_orm(column_type = "Text")]
    pub mode: String,
    pub active: bool,
    /// The feed's last `ETag` and `Last-Modified`, sent back so an unchanged feed answers `304`.
    #[sea_orm(column_type = "Text", nullable)]
    pub etag: Option<String>,
    #[sea_orm(column_type = "Text", nullable)]
    pub last_modified: Option<String>,
    pub last_checked_at: Option<DateTimeWithTimeZone>,
    #[sea_orm(column_type = "Text", nullable)]
    pub last_error: Option<String>,
    /// Failed checks in a row.
    pub failures: i32,
    pub items_posted: i32,
    /// The member who added it, whose posts its items become; `None` for an API token.
    pub created_by: Option<Uuid>,
    pub created_at: DateTimeWithTimeZone,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
