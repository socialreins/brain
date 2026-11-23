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
pub mod m20261123_000001_all_sets_name;
pub mod m20261124_000001_team_code;
mod m20261125_000001_post_pages;
mod m20261126_000001_newsletter;
mod m20261127_000001_automation_triggers;
mod m20261128_000001_posting_queue;
mod m20261129_000001_media_library;
mod m20261130_000001_post_approvals;
mod m20261201_000001_post_repeat;
mod m20261202_000001_feeds;
mod m20261203_000001_automation_flows;
mod m20261204_000001_team_slug;
pub mod m20261205_000001_contacts;
pub mod m20261206_000001_post_metrics;
pub mod m20261207_000001_inbox_conversations;
pub mod m20261208_000001_broadcasts;
mod m20261209_000001_newsletters;
mod m20261210_000001_newsletter_sender_logo;
mod m20261211_000001_billing;
mod m20261212_000001_team_ai_keys;
mod m20261213_000001_stripe_events;
mod m20261214_000001_comp_plans;

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
            Box::new(m20261123_000001_all_sets_name::Migration),
            Box::new(m20261124_000001_team_code::Migration),
            Box::new(m20261125_000001_post_pages::Migration),
            Box::new(m20261126_000001_newsletter::Migration),
            Box::new(m20261127_000001_automation_triggers::Migration),
            Box::new(m20261128_000001_posting_queue::Migration),
            Box::new(m20261129_000001_media_library::Migration),
            Box::new(m20261130_000001_post_approvals::Migration),
            Box::new(m20261201_000001_post_repeat::Migration),
            Box::new(m20261202_000001_feeds::Migration),
            Box::new(m20261203_000001_automation_flows::Migration),
            Box::new(m20261204_000001_team_slug::Migration),
            Box::new(m20261205_000001_contacts::Migration),
            Box::new(m20261206_000001_post_metrics::Migration),
            Box::new(m20261207_000001_inbox_conversations::Migration),
            Box::new(m20261208_000001_broadcasts::Migration),
            Box::new(m20261209_000001_newsletters::Migration),
            Box::new(m20261210_000001_newsletter_sender_logo::Migration),
            Box::new(m20261211_000001_billing::Migration),
            Box::new(m20261212_000001_team_ai_keys::Migration),
            Box::new(m20261213_000001_stripe_events::Migration),
            Box::new(m20261214_000001_comp_plans::Migration),
            // inject-above (do not remove this comment)
        ]
    }
}
