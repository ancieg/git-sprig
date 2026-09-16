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

//! Pairing every gitlink with its `.gitmodules` entry and resolving its URL.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use gix::{ObjectId, Url, bstr::BString};

use crate::{
    name::TreePath,
    report::{self, Reporter},
    submodule::{gitmodules::Modules, url},
    tree::Gitlink,
};

pub struct Planned {
    /// The submodule's name in `.gitmodules`.
    pub name: BString,
    pub url: Url,
    pub commit: ObjectId,
    /// Relative to the tree the gitlink was found in.
    pub path: TreePath,
    pub destination: PathBuf,
}

/// Pair every gitlink with its `.gitmodules` entry and resolve its URL.
pub fn plan(
    gitlinks: &[Gitlink],
    modules: &Modules,
    parent_url: Option<&Url>,
    basedir: &Path,
    config: &gix::config::File,
    reporter: &Reporter,
) -> Result<Vec<Planned>> {
    let mut planned = Vec::with_capacity(gitlinks.len());
    for link in gitlinks {
        // The path comes from the validated tree, never from `.gitmodules`.
        let destination = basedir.join(&link.path);

        let Some(entry) = modules.by_path(link.path.as_path()) else {
            report::warning!(
                reporter,
                "skipping gitlink {}, which has no .gitmodules entry",
                report::quoted(destination.display())
            );
            continue;
        };

        let active = modules.is_active(entry, config).with_context(|| {
            format!(
                "cannot tell whether submodule {} is active",
                report::quoted(&entry.name)
            )
        })?;
        if !active {
            report::detail!(
                reporter,
                "Skipping submodule {}: not active",
                report::quoted(destination.display())
            );
            continue;
        }

        let url = url::resolve_url(entry.url.as_ref(), parent_url).with_context(|| {
            format!(
                "cannot resolve url {} of submodule {}",
                report::quoted(&entry.url),
                report::quoted(&entry.name)
            )
        })?;

        planned.push(Planned {
            name: entry.name.clone(),
            url,
            commit: link.commit,
            path: link.path.clone(),
            destination,
        });
    }
    Ok(planned)
}
