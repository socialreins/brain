use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "waitlist_signups")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: Uuid,
    /// Always lowercased.
    #[sea_orm(column_type = "Text", unique)]
    pub email: String,
    #[sea_orm(column_type = "Text", nullable)]
    pub source: Option<String>,
    pub created_at: DateTimeWithTimeZone,
    /// When the emailed link was first opened; `None` until then.
    pub verified_at: Option<DateTimeWithTimeZone>,
    /// SHA-256 hex of the token in the latest confirmation email.
    #[sea_orm(column_type = "Text", nullable, unique)]
    pub verify_token_hash: Option<String>,
    /// When the latest confirmation email was sent, for the resend throttle.
    pub verify_sent_at: Option<DateTimeWithTimeZone>,
    /// When an admin approved the email: it may sign up whatever the sign-up rules say.
    pub approved_at: Option<DateTimeWithTimeZone>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
