use dirs::data_dir;
use rusqlite::{Connection, OpenFlags};

use crate::config::CONFIG;


#[derive(Debug)]
pub struct DBCon(Connection);

impl DBCon {
    fn connect() -> Self {
        let path = CONFIG
            .database
            .clone()
            .unwrap_or_else(|| data_dir().unwrap().join("aw-fm").join("settings.db"));
        debug!("Attempting to open database in {path:?}");
        // let flags = OpenFlags::
        Connection::open(path).unwrap();
        todo!()
    }
}
