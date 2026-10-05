use std::path::Path;
use std::str::FromStr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};

static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

/// An open, migrated database. Cheap to clone; clones share one pool.
#[derive(Clone)]
pub struct Database {
    pool: SqlitePool,
}

#[derive(Debug, thiserror::Error)]
pub enum DatabaseError {
    #[error("can't create the database file")]
    Create(#[source] std::io::Error),
    #[error("can't open the database")]
    Open(#[source] sqlx::Error),
    /// Includes a migration already applied whose file has since changed.
    #[error("can't apply the database migrations")]
    Migrate(#[source] sqlx::migrate::MigrateError),
}

impl Database {
    /// Opens the database at `path`, creating it if missing (readable by its
    /// owner only), and applies any migrations it hasn't had.
    pub async fn open(path: &Path) -> Result<Self, DatabaseError> {
        create_private(path).map_err(DatabaseError::Create)?;
        let options = SqliteConnectOptions::from_str("sqlite:")
            .map_err(DatabaseError::Open)?
            .filename(path)
            // Readers don't block the writer, and a committed transaction is on
            // disk before it returns: an enqueued job survives a power cut.
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Full)
            .foreign_keys(true)
            // SQLite has one writer at a time; others wait instead of failing.
            .busy_timeout(Duration::from_secs(5));
        let pool = SqlitePoolOptions::new()
            .max_connections(8)
            .connect_with(options)
            .await
            .map_err(DatabaseError::Open)?;
        MIGRATOR.run(&pool).await.map_err(DatabaseError::Migrate)?;
        Ok(Self { pool })
    }

    pub(crate) fn pool(&self) -> &SqlitePool {
        &self.pool
    }
}

/// Creates the file with mode 0600 if it doesn't exist. SQLite gives its WAL
/// and shared-memory files the same permissions as the database's.
fn create_private(path: &Path) -> std::io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    match options.open(path) {
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error),
    }
}

/// An instant as stored: milliseconds since the Unix epoch, UTC.
pub(crate) fn millis(instant: SystemTime) -> i64 {
    instant
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| i64::try_from(since.as_millis()).unwrap_or(i64::MAX))
}

/// A duration as stored, saturating.
pub(crate) fn duration_millis(duration: Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}
