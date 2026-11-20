use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "subscribers")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: Uuid,
    pub team_id: Uuid,
    /// Always lowercased; one row per team and email.
    #[sea_orm(column_type = "Text")]
    pub email: String,
    /// The public page the first sign-up came from.
    #[sea_orm(column_type = "Text", nullable)]
    pub page_code: Option<String>,
    /// SHA-256 hex of the token in the latest confirmation email.
    #[sea_orm(column_type = "Text", nullable, unique)]
    pub verify_token_hash: Option<String>,
    /// When the latest confirmation email was sent, for the resend throttle.
    pub verify_sent_at: Option<DateTimeWithTimeZone>,
    /// When the emailed link was first confirmed; `None` until then.
    pub verified_at: Option<DateTimeWithTimeZone>,
    /// When the person unsubscribed; never mailed while set. Confirming a new sign-up clears it.
    /// (The token of their unsubscribe links lives in `unsubscribe_token*`, read by raw SQL.)
    pub unsubscribed_at: Option<DateTimeWithTimeZone>,
    pub created_at: DateTimeWithTimeZone,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
