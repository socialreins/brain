use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "social_account_social_set")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub social_account_id: Uuid,
    #[sea_orm(primary_key, auto_increment = false)]
    pub social_set_id: Uuid,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
