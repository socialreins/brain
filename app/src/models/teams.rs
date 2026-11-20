use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "teams")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: Uuid,
    #[sea_orm(column_type = "Text")]
    pub name: String,
    /// The team's address in the web app (`/t/<code>/...`): 6 lowercase characters, permanent.
    #[sea_orm(column_type = "Text", unique)]
    pub code: String,
    /// An address the team chose, usable wherever the code is. Lowercase, unique, may change.
    #[sea_orm(column_type = "Text", nullable)]
    pub slug: Option<String>,
    /// Posts of this team may have a public page (`post_pages`).
    pub public_pages: bool,
    /// Public pages offer a newsletter sign-up (`subscribers`).
    pub newsletter: bool,
    /// Who the team's newsletters are from; set together with the address or not at all.
    #[sea_orm(column_type = "Text", nullable)]
    pub newsletter_sender_name: Option<String>,
    /// The sender's postal address, which the law wants in every newsletter.
    #[sea_orm(column_type = "Text", nullable)]
    pub newsletter_sender_address: Option<String>,
    /// A member's scheduled posts wait for an owner or admin to approve them (`posts.approval`).
    pub require_approval: bool,
    pub created_at: DateTimeWithTimeZone,
    pub updated_at: DateTimeWithTimeZone,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
