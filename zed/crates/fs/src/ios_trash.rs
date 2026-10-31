//! Trash support on iOS through `NSFileManager`.
//!
//! `trashItemAtURL` moves files in Files-app locations (iCloud Drive, On My
//! iPhone) to that provider's Recently Deleted, and files in the app container
//! to the container's own trash, which is what a user expects from "delete".

use std::{ffi::OsString, path::PathBuf};

use anyhow::{Context as _, Result, anyhow};
use objc2::rc::Retained;
use objc2_foundation::{NSFileManager, NSString, NSURL};

use crate::{TrashRestoreError, TrashedEntry};

pub(crate) fn delete_with_info(path: PathBuf) -> Result<TrashedEntry> {
    let name = path
        .file_name()
        .map(OsString::from)
        .with_context(|| format!("{} has no file name", path.display()))?;
    let original_parent = path
        .parent()
        .map(PathBuf::from)
        .with_context(|| format!("{} has no parent", path.display()))?;
    let path_string = path
        .to_str()
        .with_context(|| format!("{} is not valid UTF-8", path.display()))?;

    let url = NSURL::fileURLWithPath(&NSString::from_str(path_string));
    let mut trashed_url: Option<Retained<NSURL>> = None;
    NSFileManager::defaultManager()
        .trashItemAtURL_resultingItemURL_error(&url, Some(&mut trashed_url))
        .map_err(|error| anyhow!("trashItemAtURL failed for {}: {error}", path.display()))?;
    let trashed_path = trashed_url
        .and_then(|url| url.path())
        .context("trashItemAtURL did not report where the item went")?;

    Ok(TrashedEntry {
        id: OsString::from(trashed_path.to_string()),
        name,
        original_parent,
    })
}

pub(crate) fn restore(entry: TrashedEntry) -> Result<(), TrashRestoreError> {
    let source = PathBuf::from(&entry.id);
    let destination = entry.original_parent.join(&entry.name);
    if destination.exists() {
        return Err(TrashRestoreError::Collision { path: destination });
    }
    if !source.exists() {
        return Err(TrashRestoreError::NotFound { path: source });
    }
    std::fs::rename(&source, &destination).map_err(|error| TrashRestoreError::Unknown {
        description: format!(
            "moving {} back to {}: {error}",
            source.display(),
            destination.display()
        ),
    })
}
