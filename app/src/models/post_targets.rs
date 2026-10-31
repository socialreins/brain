use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "post_targets")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: Uuid,
    pub post_id: Uuid,
    /// `null` once the account was disconnected.
    pub social_account_id: Option<Uuid>,
    /// `pending`, `publishing`, `published`, `failed` or `deleted`.
    #[sea_orm(column_type = "Text")]
    pub status: String,
    /// The platform's id for the published post, e.g. a Bluesky `at://` URI.
    #[sea_orm(column_type = "Text", nullable)]
    pub remote_id: Option<String>,
    #[sea_orm(column_type = "Text", nullable)]
    pub remote_url: Option<String>,
    #[sea_orm(column_type = "Text", nullable)]
    pub error: Option<String>,
    pub published_at: Option<DateTimeWithTimeZone>,
    /// Bluesky's content hash of the root post, which a reply names with its URI.
    #[sea_orm(column_type = "Text", nullable)]
    pub remote_cid: Option<String>,
    /// The thread parts published so far, in order: `[{remote_id, cid, remote_url,
    /// published_at}]`.
    #[sea_orm(column_type = "JsonBinary")]
    pub parts: Json,
    pub created_at: DateTimeWithTimeZone,
    pub updated_at: DateTimeWithTimeZone,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
