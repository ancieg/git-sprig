// git-sprig - materialize git repositories and submodules as plain files, without their history
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

//! The refusals that keep git-sprig from destroying work: a destination is
//! replaced as a whole, so whatever git checkout it is, holds or belongs to
//! is lost with it unless `--force` says so.

use std::path::Path;

use anyhow::{Context, Result, bail};
use gix::Repository;

use crate::{fs, report, submodule::plan::Planned};

/// Replacing a submodule checkout discards whatever work is in it.
pub fn refuse_checkouts(planned: &[Planned]) -> Result<()> {
    let checkouts: Vec<String> = planned
        .iter()
        .filter(|planned| fs::is_checkout(&planned.destination))
        .map(|planned| report::quoted(planned.destination.display()).to_string())
        .collect();
    if !checkouts.is_empty() {
        bail!(
            "refusing to replace {} submodule checkout(s) with a .git of their own: {}\n\
             Replacing them discards any uncommitted or unpushed work in them; \
             pass --force to do it anyway.",
            checkouts.len(),
            checkouts.join(", ")
        );
    }
    Ok(())
}

/// The whole destination goes, so a checkout anywhere inside it would too.
pub fn refuse_holding_checkouts(destination: &Path) -> Result<()> {
    let Some(checkout) = fs::find_checkout(destination)? else {
        return Ok(());
    };
    if checkout == destination {
        bail!(
            "refusing to replace {}, which is a git checkout with a .git of its own\n\
             Replacing it discards any uncommitted or unpushed work; pass --force to do it \
             anyway.",
            report::quoted(destination.display())
        );
    }
    bail!(
        "refusing to replace {}, which holds the git checkout {}\n\
         Replacing it discards any uncommitted or unpushed work there; pass --force to do it \
         anyway.",
        report::quoted(destination.display()),
        report::quoted(checkout.display())
    );
}

/// Replacing a destination that holds the local source deletes the source
/// too, along with whatever work is in it. A linked worktree needs its
/// common directory as well, which may live elsewhere.
pub fn refuse_enclosing_destination(destination: &Path, repo: &Repository) -> Result<()> {
    let canonical = match std::fs::canonicalize(destination) {
        Ok(canonical) => canonical,
        // A destination that does not exist yet holds nothing.
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => {
            return Err(err).with_context(|| {
                format!("cannot inspect {}", report::quoted(destination.display()))
            });
        }
    };
    let parts = [
        repo.workdir(),
        Some(repo.git_dir()),
        Some(repo.common_dir()),
    ];
    for part in parts.into_iter().flatten() {
        let part = std::fs::canonicalize(part)
            .with_context(|| format!("cannot inspect {}", report::quoted(part.display())))?;
        if part.starts_with(&canonical) {
            bail!(
                "refusing to replace {}, which contains the source repository\n\
                 Replacing it deletes the source and any uncommitted or unpushed work in \
                 it; pass --force to do it anyway.",
                report::quoted(destination.display())
            );
        }
    }
    Ok(())
}
