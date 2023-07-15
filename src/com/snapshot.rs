use std::path::Path;
use std::sync::Arc;

use super::{Entry, EntryObject};

#[derive(Debug, Clone, Copy)]
pub enum SnapshotKind {
    Complete,
    Start,
    Middle,
    End,
}

impl SnapshotKind {
    pub const fn initial(self) -> bool {
        match self {
            SnapshotKind::Complete | SnapshotKind::Start => true,
            SnapshotKind::Middle | SnapshotKind::End => false,
        }
    }

    pub const fn finished(self) -> bool {
        match self {
            SnapshotKind::Complete | SnapshotKind::End => true,
            SnapshotKind::Start | SnapshotKind::Middle => false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct SnapshotId {
    pub kind: SnapshotKind,
    pub path: Arc<Path>,
}

#[derive(Debug)]
pub struct DirSnapshot {
    pub id: SnapshotId,
    entries: Vec<Entry>,
}

impl DirSnapshot {
    pub const fn new(kind: SnapshotKind, path: Arc<Path>, entries: Vec<Entry>) -> Self {
        Self { id: SnapshotId { kind, path }, entries }
    }
}

// Separate from DirSnapshot so that, we only have one GObject per Entry.
// EntryObject uses GObject refcounting for cloning, which is cheaper and avoids the need to update
// each tab individually.
#[derive(Debug, Clone)]
pub struct EntryObjectSnapshot {
    pub id: SnapshotId,
    pub entries: Vec<EntryObject>,
}

impl From<DirSnapshot> for EntryObjectSnapshot {
    fn from(value: DirSnapshot) -> Self {
        Self {
            id: value.id,
            entries: value.entries.into_iter().map(EntryObject::new).collect(),
        }
    }
}
