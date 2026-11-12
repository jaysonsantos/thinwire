// SPDX-License-Identifier: AGPL-3.0-only
//! The files of the Signal session stores on disk: the live store, the
//! pending link, and the stores of previous accounts under `retained/`.
//!
//! Each previous account has a folder under `retained/` with its store and
//! a slot file. The slot is the number the app shows for that account. A
//! slot is given one time: `next-slot` keeps the next number, also after a
//! restore.
//!
//! This module works on folders only. It opens no store and it asks Signal
//! nothing, so its tests need a temporary folder and no network. No log
//! line and no file name that it makes has an account id or a phone number,
//! other than the folder name of a previous account.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use presage::libsignal_service::prelude::Uuid;

use super::path::{RETAINED_DIR, pending_store_path, prepare_session_dir, sqlite_store_path};

/// File in a retained directory. The contents are the slot count only.
/// The file name and the contents have no account id and no phone number.
pub(super) const RETAINED_SLOT_FILE: &str = "slot";

/// File in `retained/`. The contents are the next slot to assign.
/// A restore deletes a slot file and does not lower this number, so a
/// slot is not given to a later account.
pub(super) const RETAINED_NEXT_SLOT_FILE: &str = "next-slot";

pub(super) fn park_sqlite_blocking(session_dir: &Path, aci: Option<Uuid>) -> Result<(), ()> {
    let source = sqlite_store_path(session_dir);
    if !source.exists() {
        return Err(());
    }
    let dest_dir = unique_retained_dir(session_dir, aci)?;
    prepare_session_dir(&dest_dir).map_err(|_| ())?;
    rename_sqlite_set(&source, &sqlite_store_path(&dest_dir))?;
    // The slot is assigned once, in link order. A later directory sort does
    // not change it. The high-water mark stays put when a restore deletes
    // this file. A crash before the write is filled in on the next link,
    // before retained rows are read. A read does not fill it in.
    if assign_retained_slot(session_dir, &dest_dir).is_err() {
        tracing::error!("{}", RETAINED_SLOT_NOT_STORED);
    }
    Ok(())
}

fn assign_retained_slot(session_dir: &Path, dest_dir: &Path) -> Result<(), ()> {
    let slot = next_retained_slot(session_dir)?;
    write_slot_file(dest_dir, slot)?;
    let next = slot.checked_add(1).filter(|slot| *slot > 0).ok_or(())?;
    store_next_slot_mark(session_dir, next)
}

/// Next slot to assign. The mark in `retained/next-slot` wins when it is
/// ahead of the files still on disk, so a freed number is not reused.
fn next_retained_slot(session_dir: &Path) -> Result<u32, ()> {
    let mut next = 1u32;
    for dir in retained_store_dirs(session_dir) {
        if let Some(slot) = read_slot_file(&dir) {
            let after = slot.checked_add(1).filter(|slot| *slot > 0).ok_or(())?;
            next = next.max(after);
        }
    }
    if let Some(mark) = read_next_slot_mark(session_dir) {
        next = next.max(mark);
    }
    Ok(next)
}

/// Write the mark before a restore can delete the highest slot file.
/// No retained store and no mark yet means there is nothing to remember,
/// and this does not create `retained/`.
fn ensure_next_slot_mark(session_dir: &Path) -> Result<(), ()> {
    let has_store = !retained_store_dirs(session_dir).is_empty();
    let has_mark = read_next_slot_mark(session_dir).is_some();
    if !has_store && !has_mark {
        return Ok(());
    }
    store_next_slot_mark(session_dir, next_retained_slot(session_dir)?)
}

fn next_slot_mark_path(session_dir: &Path) -> PathBuf {
    session_dir.join(RETAINED_DIR).join(RETAINED_NEXT_SLOT_FILE)
}

fn read_next_slot_mark(session_dir: &Path) -> Option<u32> {
    let text = std::fs::read_to_string(next_slot_mark_path(session_dir)).ok()?;
    parse_slot_count(&text)
}

fn store_next_slot_mark(session_dir: &Path, next: u32) -> Result<(), ()> {
    if next == 0 {
        return Err(());
    }
    if read_next_slot_mark(session_dir).is_some_and(|current| current >= next) {
        return Ok(());
    }
    let root = session_dir.join(RETAINED_DIR);
    prepare_session_dir(&root).map_err(|_| ())?;
    replace_file(&next_slot_mark_path(session_dir), &format!("{next}\n"))
}

fn read_slot_file(dir: &Path) -> Option<u32> {
    let text = std::fs::read_to_string(dir.join(RETAINED_SLOT_FILE)).ok()?;
    parse_slot_count(&text)
}

