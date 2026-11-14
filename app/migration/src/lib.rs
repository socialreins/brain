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
mod m20261106_000001_post_lists;
mod m20261107_000001_post_groups;
mod m20261108_000001_thread_posts;
mod m20261109_000001_waitlist_verify;
mod m20261110_000001_post_media;
mod m20261111_000001_threads_connect;
mod m20261112_000001_meta_picks;
mod m20261113_000001_inbox_permalink;
mod m20261114_000001_first_comment;
mod m20261115_000001_video_cover;
mod m20261116_000001_tiktok_settings;
mod m20261117_000001_post_fanned_out;
mod m20261118_000001_magic_link_name;
mod m20261119_000001_google_login_only;
mod m20261120_000001_youtube_settings;
mod m20261121_000001_read_only_tokens;
mod m20261122_000001_admin_console;
pub mod m20261123_000001_team_code;

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
            Box::new(m20261106_000001_post_lists::Migration),
            Box::new(m20261107_000001_post_groups::Migration),
            Box::new(m20261108_000001_thread_posts::Migration),
            Box::new(m20261109_000001_waitlist_verify::Migration),
            Box::new(m20261110_000001_post_media::Migration),
            Box::new(m20261111_000001_threads_connect::Migration),
            Box::new(m20261112_000001_meta_picks::Migration),
            Box::new(m20261113_000001_inbox_permalink::Migration),
            Box::new(m20261114_000001_first_comment::Migration),
            Box::new(m20261115_000001_video_cover::Migration),
            Box::new(m20261116_000001_tiktok_settings::Migration),
            Box::new(m20261117_000001_post_fanned_out::Migration),
            Box::new(m20261118_000001_magic_link_name::Migration),
            Box::new(m20261119_000001_google_login_only::Migration),
            Box::new(m20261120_000001_youtube_settings::Migration),
            Box::new(m20261121_000001_read_only_tokens::Migration),
            Box::new(m20261122_000001_admin_console::Migration),
            Box::new(m20261123_000001_team_code::Migration),
            // inject-above (do not remove this comment)
        ]
    }
}
