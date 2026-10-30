use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "inbox_items")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: Uuid,
    pub team_id: Uuid,
    pub social_account_id: Uuid,
    #[sea_orm(column_type = "Text")]
    pub platform: String,
    /// `comment`, `mention` or `dm`.
    #[sea_orm(column_type = "Text")]
    pub kind: String,
    /// The platform's id: a Bluesky post `at://` URI, or a chat message id.
    #[sea_orm(column_type = "Text")]
    pub remote_id: String,
    #[sea_orm(column_type = "Text", nullable)]
    pub remote_cid: Option<String>,
    /// The post this one answers.
    #[sea_orm(column_type = "Text", nullable)]
    pub parent_remote_id: Option<String>,
    /// The thread's first post (the item itself when it starts one), for a reply's `root`.
    #[sea_orm(column_type = "Text", nullable)]
    pub root_remote_id: Option<String>,
    #[sea_orm(column_type = "Text", nullable)]
    pub root_remote_cid: Option<String>,
    /// The DM conversation.
    #[sea_orm(column_type = "Text", nullable)]
    pub convo_id: Option<String>,
    /// Our post the thread hangs off, when it was published through SocialReins.
    pub post_id: Option<Uuid>,
    #[sea_orm(column_type = "Text")]
    pub author_id: String,
    #[sea_orm(column_type = "Text", nullable)]
    pub author_handle: Option<String>,
    #[sea_orm(column_type = "Text", nullable)]
    pub author_name: Option<String>,
    #[sea_orm(column_type = "Text", nullable)]
    pub author_avatar: Option<String>,
    #[sea_orm(column_type = "Text")]
    pub text: String,
    pub received_at: DateTimeWithTimeZone,
    /// `open` or `done`.
    #[sea_orm(column_type = "Text")]
    pub state: String,
    pub replied_at: Option<DateTimeWithTimeZone>,
    pub created_at: DateTimeWithTimeZone,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
