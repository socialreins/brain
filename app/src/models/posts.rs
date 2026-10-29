use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "posts")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: Uuid,
    pub team_id: Uuid,
    pub social_set_id: Uuid,
    /// `bluesky` today; the CHECK also allows the platforms still to come.
    #[sea_orm(column_type = "Text")]
    pub platform: String,
    #[sea_orm(column_type = "Text")]
    pub body: String,
    /// A JSON list of public `https://` URLs.
    #[sea_orm(column_type = "JsonBinary")]
    pub media: Json,
    /// `draft`, `scheduled`, `queued`, `published`, `failed` or `deleted`.
    #[sea_orm(column_type = "Text")]
    pub status: String,
    pub scheduled_at: Option<DateTimeWithTimeZone>,
    pub published_at: Option<DateTimeWithTimeZone>,
    #[sea_orm(column_type = "Text", nullable)]
    pub error: Option<String>,
    pub created_by: Option<Uuid>,
    pub created_at: DateTimeWithTimeZone,
    pub updated_at: DateTimeWithTimeZone,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
