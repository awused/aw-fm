use std::cell::RefCell;
use std::mem::ManuallyDrop;
use std::time::Instant;

use dirs::data_dir;
use rusqlite::types::{FromSql, FromSqlError};
use rusqlite::{params, Connection, OpenFlags, ToSql};

use crate::com::DisplayMode;
use crate::config::CONFIG;


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


#[derive(Debug)]
pub struct DBCon(RefCell<Option<Connection>>);

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
    let v = get_version(con);

    update_to(
        con,
        1,
        0,
        r#"
CREATE TABLE metadata(key TEXT, value TEXT, PRIMARY KEY(key));
CREATE TABLE dir_settings(
    path TEXT, -- may be invalid utf-8, but sqlite should pass it through cleanly
    display TEXT,
    sort_mode TEXT,
    sort_direction TEXT,
    -- display_hidden TEXT,
    PRIMARY KEY(path)
);
            "#,
    );
}

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

        // For testing, destroy DB each time
        if path.is_file() {
            error!("Removing existing DB for testing purposes");
            std::fs::remove_file(&path).unwrap();
        }

        info!("Attempting to open database in {path:?}");
        if !path.exists() {
            info!("Directory {path:?} does not exist, attempting to create it.");
            let dir = path.parent().unwrap();
            assert!(!dir.exists() || dir.is_dir(), "{dir:?} exists and is not a directory");

            std::fs::create_dir_all(dir).unwrap_or_else(|e| {
                &format!("Failed to create parent directory {dir:?} for database: {e}");
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
}
