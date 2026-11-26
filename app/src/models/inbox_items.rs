use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "inbox_items")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: Uuid,
    /// The number web addresses use: one counter over every team, set by the column's identity, so an
    /// insert leaves it `NotSet`.
    pub number: i64,
    pub team_id: Uuid,
    pub social_account_id: Uuid,
    #[sea_orm(column_type = "Text")]
    pub platform: String,
    /// `comment`, `mention`, `dm` or `follow` (a Bluesky follow, stored `done`).
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
    /// Where the item lives, when the platform gave a link (Meta); Bluesky's is built from
    /// `remote_id`.
    #[sea_orm(column_type = "Text", nullable)]
    pub permalink: Option<String>,
    /// A `dm` that replies to one of the account's stories (Instagram).
    pub story_reply: bool,
    #[sea_orm(column_type = "Text", nullable)]
    pub story_id: Option<String>,
    #[sea_orm(column_type = "Text", nullable)]
    pub story_url: Option<String>,
    /// A `dm` that is a click on a quick reply button (Meta): the button's payload.
    #[sea_orm(column_type = "Text", nullable)]
    pub button_payload: Option<String>,
    /// The author's contact (`contacts`), set when the item is stored.
    pub contact_id: Option<Uuid>,
    /// The member the item is assigned to.
    pub assigned_to: Option<Uuid>,
    /// When automations last ran over the item to the end; `None` until then (the sweep retries).
    pub automated_at: Option<DateTimeWithTimeZone>,
    /// What the item carries besides text: `inbox::Attachment`s, https links only.
    #[sea_orm(column_type = "JsonBinary")]
    pub attachments: Json,
    /// What was done to a Meta comment from the inbox: `hidden`, `liked`, `deleted`.
    #[sea_orm(column_type = "JsonBinary")]
    pub moderation: Json,
    pub created_at: DateTimeWithTimeZone,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
