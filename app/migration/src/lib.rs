pub use sea_orm_migration::prelude::*;

mod m20261026_000001_waitlist_signups;
mod m20261027_000001_users_sessions_magic_links;

pub struct Migrator;

#[async_trait::async_trait]
impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![
            Box::new(m20261026_000001_waitlist_signups::Migration),
            Box::new(m20261027_000001_users_sessions_magic_links::Migration),
            // inject-above (do not remove this comment)
        ]
    }
}
