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
    fs,
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

/// Build the root-level selection, or `None` when every submodule is included.
pub fn selection(
    includes: &[String],
    excludes: &[String],
) -> Result<Option<gix::pathspec::Search>> {
    if includes.is_empty() && excludes.is_empty() {
        return Ok(None);
    }
    let parse = |pattern: &String| {
        gix::pathspec::parse(pattern.as_bytes(), gix::pathspec::Defaults::default())
            .with_context(|| format!("invalid submodule pathspec {}", report::quoted(pattern)))
    };
    let patterns = includes
        .iter()
        .map(parse)
        .chain(excludes.iter().map(|pattern| {
            // Set on the parsed pattern: a textual `:(exclude)` prefix would
            // turn magic the pattern has of its own, like `:(glob)`, into a path.
            let mut pattern = parse(pattern)?;
            pattern.signature |= gix::pathspec::MagicSignature::EXCLUDE;
            Ok(pattern)
        }))
        .collect::<Result<Vec<_>>>()?;
    Ok(Some(
        gix::pathspec::Search::from_specs(patterns, None, Path::new(""))
            .context("cannot normalize submodule pathspecs")?,
    ))
}

/// Pair every selected gitlink with its `.gitmodules` entry and resolve its URL.
pub fn plan(
    gitlinks: &[Gitlink],
    modules: &Modules,
    parent_url: Option<&Url>,
    basedir: &Path,
    config: &gix::config::File,
    reporter: &Reporter,
    mut selection: Option<&mut gix::pathspec::Search>,
) -> Result<Vec<Planned>> {
    let mut planned = Vec::with_capacity(gitlinks.len());
    for link in gitlinks {
        // The path comes from the validated tree, never from `.gitmodules`.
        let destination = basedir.join(&link.path);
        if let Some(search) = &mut selection {
            let selected = search
                .pattern_matching_relative_path(
                    fs::path_bytes(link.path.as_path()),
                    Some(true),
                    &mut |_, _, _, _| false,
                )
                .is_some_and(|matched| !matched.pattern.is_excluded());
            if !selected {
                report::detail!(
                    reporter,
                    "Skipping submodule {}: not selected",
                    report::quoted(destination.display())
                );
                continue;
            }
        }

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
