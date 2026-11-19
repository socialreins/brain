use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "queue_schedules")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub social_set_id: Uuid,
    pub team_id: Uuid,
    /// An IANA zone, e.g. `Asia/Dhaka`.
    #[sea_orm(column_type = "Text")]
    pub time_zone: String,
    /// Sorted minutes of the week: `weekday * 1440 + minute_of_day`, weekday 0 is Sunday.
    pub slots: Json,
    pub updated_at: DateTimeWithTimeZone,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