fn parse_slot_count(text: &str) -> Option<u32> {
    let text = text.trim();
    if text.is_empty() || text.starts_with('+') || (text.len() > 1 && text.starts_with('0')) {
        return None;
    }
    if !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    text.parse().ok().filter(|slot: &u32| *slot > 0)
}

fn write_slot_file(dir: &Path, slot: u32) -> Result<(), ()> {
    replace_file(&dir.join(RETAINED_SLOT_FILE), &format!("{slot}\n"))
}

/// Write `text` over `path` by a rename in the same directory.
/// A crash keeps the previous file, so the path is never an empty file.
fn replace_file(path: &Path, text: &str) -> Result<(), ()> {
    use std::io::Write;

    let parent = path.parent().ok_or(())?;
    let name = path.file_name().ok_or(())?;
    let temp = parent.join(format!(
        ".{}.{}.tmp",
        name.to_string_lossy(),
        std::process::id()
    ));
    let _ = std::fs::remove_file(&temp);
    let written = (|| -> std::io::Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()
    })();
    let result = written.and_then(|()| std::fs::rename(&temp, path));
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result.map_err(|_| ())
}

/// Slot and directory for every retained store that already has a slot file.
/// This does not write. A directory with no file, or with a duplicate count,
/// is left out until [`repair_retained_slots`] runs on the link path.
/// The result is sorted by slot.
pub(super) fn retained_slots(session_dir: &Path) -> Vec<(u32, PathBuf)> {
    let mut used = HashSet::new();
    let mut ready = Vec::new();
    for dir in retained_store_dirs(session_dir) {
        if let Some(slot) = read_slot_file(&dir)
            && used.insert(slot)
        {
            ready.push((slot, dir));
        }
    }
    ready.sort_by_key(|(slot, _)| *slot);
    ready
}

/// Fill a slot file that a crash skipped.
///
/// [`park_sqlite_blocking`] renames the store, then writes the slot file.
/// A crash between those two steps leaves a directory with no slot. A
/// duplicate count is the same gap. This runs when a link is placed and
/// once at helper start, before retained rows are published. A read does
/// not call it. The number comes from the high-water mark, so it can
/// differ from the original link order.
pub(super) fn repair_retained_slots(session_dir: &Path) -> Result<(), ()> {
    ensure_next_slot_mark(session_dir)?;
    let mut missing = Vec::new();
    let mut used = HashSet::new();
    for dir in retained_store_dirs(session_dir) {
        match read_slot_file(&dir) {
            Some(slot) if used.insert(slot) => {}
            _ => missing.push(dir),
        }
    }
    for dir in missing {
        assign_retained_slot(session_dir, &dir)?;
    }
    Ok(())
}

/// Directory for one parked store.
///
/// The first park of an account uses `retained/<aci>`. When that directory
/// is still there, the next park uses `retained/<aci>-2`, then `-3`, and
/// so on. A failed removal is what leaves the first directory.
/// [`restore_retained_account`] and [`absorb_retained_copy`] read every
/// `<aci>-N` directory. `None` uses `retained/unstored`, then
/// `retained/unstored-2`. That name has no account id.
fn unique_retained_dir(session_dir: &Path, aci: Option<Uuid>) -> Result<PathBuf, ()> {
    let leaf = match aci {
        Some(aci) => aci.to_string(),
        None => "unstored".to_string(),
    };
    let root = session_dir.join(RETAINED_DIR);
    prepare_session_dir(&root).map_err(|_| ())?;
    let first = root.join(&leaf);
    if !first.exists() {
        return Ok(first);
    }
    for n in 2..10_000u32 {
        let candidate = root.join(format!("{leaf}-{n}"));
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    Err(())
}

pub(super) fn adopt_pending_if_session_missing(session_dir: &Path) -> Result<(), ()> {
    let session = sqlite_store_path(session_dir);
    let pending = pending_store_path(session_dir);
    if session.exists() || !pending.exists() {
        return Ok(());
    }
    rename_sqlite_set(&pending, &session)
}

pub(super) fn rename_sqlite_set(from: &Path, to: &Path) -> Result<(), ()> {
    if to.exists() {
        return Err(());
    }
    std::fs::rename(from, to).map_err(|_| ())?;
    for suffix in ["-wal", "-shm"] {
        let src = sqlite_sidecar(from, suffix);
        if !src.exists() {
            continue;
        }
        let dst = sqlite_sidecar(to, suffix);
        if std::fs::rename(&src, &dst).is_err() {
            // Callers run `wal_checkpoint(TRUNCATE)` before this rename, so a
            // `-wal` already in the destination is normally empty. Leave it
            // there. Move the main file back and ignore a failure of that move.
            let _ = std::fs::rename(to, from);
            return Err(());
        }
    }
    Ok(())
}

pub(super) fn sqlite_sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path
        .file_name()
        .map(std::ffi::OsStr::to_os_string)
        .unwrap_or_default();
    name.push(suffix);
    path.with_file_name(name)
}

