use std::cell::RefCell;
use std::os::unix::prelude::OsStrExt;
use std::path::Path;
use std::time::Instant;

use dirs::data_dir;
use rusqlite::types::{FromSql, FromSqlError};
use rusqlite::{params, Connection, ToSql};

use crate::com::{DirSettings, DisplayHidden, DisplayMode, SortDir, SortMode, SortSettings};
use crate::config::CONFIG;


#[derive(Debug)]
pub struct DBCon(RefCell<Option<Connection>>);

impl DBCon {
    // If we fail to connect, just panic and die.
    // Some operations later might be allowed to fail (DB deleted underneath us?) but not
    // creation/connection.
    pub fn connect() -> Self {
        let start = Instant::now();
        let path = CONFIG
            .database
            .clone()
            .unwrap_or_else(|| data_dir().unwrap().join("aw-fm").join("settings.db"));


        info!("Attempting to open database in {path:?}");
        if !path.exists() {
            info!("Directory {path:?} does not exist, attempting to create it.");
            let dir = path.parent().unwrap();
            assert!(!dir.exists() || dir.is_dir(), "{dir:?} exists and is not a directory");

            std::fs::create_dir_all(dir).unwrap_or_else(|e| {
                panic!("Failed to create parent directory {dir:?} for database: {e}");
            });
        } else if !path.is_file() {
            panic!("Database {path:?} exists but is not a file.");
        }

        let mut con = Connection::open(path).unwrap();

        con.pragma_update(None, "foreign_keys", "ON").unwrap();

        update_to_current(&mut con);

        trace!("Opened database in {:?}", start.elapsed());

        Self(Some(con).into())
    }

    pub fn destroy(&self) {
        debug!("Tearing down database connection");
        self.0.borrow_mut().take().unwrap().close().unwrap();
    }

    // Reading is fast enough to do blocking.
    pub fn get(&self, path: &Path) -> DirSettings {
        let start = Instant::now();

        let b = self.0.borrow();
        let con = b.as_ref().unwrap();

        let settings = con
            .query_row(
                "SELECT display_mode, sort_mode, sort_direction FROM dir_settings WHERE PATH = ?",
                [path.as_os_str().as_bytes()],
                |row| {
                    Ok(DirSettings {
                        display_mode: row.get(0)?,
                        sort: SortSettings {
                            mode: row.get(1)?,
                            direction: row.get(2)?,
                        },
                    })
                },
            )
            .unwrap_or_else(|e| {
                if e == rusqlite::Error::QueryReturnedNoRows {
                    trace!("No saved settings for {path:?}");
                } else {
                    error!("Error reading saved settings for {path:?}: {e}");
                }
                DirSettings::default()
            });

        trace!("Fetched settings for {path:?} in {:?}", start.elapsed());
        settings
    }

    // Writing is slow enough 16-17ms to want to do off the main thread
    pub fn store(&self, path: &Path, settings: DirSettings) {
        let start = Instant::now();

        let b = self.0.borrow();
        let con = b.as_ref().unwrap();

        if settings == DirSettings::default() {
            con.execute("DELETE FROM dir_settings WHERE path = ?;", [path.as_os_str().as_bytes()])
                .unwrap_or_else(|e| {
                    error!("Error clearing directory settings for {path:?} to DB: {e}");
                    0
                });
            trace!("Reset settings for path {path:?} in {:?}", start.elapsed());
            return;
        }

        con.execute(
            r#"
INSERT OR REPLACE INTO
    dir_settings(path, display_mode, sort_mode, sort_direction)
VALUES
    (?, ?, ?, ?);"#,
            params![
                path.as_os_str().as_bytes(),
                settings.display_mode,
                settings.sort.mode,
                settings.sort.direction,
            ],
        )
        .unwrap_or_else(|e| {
            error!("Error writing directory settings for {path:?} to DB: {e}");
            0
        });
        trace!("Saved settings for path {path:?} in {:?}", start.elapsed());
    }
}


// If this is needed for a lot more types, this can be done with a macro.
impl ToSql for DisplayMode {
    fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
        Ok(self.as_ref().into())
    }
}

impl FromSql for DisplayMode {
    fn column_result(value: rusqlite::types::ValueRef<'_>) -> rusqlite::types::FromSqlResult<Self> {
        value.as_str()?.parse().map_err(|e| FromSqlError::Other(Box::new(e)))
    }
}

impl ToSql for SortMode {
    fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
        Ok(self.as_ref().into())
    }
}

impl FromSql for SortMode {
    fn column_result(value: rusqlite::types::ValueRef<'_>) -> rusqlite::types::FromSqlResult<Self> {
        value.as_str()?.parse().map_err(|e| FromSqlError::Other(Box::new(e)))
    }
}

impl ToSql for SortDir {
    fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
        Ok(self.as_ref().into())
    }
}

impl FromSql for SortDir {
    fn column_result(value: rusqlite::types::ValueRef<'_>) -> rusqlite::types::FromSqlResult<Self> {
        value.as_str()?.parse().map_err(|e| FromSqlError::Other(Box::new(e)))
    }
}

impl ToSql for DisplayHidden {
    fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
        Ok(self.as_ref().into())
    }
}

impl FromSql for DisplayHidden {
    fn column_result(value: rusqlite::types::ValueRef<'_>) -> rusqlite::types::FromSqlResult<Self> {
        value.as_str()?.parse().map_err(|e| FromSqlError::Other(Box::new(e)))
    }
}


fn get_version(con: &Connection) -> u32 {
    let r = con.query_row(
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'metadata';",
        [],
        |_| Ok(()),
    );

    if r == Err(rusqlite::Error::QueryReturnedNoRows) {
        info!("Database was version 0");
        return 0;
    }
    r.unwrap();

    let v = con
        .query_row("SELECT value FROM metadata WHERE key = 'db_version';", [], |row| {
            let val: String = row.get(0).unwrap();
            Ok(val.parse().unwrap())
        })
        .unwrap();
    debug!("Database was version {v}");
    v
}

fn update_to(con: &mut Connection, version: u32, initial_version: u32, sql: &str) {
    if version <= initial_version {
        return;
    }

    info!("Updating database to version {version}");
    let tx = con.transaction().unwrap();

    tx.execute_batch(sql).unwrap();
    tx.execute(
        "INSERT OR REPLACE INTO metadata(key, value) VALUES ('db_version', ?);",
        [version],
    )
    .unwrap();

    tx.commit().unwrap();
    debug!("Updated database to version {version}");
}

fn update_to_current(con: &mut Connection) {
    let initial_version = get_version(con);

    // For now, assume all dir settings are always set.
    // This only needs to change if default globakl options become configurable, and even then we
    // can just make the default parameter the global option.
    update_to(
        con,
        1,
        initial_version,
        r#"
CREATE TABLE metadata(key TEXT, value TEXT, PRIMARY KEY(key));
CREATE TABLE dir_settings(
    path TEXT NOT NULL, -- may be invalid utf-8, but sqlite should pass it through cleanly
    display_mode TEXT NOT NULL,
    sort_mode TEXT NOT NULL,
    sort_direction TEXT NOT NULL,
    -- display_hidden TEXT NOT NULL,
    PRIMARY KEY(path)
);"#,
    );
}
