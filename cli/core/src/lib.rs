//! `dira-core` — the capture engine shared by the daemon and CLI.
//!
//! - [`model`]: the normalized event model (the source of truth).
//! - [`accounting`]: de-duplicated, idle-trimmed human-time math (property-tested).
//! - [`report`]: on-demand local reports computed from the event log.
//! - [`store`]: the append-only SQLite store.
//! - [`project`]: working-dir → canonical repo + identity resolution.
//! - [`config`]: layered configuration.

pub mod accounting;
pub mod config;
pub mod identity;
pub mod model;
pub mod pricing;
pub mod project;
pub mod protocol;
pub mod report;
pub mod signing;
pub mod store;
pub mod sync;
pub mod tokens;
pub mod zavet;

pub use config::Config;
pub use model::{EventKind, RawEvent};
pub use store::Store;

/// Errors surfaced by the core engine.
///
/// The `sqlx`/`migrate` variants are boxed: those error types are large, and
/// keeping them inline would bloat every `Result<_, Error>` in the crate
/// (clippy::result_large_err).
#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("database error: {0}")]
    Sqlx(Box<sqlx::Error>),
    #[error("migration error: {0}")]
    Migrate(Box<sqlx::migrate::MigrateError>),
    #[error(
        "{db} was last written by a newer dira/dirad (it records schema migration {version}, \
         which this binary does not know). Refusing to run against a newer schema. \
         Update this binary (`dira update`), or restore the version that wrote the database \
         (`dira update --version <that version>`)."
    )]
    SchemaNewer { version: i64, db: String },
    #[error("time formatting error: {0}")]
    Time(String),
    #[error("time parse error: {0}")]
    Parse(String),
    #[error("decode error: {0}")]
    Decode(String),
    #[error("config error: {0}")]
    Config(String),
    #[error("crypto error: {0}")]
    Crypto(String),
}

impl From<sqlx::Error> for Error {
    fn from(e: sqlx::Error) -> Self {
        Error::Sqlx(Box::new(e))
    }
}

impl From<sqlx::migrate::MigrateError> for Error {
    fn from(e: sqlx::migrate::MigrateError) -> Self {
        Error::Migrate(Box::new(e))
    }
}

impl Error {
    pub(crate) fn time(e: time::error::Format) -> Self {
        Error::Time(e.to_string())
    }
    pub(crate) fn parse(e: time::error::Parse) -> Self {
        Error::Parse(e.to_string())
    }
}
