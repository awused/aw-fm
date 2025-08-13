use std::cell::Cell;
use std::ffi::OsStr;
use std::os::unix::prelude::OsStrExt;
use std::path::Path;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, SyncSender};
use std::thread::JoinHandle;
use std::time::Instant;

use dirs::data_dir;
use rusqlite::types::{FromSql, FromSqlError};
use rusqlite::{Connection, ToSql, params};
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;

use crate::com::{DebugIgnore, DirSettings, DisplayMode, SortDir, SortMode, SortSettings};
use crate::config::CONFIG;
use crate::{closing, spawn_thread};


#[derive(Debug, Serialize, Deserialize)]
pub enum SplitChild {
    Split(Box<SavedSplit>),
    Tab(u32),
}

impl SplitChild {
    pub const fn first_child(mut self: &Self) -> u32 {
        loop {
            match self {
                Self::Split(s) => self = &s.start,
                Self::Tab(n) => return *n,
            }
        }
    }
}


#[derive(Debug, Serialize, Deserialize)]
pub struct SavedSplit {
    pub horizontal: bool,
    pub start: SplitChild,
    pub end: SplitChild,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SavedGroup {
    pub parent: u32,
    pub split: SavedSplit,
}

#[derive(Debug)]
pub struct Session {
    pub paths: Vec<Arc<Path>>,
    pub groups: Vec<SavedGroup>,
}

enum DBAction {
    Get(Arc<Path>, oneshot::Sender<DirSettings>),
    Store(Arc<Path>, DirSettings),
    LoadSession(String, oneshot::Sender<Option<Session>>),
    SaveSession(String, Session),
    DeleteSession(String),
    Teardown,
}

#[derive(Debug)]
pub struct DBCon(SyncSender<DBAction>, DebugIgnore<Cell<Option<JoinHandle<()>>>>);

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

        let (sender, receiver) = std::sync::mpsc::sync_channel(2);
        let h = spawn_thread("database", move || Con(con).run(receiver));

        trace!("Opened database in {:?}", start.elapsed());

        Self(sender, DebugIgnore::from(Cell::new(Some(h))))
    }

    // Reading is fast enough to block on
    pub fn get(&self, path: Arc<Path>) -> DirSettings {
        let start = Instant::now();

        let (send, recv) = oneshot::channel();

        self.0.send(DBAction::Get(path, send)).unwrap();

        // This should swallow all DB errors so should not fail
        let settings = recv.blocking_recv().unwrap();

        trace!("Fetched settings in {:?}", start.elapsed());
        settings
    }

    // Writing is slow enough 16-17ms to want to do off the main thread
    pub fn store(&self, path: Arc<Path>, settings: DirSettings) {
        self.0.send(DBAction::Store(path, settings)).unwrap();
    }

    pub fn load_session(&self, name: String) -> Option<Session> {
        let start = Instant::now();

        let (send, recv) = oneshot::channel();

        self.0.send(DBAction::LoadSession(name, send)).unwrap();

        // This should swallow all DB errors so should not fail
        let session = recv.blocking_recv().unwrap();

        trace!("Loaded session in {:?}: found {}", start.elapsed(), session.is_some());
        session
    }

    pub fn save_session(&self, name: String, session: Session) {
        self.0.send(DBAction::SaveSession(name, session)).unwrap();
    }

    pub fn delete_session(&self, name: String) {
        self.0.send(DBAction::DeleteSession(name)).unwrap();
    }

    pub fn destroy(&self) {
        debug!("Tearing down database connection");
        self.0.send(DBAction::Teardown).unwrap();
        self.1.take().unwrap().join().unwrap();
    }
}


struct Con(Connection);

impl Con {
    fn run(&self, receiver: Receiver<DBAction>) {
        while let Ok(a) = receiver.recv() {
            match a {
                DBAction::Get(path, resp) => drop(resp.send(self.get(&path))),
                DBAction::Store(path, settings) => self.store(&path, settings),
                DBAction::SaveSession(name, session) => self.save_session(&name, session),
                DBAction::LoadSession(name, resp) => drop(resp.send(self.load_session(&name))),
                DBAction::DeleteSession(name) => self.delete_session(&name),
                DBAction::Teardown => {
                    return;
                }
            }
        }

        if !closing::closed() {
            error!("Gui->Database connection unexpectedly closed.")
        }
    }

    fn get(&self, path: &Path) -> DirSettings {
        let con = &self.0;

        con.query_row(
            "SELECT display_mode, sort_mode, sort_direction FROM dir_settings WHERE path = ?",
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
        })
    }

    fn store(&self, path: &Path, settings: DirSettings) {
        let start = Instant::now();
        let con = &self.0;

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

    fn load_session(&self, name: &str) -> Option<Session> {
        let con = &self.0;

        con.query_row("SELECT paths, groups FROM sessions WHERE name = ?", [name], |row| {
            let raw: &[u8] = row.get_ref(0)?.as_bytes()?;

            let paths = raw
                .split(|b| *b == 0)
                .map(OsStr::from_bytes)
                .map(Path::new)
                .map(Into::into)
                .collect();

            let groups = if let Some(raw) = row.get_ref(1)?.as_blob_or_null()? {
                match rmp_serde::from_slice(raw) {
                    Ok(gs) => gs,
                    Err(e) => {
                        error!("Error deserializing saved groups: {e}");
                        Vec::new()
                    }
                }
            } else {
                Vec::new()
            };

            Ok(Session { paths, groups })
        })
        .map_err(|e| {
            if e == rusqlite::Error::QueryReturnedNoRows {
                trace!("No saved session named {name}");
            } else {
                error!("Error reading saved session {name}: {e}");
            }
        })
        .ok()
    }

    fn save_session(&self, name: &str, session: Session) {
        let start = Instant::now();
        let con = &self.0;

        let paths = session
            .paths
            .iter()
            .map(|p| p.as_os_str())
            .map(OsStr::as_bytes)
            .collect::<Vec<_>>()
            .join(&[0u8] as &[u8]);

        // to_vec_named might be slightly more resilient to changes, but neither will really be
        // forward compatible
        // Should never fail.
        let groups = rmp_serde::to_vec(&session.groups).unwrap();

        con.execute(
            "INSERT OR REPLACE INTO sessions(name, paths, groups) VALUES (?, ?, ?);",
            params![name, paths, groups],
        )
        .unwrap_or_else(|e| {
            if e == rusqlite::Error::QueryReturnedNoRows {
                trace!("No saved session named {name}");
            } else {
                error!("Error reading saved session {name}: {e}");
            }
            0
        });

        trace!("Saved session {name} in {:?}", start.elapsed());
    }

    fn delete_session(&self, name: &str) {
        let start = Instant::now();
        let con = &self.0;

        drop(con.execute("DELETE FROM sessions WHERE name = ?", [name]));

        trace!("Deleted session {name} in {:?}", start.elapsed());
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
    update_to(
        con,
        2,
        initial_version,
        r#"
CREATE TABLE sessions(
    name TEXT NOT NULL,
    paths BLOB NOT NULL, -- null separated possibly invalid UTF-8, very few characters are disallowed in paths
    PRIMARY KEY(name)
);"#,
    );
    update_to(
        con,
        3,
        initial_version,
        r#"
ALTER TABLE sessions
    ADD COLUMN groups BLOB; -- nullable
"#,
    );
}
