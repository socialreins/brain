//! Automations on Facebook and Instagram against a mock Graph API (no real Meta call): the
//! engine's safety rules and, in the other modules, the rules Meta sets. Needs `DATABASE_URL`
//! like the request tests.
mod log;
mod meta;
mod reply;
mod rules;
mod support;
