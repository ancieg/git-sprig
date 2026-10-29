// git-sprig - materialize git submodules as plain files, without their history
// Copyright (C) 2026  Anton Zhukharev <ancieg@altlinux.org>
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <https://www.gnu.org/licenses/>.

//! Filesystem operations that follow no symlinks. All Unix specifics live here.

use std::{
    collections::VecDeque,
    ffi::OsStr,
    fs,
    os::unix::{ffi::OsStrExt, fs::OpenOptionsExt},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use gix::bstr::{BStr, ByteSlice};

use crate::{name::TreePath, report};

/// Remove whatever sits at `path`, following no symlinks.
pub fn remove_existing(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_dir() => fs::remove_dir_all(path)
            .with_context(|| format!("cannot remove directory {}", report::quoted(path.display()))),
        Ok(_) => fs::remove_file(path)
            .with_context(|| format!("cannot remove {}", report::quoted(path.display()))),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => {
            Err(err).with_context(|| format!("cannot inspect {}", report::quoted(path.display())))
        }
    }
}

/// Make sure `path` is a real directory, replacing a symlink or file rather than following it.
pub fn ensure_dir(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_dir() => Ok(()),
        Ok(_) => {
            remove_existing(path)?;
            fs::create_dir(path)
                .with_context(|| format!("cannot create {}", report::quoted(path.display())))
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => fs::create_dir_all(path)
            .with_context(|| format!("cannot create {}", report::quoted(path.display()))),
        Err(err) => {
            Err(err).with_context(|| format!("cannot inspect {}", report::quoted(path.display())))
        }
    }
}

/// Fail unless every component of `relative` below `base` is a real directory.
pub fn ensure_real_dirs(base: &Path, relative: &TreePath) -> Result<()> {
    let mut path = base.to_owned();
    for component in relative.as_path().components() {
        path.push(component);
        match fs::symlink_metadata(&path) {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => bail!(
                "{} is not a directory, refusing to write below it",
                report::quoted(path.display())
            ),
            Err(err) => {
                return Err(err)
                    .with_context(|| format!("cannot inspect {}", report::quoted(path.display())));
            }
        }
    }
    Ok(())
}

/// Whether `path` is a directory with a `.git` of its own.
pub fn is_checkout(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|meta| meta.is_dir())
        && fs::symlink_metadata(path.join(".git")).is_ok()
}

/// The shallowest directory at or below `path` with a `.git` of its own, if
/// any. A symlink is not followed, since replacing it leaves its target alone.
pub fn find_checkout(path: &Path) -> Result<Option<PathBuf>> {
    if !fs::symlink_metadata(path).is_ok_and(|meta| meta.is_dir()) {
        return Ok(None);
    }
    // Breadth first, so a checkout near the top is found without walking
    // everything else first.
    let mut pending = VecDeque::from([path.to_owned()]);
    while let Some(dir) = pending.pop_front() {
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(err) => {
                return Err(err)
                    .with_context(|| format!("cannot list {}", report::quoted(dir.display())));
            }
        };
        let mut subdirs = Vec::new();
        for entry in entries {
            let entry =
                entry.with_context(|| format!("cannot list {}", report::quoted(dir.display())))?;
            if entry.file_name() == ".git" {
                return Ok(Some(dir));
            }
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                subdirs.push(entry.path());
            }
        }
        pending.extend(subdirs);
    }
    Ok(None)
}

/// Create `path` with git's checkout modes (the umask applies), following no symlink.
pub fn create_file(path: &Path, executable: bool) -> Result<fs::File> {
    let mode = if executable { 0o777 } else { 0o666 };
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(path)
        .with_context(|| format!("cannot create {}", report::quoted(path.display())))
}

/// Create a symlink at `link` pointing at the raw bytes `target`, as git stores them.
pub fn symlink(link: &Path, target: &[u8]) -> std::io::Result<()> {
    std::os::unix::fs::symlink(OsStr::from_bytes(target), link)
}

/// A path as the bytes git addresses it by.
pub fn path_bytes(path: &Path) -> &BStr {
    path.as_os_str().as_bytes().as_bstr()
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::name::EntryName;

    fn tree_path(parts: &[&str]) -> TreePath {
        parts.iter().fold(TreePath::root(), |path, part| {
            path.join(EntryName::new((*part).into(), false).unwrap())
        })
    }

    #[test]
    fn real_dirs_have_no_symlink_on_the_way() {
        let base = tempfile::tempdir().unwrap();
        fs::create_dir_all(base.path().join("a/b")).unwrap();
        std::os::unix::fs::symlink(base.path().join("a"), base.path().join("link")).unwrap();
        fs::write(base.path().join("file"), "").unwrap();

        assert!(ensure_real_dirs(base.path(), &tree_path(&["a", "b"])).is_ok());
        assert!(ensure_real_dirs(base.path(), &tree_path(&["link", "b"])).is_err());
        assert!(ensure_real_dirs(base.path(), &tree_path(&["file"])).is_err());
        assert!(ensure_real_dirs(base.path(), &tree_path(&["a", "missing"])).is_err());
    }
}
