use std::path::Path;
use std::sync::atomic::AtomicBool;
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
    pub id: Arc<AtomicBool>,
    pub path: Arc<Path>,
}

#[derive(Debug)]
pub struct DirSnapshot {
    pub id: SnapshotId,
    entries: Vec<Entry>,
}

impl DirSnapshot {
    pub fn new(
        kind: SnapshotKind,
        path: &Arc<Path>,
        id: &Arc<AtomicBool>,
        entries: Vec<Entry>,
    ) -> Self {
        Self {
            id: SnapshotId { kind, id: id.clone(), path: path.clone() },
            entries,
        }
    }
}

// Separate from DirSnapshot so that, we only have one GObject per Entry.
// EntryObject uses GObject refcounting for cloning, which is cheaper and avoids the need to update
// each tab individually.
#[derive(Debug, Clone)]
pub struct EntryObjectSnapshot {
    pub id: SnapshotId,
    pub entries: Vec<EntryObject>,
    // Rare case, handle by just forcing a sort of all search tabs.
    pub had_search_updates: bool,
}

impl From<DirSnapshot> for EntryObjectSnapshot {
    fn from(value: DirSnapshot) -> Self {
        let mut had_search_updates = false;
        let entries = value
            .entries
            .into_iter()
            .map(|entry| {
                let (eo, updated) = EntryObject::create_or_update(entry);
                had_search_updates = had_search_updates || updated.is_some();
                eo
            })
            .collect();

        Self {
            id: value.id,
            entries,
            had_search_updates,
        }
    }
}
