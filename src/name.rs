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

//! Names and paths out of a git tree that have passed validation.
//!
//! Git forbids the dangerous names, but a hand-crafted object can hold anything,
//! so every name is checked before it becomes a path component. [`EntryName`]
//! and [`TreePath`] carry that check in the type: a [`TreePath`] can only be
//! built by joining names that were validated, so the functions that write below
//! a destination cannot be handed an arbitrary path by mistake.

use std::{
    ffi::OsStr,
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
};

use anyhow::{Result, bail};
use gix::bstr::{BStr, ByteSlice};

use crate::report;

/// A tree entry name safe to use as one path component.
#[derive(Debug, Clone, Copy)]
pub struct EntryName<'a>(&'a OsStr);

impl<'a> EntryName<'a> {
    /// Validate `name`, refusing what git refuses: an empty name, a path
    /// separator or NUL, `.` and `..`, any spelling of `.git`, and — for a
    /// symlink — any spelling of `.gitmodules`.
    pub fn new(name: &'a BStr, is_symlink: bool) -> Result<Self> {
        if name.is_empty() {
            bail!("tree entry with an empty name");
        }
        if name.contains(&b'/') || name.contains(&0) {
            bail!(
                "tree entry name {} contains a path separator or NUL",
                report::quoted(name)
            );
        }
        if name == "." || name == ".." {
            bail!(
                "tree entry name {} is a directory traversal",
                report::quoted(name)
            );
        }
        if is_spelling_of(name, ".git", "git~1") {
            bail!(
                "tree entry name {} would write into a .git directory",
                report::quoted(name)
            );
        }
        if is_symlink && is_spelling_of(name, ".gitmodules", "gitmod~1") {
            bail!(
                "tree entry {} is a symlink, which git never allows for .gitmodules",
                report::quoted(name)
            );
        }
        Ok(Self(OsStr::from_bytes(name)))
    }
}

/// `dotted` in any case, with trailing dots or spaces (dropped by Windows), or as
/// its 8.3 short name.
fn is_spelling_of(name: &BStr, dotted: &str, short_name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    let trimmed = lower.trim_end_with(|c| c == '.' || c == ' ');
    trimmed == dotted.as_bytes() || trimmed == short_name.as_bytes()
}

/// A relative path made only of [`EntryName`]s, so nothing in it can escape the
/// directory it is joined to. The root of a tree is the empty path.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct TreePath(PathBuf);

impl TreePath {
    /// The tree's own root, which names are joined onto.
    pub fn root() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn join(&self, name: EntryName<'_>) -> Self {
        Self(self.0.join(name.0))
    }

    pub fn as_path(&self) -> &Path {
        &self.0
    }

    pub fn display(&self) -> std::path::Display<'_> {
        self.0.display()
    }

    pub fn is_root(&self) -> bool {
        self.0.as_os_str().is_empty()
    }
}

impl AsRef<Path> for TreePath {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(name: &str) -> bool {
        EntryName::new(name.into(), false).is_ok()
    }

    #[test]
    fn rejects_traversal_and_git_directories() {
        assert!(ok("src"));
        assert!(ok("a.git"));
        assert!(ok(".gitmodules"));
        assert!(ok(".gitignore"));

        assert!(!ok(""));
        assert!(!ok("."));
        assert!(!ok(".."));
        assert!(!ok("a/b"));
        assert!(!ok("a\0b"));
        assert!(!ok(".git"));
        assert!(!ok(".GIT"));
        assert!(!ok(".Git."));
        assert!(!ok(".git "));
        assert!(!ok("git~1"));
    }

    #[test]
    fn rejects_symlinked_gitmodules_only() {
        let entry = |name: &str, link| EntryName::new(name.into(), link).is_ok();
        assert!(entry(".gitmodules", false));
        assert!(entry("modules", true));

        assert!(!entry(".gitmodules", true));
        assert!(!entry(".GITMODULES.", true));
        assert!(!entry("gitmod~1", true));
    }

    #[test]
    fn paths_grow_only_from_validated_names() {
        let path = TreePath::root();
        assert!(path.is_root());
        let vendor = path.join(EntryName::new("vendor".into(), false).unwrap());
        let nested = vendor.join(EntryName::new("lib".into(), false).unwrap());
        assert_eq!(nested.as_path(), Path::new("vendor/lib"));
        assert!(!nested.is_root());
    }
}
