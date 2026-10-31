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
    /// The post holds a hit of its team's daily publish cap.
    pub quota_reserved: bool,
    pub created_by: Option<Uuid>,
    /// Shared by the posts of one `POST /v1/post-groups` call; `None` for a single post.
    pub group_id: Option<Uuid>,
    /// Follow-up parts of a thread, `[{body, media}]`, each a reply to the one before.
    #[sea_orm(column_type = "JsonBinary")]
    pub thread: Json,
    pub created_at: DateTimeWithTimeZone,
    pub updated_at: DateTimeWithTimeZone,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
