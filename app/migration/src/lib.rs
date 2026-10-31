pub use sea_orm_migration::prelude::*;

mod m20261026_000001_waitlist_signups;
mod m20261027_000001_users_sessions_magic_links;
mod m20261028_000001_passwords_passkeys;
mod m20261028_000002_teams;
mod m20261028_000003_rate_limit_hits;
mod m20261029_000001_social_accounts;
mod m20261030_000001_social_set_names;
mod m20261031_000001_posts;
mod m20261101_000001_google_sign_in;
mod m20261102_000001_inbox;
mod m20261103_000001_api_tokens;
mod m20261104_000001_idempotency_keys;
mod m20261105_000001_webhooks;

pub struct Migrator;

#[async_trait::async_trait]
impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![
            Box::new(m20261026_000001_waitlist_signups::Migration),
            Box::new(m20261027_000001_users_sessions_magic_links::Migration),
            Box::new(m20261028_000001_passwords_passkeys::Migration),
            Box::new(m20261028_000002_teams::Migration),
            Box::new(m20261028_000003_rate_limit_hits::Migration),
            Box::new(m20261029_000001_social_accounts::Migration),
            Box::new(m20261030_000001_social_set_names::Migration),
            Box::new(m20261031_000001_posts::Migration),
            Box::new(m20261101_000001_google_sign_in::Migration),
            Box::new(m20261102_000001_inbox::Migration),
            Box::new(m20261103_000001_api_tokens::Migration),
            Box::new(m20261104_000001_idempotency_keys::Migration),
            Box::new(m20261105_000001_webhooks::Migration),
            // inject-above (do not remove this comment)
        ]
    }
}
