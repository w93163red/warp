//! Reads [atuin](https://atuin.sh)'s history database so its commands can be
//! searched from Warp's own command search.
//!
//! atuin normally replaces the shell's ctrl-r with a UI of its own, which does
//! not work inside Warp: it is drawn from a ZLE widget, inline below the prompt,
//! and Warp's block model has no notion of that space. Reading its database
//! instead puts the same history behind the search Warp already has, and leaves
//! ctrl-r bound to Warp.
//!
//! The database is only ever read. atuin owns it, including syncing it between
//! machines; entries recorded during this Warp session show up the next time the
//! database is read.

use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context as _, Result};
use chrono::{DateTime, Local, TimeZone as _};
use diesel::{sql_query, sql_types, Connection as _, QueryableByName, RunQueryDsl as _,
    SqliteConnection};

use crate::terminal::HistoryEntry;

/// Environment variable atuin uses to override the database location.
const DB_PATH_ENV: &str = "ATUIN_DB_PATH";

/// Location of the database under the XDG data directory.
const DB_RELATIVE_PATH: &str = "atuin/history.db";

/// How many of the most recent entries to read.
///
/// A synced atuin database can hold years of history across several machines,
/// and every entry read here is held in memory and walked on each keystroke.
/// The cap keeps both bounded; entries past it are old enough that the shell's
/// own history is a better match for them anyway.
const MAX_ENTRIES: i64 = 20_000;

/// A row of atuin's `history` table.
///
/// atuin has added columns over time but has not removed or repurposed any of
/// these, so naming them explicitly rather than selecting `*` keeps this working
/// across versions.
#[derive(QueryableByName)]
struct HistoryRow {
    #[diesel(sql_type = sql_types::Text)]
    command: String,
    #[diesel(sql_type = sql_types::Text)]
    cwd: String,
    /// Time the command was run. atuin has stored this with different
    /// precisions over the years; see [`timestamp_to_local`].
    #[diesel(sql_type = sql_types::BigInt)]
    timestamp: i64,
    #[diesel(sql_type = sql_types::BigInt)]
    exit: i64,
}

/// Path of atuin's database, or `None` if it is not where atuin would put it.
///
/// `$ATUIN_DB_PATH` wins, then `$XDG_DATA_HOME`, then the default data
/// directory. A `db_path` set in atuin's own config file is deliberately not
/// parsed: it would mean reading and interpreting their TOML, and the two
/// environment variables cover the cases where the path is not the default.
pub fn db_path() -> Option<PathBuf> {
    resolve_db_path(
        std::env::var_os(DB_PATH_ENV),
        std::env::var_os("XDG_DATA_HOME"),
        dirs::home_dir(),
    )
}

/// The path resolution behind [`db_path`], taking the environment as arguments
/// so it can be exercised without mutating this process's.
fn resolve_db_path(
    db_path_env: Option<OsString>,
    xdg_data_home: Option<OsString>,
    home_dir: Option<PathBuf>,
) -> Option<PathBuf> {
    if let Some(path) = db_path_env.filter(|path| !path.is_empty()) {
        return Some(PathBuf::from(path));
    }

    let data_dir = match xdg_data_home.filter(|dir| !dir.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => home_dir?.join(".local").join("share"),
    };

    Some(data_dir.join(DB_RELATIVE_PATH))
}

/// Reads the most recent [`MAX_ENTRIES`] commands from the database at `path`.
///
/// Entries atuin has marked deleted are skipped, so a command deleted there does
/// not linger in Warp's search.
pub fn load_entries(path: &Path) -> Result<Vec<HistoryEntry>> {
    // Opening read-only keeps us from creating a database where atuin has none,
    // and from taking a write lock on one atuin is using.
    let url = format!("file:{}?mode=ro", path.display());
    let mut connection = SqliteConnection::establish(&url)
        .with_context(|| format!("failed to open atuin's history database at {}", path.display()))?;

    let rows: Vec<HistoryRow> = sql_query(
        "SELECT command, cwd, timestamp, exit FROM history \
         WHERE deleted_at IS NULL \
         ORDER BY timestamp DESC \
         LIMIT ?",
    )
    .bind::<sql_types::BigInt, _>(MAX_ENTRIES)
    .load(&mut connection)
    .context("failed to read atuin's history table")?;

    Ok(rows.into_iter().map(HistoryRow::into_entry).collect())
}

impl HistoryRow {
    fn into_entry(self) -> HistoryEntry {
        let mut entry = HistoryEntry::command_only(self.command);
        entry.pwd = (!self.cwd.is_empty()).then_some(self.cwd);
        entry.start_ts = timestamp_to_local(self.timestamp);
        // atuin records -1 for a command whose exit status it never saw, which
        // is not an exit code and would render as a failed command.
        entry.exit_code = (self.exit >= 0).then(|| (self.exit as i32).into());
        entry
    }
}

/// Converts one of atuin's timestamps to a local datetime.
///
/// atuin has stored these as seconds, and later as nanoseconds since the epoch,
/// and a synced database can hold rows written by both. Rather than depend on a
/// schema version, pick the unit that lands the value in a plausible range: the
/// unit that is wrong by a factor of a billion puts the command either in 1970
/// or tens of thousands of years from now.
fn timestamp_to_local(timestamp: i64) -> Option<DateTime<Local>> {
    if timestamp <= 0 {
        return None;
    }

    /// Seconds at the start of the year 3000, above which a value has to be a
    /// finer unit than seconds to be a real timestamp.
    const MAX_PLAUSIBLE_SECONDS: i64 = 32_503_680_000;

    let mut seconds = timestamp;
    let mut subsec_nanos = 0;
    for divisor in [1, 1_000, 1_000_000, 1_000_000_000] {
        if timestamp / divisor <= MAX_PLAUSIBLE_SECONDS {
            seconds = timestamp / divisor;
            subsec_nanos = ((timestamp % divisor) * (1_000_000_000 / divisor)) as u32;
            break;
        }
    }

    Local.timestamp_opt(seconds, subsec_nanos).single()
}

/// Loads atuin's history, or `None` if atuin is not set up on this machine.
///
/// A database that exists but cannot be read is reported and then treated as
/// absent: Warp's own history is still worth searching.
pub(crate) fn load_shared_entries() -> Option<Arc<[Arc<HistoryEntry>]>> {
    let path = db_path()?;
    if !path.is_file() {
        return None;
    }

    match load_entries(&path) {
        Ok(entries) => {
            log::info!(
                "Loaded {} commands from atuin's history at {}",
                entries.len(),
                path.display()
            );
            Some(entries.into_iter().map(Arc::new).collect())
        }
        Err(err) => {
            log::warn!("Could not read atuin's history: {err:#}");
            None
        }
    }
}

#[cfg(test)]
#[path = "atuin_tests.rs"]
mod tests;
