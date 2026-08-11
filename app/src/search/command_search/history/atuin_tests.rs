use diesel::{sql_query, Connection as _, RunQueryDsl as _, SqliteConnection};
use tempfile::TempDir;

use super::*;

/// Builds a database with atuin's schema and the given rows, each
/// `(command, cwd, timestamp, exit, deleted_at)`.
fn atuin_db(rows: &[(&str, &str, i64, i64, Option<i64>)]) -> (TempDir, PathBuf) {
    let dir = TempDir::new().expect("temp dir");
    let path = dir.path().join("history.db");
    let mut connection =
        SqliteConnection::establish(path.to_str().expect("utf-8 path")).expect("creates db");

    sql_query(
        "CREATE TABLE history (
            id TEXT PRIMARY KEY,
            timestamp INTEGER NOT NULL,
            duration INTEGER NOT NULL,
            exit INTEGER NOT NULL,
            command TEXT NOT NULL,
            cwd TEXT NOT NULL,
            session TEXT NOT NULL,
            hostname TEXT NOT NULL,
            deleted_at INTEGER
        )",
    )
    .execute(&mut connection)
    .expect("creates table");

    for (index, (command, cwd, timestamp, exit, deleted_at)) in rows.iter().enumerate() {
        sql_query(format!(
            "INSERT INTO history VALUES ('id-{index}', {timestamp}, 0, {exit}, '{command}', \
             '{cwd}', 'session', 'host', {})",
            deleted_at.map_or("NULL".to_owned(), |at| at.to_string())
        ))
        .execute(&mut connection)
        .expect("inserts row");
    }

    (dir, path)
}

#[test]
fn commands_are_read_most_recent_first() {
    let (_dir, path) = atuin_db(&[
        ("git status", "/repo", 1_700_000_001, 0, None),
        ("cargo test", "/repo", 1_700_000_003, 0, None),
        ("ls", "/", 1_700_000_002, 0, None),
    ]);

    let entries = load_entries(&path).expect("loads");

    let commands: Vec<&str> = entries.iter().map(|entry| entry.command.as_str()).collect();
    assert_eq!(commands, ["cargo test", "ls", "git status"]);
}

#[test]
fn the_directory_and_exit_code_come_along() {
    let (_dir, path) = atuin_db(&[("cargo test", "/repo", 1_700_000_000, 101, None)]);

    let entry = load_entries(&path).expect("loads").remove(0);

    assert_eq!(entry.pwd.as_deref(), Some("/repo"));
    assert_eq!(entry.exit_code.map(|code| code.value()), Some(101));
    assert!(entry.start_ts.is_some());
}

#[test]
fn an_unknown_exit_status_is_not_reported_as_an_exit_code() {
    // atuin records -1 when it never saw the command finish. Passed through it
    // would render as a command that failed.
    let (_dir, path) = atuin_db(&[("vim", "/repo", 1_700_000_000, -1, None)]);

    let entry = load_entries(&path).expect("loads").remove(0);

    assert_eq!(entry.exit_code, None);
}

#[test]
fn entries_deleted_in_atuin_are_left_out() {
    let (_dir, path) = atuin_db(&[
        ("kept", "/", 1_700_000_001, 0, None),
        ("deleted", "/", 1_700_000_002, 0, Some(1_700_000_500)),
    ]);

    let entries = load_entries(&path).expect("loads");

    let commands: Vec<&str> = entries.iter().map(|entry| entry.command.as_str()).collect();
    assert_eq!(commands, ["kept"]);
}

#[test]
fn timestamps_are_understood_in_every_precision_atuin_has_used() {
    // The same instant, as seconds / millis / micros / nanos.
    let seconds = 1_700_000_000_i64;
    let (_dir, path) = atuin_db(&[
        ("as-seconds", "/", seconds, 0, None),
        ("as-millis", "/", seconds * 1_000, 0, None),
        ("as-micros", "/", seconds * 1_000_000, 0, None),
        ("as-nanos", "/", seconds * 1_000_000_000, 0, None),
    ]);

    let entries = load_entries(&path).expect("loads");

    assert_eq!(entries.len(), 4);
    for entry in entries {
        assert_eq!(
            entry.start_ts.expect("has a timestamp").timestamp(),
            seconds,
            "{} was read at the wrong precision",
            entry.command
        );
    }
}

#[test]
fn a_missing_database_is_an_error_rather_than_an_empty_history() {
    let dir = TempDir::new().expect("temp dir");

    // Silently returning no entries would be indistinguishable from a user who
    // has atuin installed but has run nothing yet.
    assert!(load_entries(&dir.path().join("history.db")).is_err());
}

#[test]
fn an_explicit_database_path_wins_over_the_data_directory() {
    let resolved = resolve_db_path(
        Some(OsString::from("/somewhere/else/history.db")),
        Some(OsString::from("/xdg/data")),
        Some(PathBuf::from("/home/user")),
    );

    assert_eq!(resolved, Some(PathBuf::from("/somewhere/else/history.db")));
}

#[test]
fn the_database_path_falls_back_to_the_xdg_data_directory() {
    let resolved = resolve_db_path(
        None,
        Some(OsString::from("/xdg/data")),
        Some(PathBuf::from("/home/user")),
    );

    assert_eq!(resolved, Some(PathBuf::from("/xdg/data/atuin/history.db")));
}

#[test]
fn the_database_path_falls_back_to_the_home_directory() {
    let resolved = resolve_db_path(None, None, Some(PathBuf::from("/home/user")));

    assert_eq!(
        resolved,
        Some(PathBuf::from("/home/user/.local/share/atuin/history.db"))
    );
}

#[test]
fn an_empty_environment_variable_is_treated_as_unset() {
    // An exported-but-empty $ATUIN_DB_PATH would otherwise resolve to "".
    let resolved = resolve_db_path(
        Some(OsString::new()),
        Some(OsString::new()),
        Some(PathBuf::from("/home/user")),
    );

    assert_eq!(
        resolved,
        Some(PathBuf::from("/home/user/.local/share/atuin/history.db"))
    );
}
