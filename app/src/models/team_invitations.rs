use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "team_invitations")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: Uuid,
    pub team_id: Uuid,
    /// Always lowercased; only the user with this email can accept.
    #[sea_orm(column_type = "Text")]
    pub email: String,
    /// `admin` or `member`.
    #[sea_orm(column_type = "Text")]
    pub role: String,
    /// SHA-256 hex of the token in the emailed link.
    #[sea_orm(column_type = "Text", unique)]
    pub token_hash: String,
    pub invited_by: Option<Uuid>,
    pub expires_at: DateTimeWithTimeZone,
    pub created_at: DateTimeWithTimeZone,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
