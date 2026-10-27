use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "magic_links")]
pub struct Model {
    /// SHA-256 hex of the token in the emailed link.
    #[sea_orm(primary_key, auto_increment = false, column_type = "Text")]
    pub token_hash: String,
    /// Always lowercased. The account is created on redeem if it does not exist yet.
    #[sea_orm(column_type = "Text")]
    pub email: String,
    pub expires_at: DateTimeWithTimeZone,
    pub created_at: DateTimeWithTimeZone,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
