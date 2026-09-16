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

//! A snapshot built in a hidden directory next to its destination and swapped
//! into place atomically, so a failure or interrupt leaves the old contents.

use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};

use crate::{
    cancel::CancelToken,
    report::{self, Reporter},
};

/// A directory next to a destination that a snapshot is built in. Dropping it
/// without [`Snapshot::commit`] removes it and leaves the destination alone.
pub struct Snapshot {
    dir: tempfile::TempDir,
    destination: PathBuf,
    cancel: CancelToken,
}

impl Snapshot {
    pub fn new(destination: &Path, cancel: &CancelToken) -> Result<Self> {
        let parent = parent_of(destination);
        fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", report::quoted(parent.display())))?;
        let dir = tempfile::Builder::new()
            .prefix(".git-sprig-")
            // Git's directory mode instead of tempfile's 0700; the umask applies.
            .permissions(fs::Permissions::from_mode(0o777))
            .tempdir_in(parent)
            .with_context(|| {
                format!(
                    "cannot create a staging directory in {}",
                    report::quoted(parent.display())
                )
            })?;
        cancel.register(dir.path());
        Ok(Self {
            dir,
            destination: destination.to_owned(),
            cancel: cancel.clone(),
        })
    }

    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    /// Move the snapshot into place. The old destination is moved aside first
    /// and put back if that fails.
    pub fn commit(mut self, reporter: &Reporter) -> Result<()> {
        let destination = &self.destination;
        let parent = parent_of(destination);
        let trash = tempfile::Builder::new()
            .prefix(".git-sprig-old-")
            .tempdir_in(parent)
            .with_context(|| {
                format!(
                    "cannot create a directory in {}",
                    report::quoted(parent.display())
                )
            })?;
        let displaced = trash.path().join("previous");

        let had_previous = match fs::symlink_metadata(destination) {
            Ok(_) => {
                fs::rename(destination, &displaced).with_context(|| {
                    format!(
                        "cannot move {} out of the way",
                        report::quoted(destination.display())
                    )
                })?;
                true
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => false,
            Err(err) => {
                return Err(err).with_context(|| {
                    format!("cannot inspect {}", report::quoted(destination.display()))
                });
            }
        };

        if let Err(err) = fs::rename(self.dir.path(), destination) {
            if had_previous && let Err(restore) = fs::rename(&displaced, destination) {
                let kept = trash.keep();
                return Err(err).with_context(|| {
                    format!(
                        "cannot move the snapshot to {}, and its previous contents could not be \
                         put back either ({restore}); they are in {}",
                        report::quoted(destination.display()),
                        report::quoted(kept.display())
                    )
                });
            }
            return Err(err).with_context(|| {
                format!(
                    "cannot move the snapshot to {}",
                    report::quoted(destination.display())
                )
            });
        }
        self.dir.disable_cleanup(true);

        if let Err(err) = trash.close() {
            report::warning!(
                reporter,
                "cannot remove the previous contents of {}: {err}",
                report::quoted(destination.display())
            );
        }
        Ok(())
    }
}

impl Drop for Snapshot {
    fn drop(&mut self) {
        self.cancel.forget(self.dir.path());
    }
}

fn parent_of(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
}
