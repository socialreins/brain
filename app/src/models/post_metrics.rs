use sea_orm::entity::prelude::*;

/// The latest counts of one published target (`metrics.rs`).
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "post_metrics")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub target_id: Uuid,
    pub post_id: Uuid,
    pub team_id: Uuid,
    #[sea_orm(column_type = "Text")]
    pub platform: String,
    pub likes: Option<i64>,
    pub comments: Option<i64>,
    pub shares: Option<i64>,
    pub views: Option<i64>,
    /// The last try, failed or not.
    pub fetched_at: DateTimeWithTimeZone,
    /// Why the last try failed; `gone` is never tried again.
    #[sea_orm(column_type = "Text", nullable)]
    pub error: Option<String>,
    pub attempts: i32,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