pub(super) fn remove_sqlite_files(path: &Path) -> Result<(), ()> {
    for suffix in ["", "-wal", "-shm"] {
        let file = if suffix.is_empty() {
            path.to_path_buf()
        } else {
            sqlite_sidecar(path, suffix)
        };
        if file.exists() && std::fs::remove_file(&file).is_err() {
            return Err(());
        }
    }
    Ok(())
}

fn retained_account_dirs(session_dir: &Path, aci: Uuid) -> Vec<PathBuf> {
    let aci = aci.to_string();
    retained_dirs(session_dir)
        .into_iter()
        .filter(|dir| {
            dir.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| retained_name_is_aci(name, &aci))
        })
        .collect()
}

pub(super) fn retained_account_stores(session_dir: &Path, aci: Uuid) -> Vec<PathBuf> {
    retained_account_dirs(session_dir, aci)
        .into_iter()
        .map(|dir| sqlite_store_path(&dir))
        .filter(|path| path.is_file())
        .collect()
}

/// The newest parked store. A higher slot is newer. With no slot file,
/// a higher `<aci>-N` suffix is newer than `retained/<aci>`.
pub(super) fn preferred_retained_store(stores: &[PathBuf], aci: &str) -> Option<PathBuf> {
    stores
        .iter()
        .max_by_key(|path| {
            let dir = path.parent().unwrap_or(path);
            (
                read_slot_file(dir).unwrap_or(0),
                retained_suffix_index(dir, aci),
            )
        })
        .cloned()
}

fn retained_suffix_index(dir: &Path, aci: &str) -> u32 {
    let Some(name) = dir.file_name().and_then(|name| name.to_str()) else {
        return 0;
    };
    if name == aci {
        return 1;
    }
    name.strip_prefix(aci)
        .and_then(|rest| rest.strip_prefix('-'))
        .and_then(parse_slot_count)
        .unwrap_or(0)
}

/// `true` when `name` is `retained/<aci>` or `retained/<aci>-N` for `N >= 2`.
pub(super) fn retained_name_is_aci(name: &str, aci: &str) -> bool {
    if name == aci {
        return true;
    }
    let Some(suffix) = name
        .strip_prefix(aci)
        .and_then(|rest| rest.strip_prefix('-'))
    else {
        return false;
    };
    parse_slot_count(suffix).is_some_and(|n| n >= 2)
}

pub(super) fn remove_retained_dir(session_dir: &Path, aci: Uuid) -> Result<(), ()> {
    for dir in retained_account_dirs(session_dir, aci) {
        remove_retained_tree(&dir)?;
    }
    Ok(())
}

pub(super) fn remove_retained_tree(dir: &Path) -> Result<(), ()> {
    let sqlite = sqlite_store_path(dir);
    if sqlite.exists() {
        remove_sqlite_files(&sqlite)?;
    }
    let slot = dir.join(RETAINED_SLOT_FILE);
    if slot.exists() && std::fs::remove_file(&slot).is_err() {
        return Err(());
    }
    // `replace_file` names a temp `.<name>.<pid>.tmp`. A crash can leave a
    // file from another pid. `remove_dir` is not recursive, so that file
    // would keep this directory.
    remove_replace_temps(dir)?;
    if dir.exists() && std::fs::remove_dir(dir).is_err() {
        return Err(());
    }
    Ok(())
}

/// Remove `.<name>.<pid>.tmp` files that [`replace_file`] left in `dir`.
fn remove_replace_temps(dir: &Path) -> Result<(), ()> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(_) => return Err(()),
    };
    for entry in entries {
        let entry = entry.map_err(|_| ())?;
        if entry.file_type().map(|kind| kind.is_dir()).unwrap_or(true) {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.')
            && name.ends_with(".tmp")
            && std::fs::remove_file(entry.path()).is_err()
        {
            return Err(());
        }
    }
    Ok(())
}

pub(super) const RETAINED_SLOT_NOT_STORED: &str =
    "signal could not store the previous account slot";

pub(super) fn retained_dir_is_live(dir: &Path, live: Option<Uuid>) -> bool {
    let Some(live) = live else {
        return false;
    };
    dir.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| retained_name_is_aci(name, &live.to_string()))
}

