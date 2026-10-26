pub use sea_orm_migration::prelude::*;

mod m20261026_000001_waitlist_signups;

pub struct Migrator;

#[async_trait::async_trait]
impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![
            Box::new(m20261026_000001_waitlist_signups::Migration),
            // inject-above (do not remove this comment)
        ]
    }
}
