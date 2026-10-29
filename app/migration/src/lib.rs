pub use sea_orm_migration::prelude::*;

mod m20261026_000001_waitlist_signups;
mod m20261027_000001_users_sessions_magic_links;
mod m20261028_000001_passwords_passkeys;
mod m20261028_000002_teams;
mod m20261028_000003_rate_limit_hits;
mod m20261029_000001_social_accounts;

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
            // inject-above (do not remove this comment)
        ]
    }
}