pub(super) fn retained_dirs(session_dir: &Path) -> Vec<PathBuf> {
    let root = session_dir.join(RETAINED_DIR);
    let Ok(read) = std::fs::read_dir(&root) else {
        return Vec::new();
    };
    let mut dirs = Vec::new();
    for entry in read.flatten() {
        let path = entry.path();
        if path.is_dir() {
            dirs.push(path);
        }
    }
    dirs.sort();
    dirs
}

pub(super) fn retained_store_dirs(session_dir: &Path) -> Vec<PathBuf> {
    retained_dirs(session_dir)
        .into_iter()
        .filter(|path| sqlite_store_path(path).is_file())
        .collect()
}

pub(super) fn retained_dir_for_slot(session_dir: &Path, slot: u32) -> Option<PathBuf> {
    retained_slots(session_dir)
        .into_iter()
        .find(|(found, _)| *found == slot)
        .map(|(_, dir)| dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A new empty session folder for one test. The test removes it at its
    /// end.
    struct Folder(PathBuf);

    impl Folder {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "thinwire-signal-retained-{}-{name}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("session folder");
            Self(dir)
        }

        /// Write a live store file, and move it to `retained/` as the
        /// previous account `account`.
        fn park(&self, account: u128) {
            std::fs::write(sqlite_store_path(&self.0), b"store").expect("live store");
            park_sqlite_blocking(&self.0, Some(Uuid::from_u128(account))).expect("park");
        }

        fn slots(&self) -> Vec<u32> {
            retained_slots(&self.0)
                .into_iter()
                .map(|(slot, _)| slot)
                .collect()
        }
    }

    impl Drop for Folder {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_slot_file_holds_one_plain_positive_number() {
        assert_eq!(parse_slot_count("7\n"), Some(7));
        assert_eq!(parse_slot_count(" 12 "), Some(12));
        for bad in ["", "0", "07", "+7", "-1", "7a", "7 8", "4294967296"] {
            assert_eq!(parse_slot_count(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn each_parked_store_gets_the_next_slot_in_link_order() {
        let folder = Folder::new("order");
        assert!(park_sqlite_blocking(&folder.0, None).is_err(), "no store");
        folder.park(1);
        folder.park(2);
        assert_eq!(folder.slots(), [1, 2]);
        assert!(
            !sqlite_store_path(&folder.0).exists(),
            "the live store moved"
        );
        assert_eq!(read_next_slot_mark(&folder.0), Some(3));
        let first = retained_dir_for_slot(&folder.0, 1).expect("slot 1");
        assert!(retained_dir_is_live(&first, Some(Uuid::from_u128(1))));
        assert!(!retained_dir_is_live(&first, Some(Uuid::from_u128(2))));
        assert!(!retained_dir_is_live(&first, None));
    }

    #[test]
    fn a_slot_that_a_restore_freed_is_not_given_again() {
        let folder = Folder::new("freed");
        folder.park(1);
        folder.park(2);
        remove_retained_dir(&folder.0, Uuid::from_u128(2)).expect("restore");
        assert_eq!(folder.slots(), [1]);
        folder.park(3);
        assert_eq!(folder.slots(), [1, 3]);
    }

    #[test]
    fn the_same_account_parked_again_gets_a_second_folder() {
        let folder = Folder::new("again");
        folder.park(1);
        folder.park(1);
        assert_eq!(folder.slots(), [1, 2]);
        assert_eq!(
            retained_account_stores(&folder.0, Uuid::from_u128(1)).len(),
            2
        );
        assert!(retained_account_stores(&folder.0, Uuid::from_u128(2)).is_empty());
    }

    #[test]
    fn a_repair_fills_a_missing_slot_and_a_duplicate_slot() {
        let folder = Folder::new("repair");
        folder.park(1);
        folder.park(2);
        folder.park(3);
        let second = retained_dir_for_slot(&folder.0, 2).expect("slot 2");
        let third = retained_dir_for_slot(&folder.0, 3).expect("slot 3");
        // A crash before the slot write, and a duplicate count.
        std::fs::remove_file(second.join(RETAINED_SLOT_FILE)).expect("remove slot");
        write_slot_file(&third, 1).expect("duplicate");
        assert_eq!(folder.slots(), [1], "a read does not repair");
        repair_retained_slots(&folder.0).expect("repair");
        let slots = folder.slots();
        assert_eq!(slots.len(), 3);
        assert_eq!(slots[0], 1);
        assert!(slots[1..].iter().all(|slot| *slot >= 4), "{slots:?}");
    }

    #[test]
    fn a_folder_with_no_previous_account_gets_no_retained_folder() {
        let folder = Folder::new("empty");
        repair_retained_slots(&folder.0).expect("repair");
        assert!(!folder.0.join(RETAINED_DIR).exists());
        assert!(retained_dirs(&folder.0).is_empty());
    }
}
