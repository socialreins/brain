use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "automation_runs")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: Uuid,
    pub automation_id: Uuid,
    pub inbox_item_id: Uuid,
    pub team_id: Uuid,
    /// `running`, `waiting` (a flow at a `wait` step), `sent`, `failed` or `skipped`.
    #[sea_orm(column_type = "Text")]
    pub status: String,
    #[sea_orm(column_type = "Text", nullable)]
    pub reason: Option<String>,
    /// One `{type, status, reason, remote_id}` per action tried.
    #[sea_orm(column_type = "JsonBinary")]
    pub results: Json,
    /// A flow's step last tried, from 0; null for an automation with `actions`.
    pub step_index: Option<i32>,
    /// When a `waiting` run goes on. Still set on a `running` one that a round took up and
    /// has not started yet.
    pub resume_at: Option<DateTimeWithTimeZone>,
    /// Parked at a `wait_for_reply` step: goes on when the person's next DM comes
    /// (`reply_item_id`), else ends `skipped: no_reply` at `resume_at`.
    pub awaiting_reply: bool,
    /// The DM that answered the `wait_for_reply`; later DM steps go out in its conversation.
    pub reply_item_id: Option<Uuid>,
    /// Tries again after a rate limit refused a send (by itself, at most `MAX_RETRIES`, or by a
    /// person).
    pub retries: i32,
    /// The step a retry sends again already took its hit of the team's daily cap.
    pub retry_paid: bool,
    /// A flow's run: the md5 of the steps (as jsonb text) it started on.
    #[sea_orm(column_type = "Text", nullable)]
    pub steps_hash: Option<String>,
    pub created_at: DateTimeWithTimeZone,
    pub updated_at: DateTimeWithTimeZone,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
