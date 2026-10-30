use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "social_accounts")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: Uuid,
    pub team_id: Uuid,
    /// `bluesky`, later `facebook`, `instagram`, `threads`, `linkedin`, `x`, ...
    #[sea_orm(column_type = "Text")]
    pub provider: String,
    /// The platform's id for the account, e.g. a Bluesky DID.
    #[sea_orm(column_type = "Text")]
    pub external_id: String,
    #[sea_orm(column_type = "Text", nullable)]
    pub handle: Option<String>,
    #[sea_orm(column_type = "Text", nullable)]
    pub display_name: Option<String>,
    #[sea_orm(column_type = "Text", nullable)]
    pub avatar_url: Option<String>,
    /// Sealed by `socialreins_core::crypto::Cipher`. Never log it or send it to a client.
    pub access_token: Vec<u8>,
    pub token_expires_at: Option<DateTimeWithTimeZone>,
    #[sea_orm(column_type = "JsonBinary")]
    pub meta: Json,
    /// The app password may read and send DMs (Bluesky: "Allow access to your direct messages").
    pub dm_access: bool,
    /// Ingest cursors: the newest notification and DM already read.
    pub notifications_seen_at: Option<DateTimeWithTimeZone>,
    pub messages_seen_at: Option<DateTimeWithTimeZone>,
    /// The platform refused the stored password at ingest; ingest pauses for an hour.
    pub inbox_error_at: Option<DateTimeWithTimeZone>,
    pub created_by: Option<Uuid>,
    pub created_at: DateTimeWithTimeZone,
    pub updated_at: DateTimeWithTimeZone,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
