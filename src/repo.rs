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

//! Finding local repositories as git finds them, and reading from them what
//! both modes need: the commit a revision names, and the URL that relative
//! submodule URLs are resolved against.

use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use gix::{ObjectId, Repository, ThreadSafeRepository, Url};

use crate::{fs, report};

/// Open the repository at or above `at`, with the `-c` settings applied as
/// `git -c` applies them.
pub fn discover(at: &Path, overrides: &[String]) -> Result<Repository> {
    map_discover_error(
        at,
        gix::discover_opts(
            at,
            gix::discover::upwards::Options::default(),
            gix::open::Options::default().cli_overrides(overrides.iter().map(String::as_str)),
        ),
    )
}

/// The superproject, found as git finds it: through `GIT_DIR` and
/// `GIT_WORK_TREE` if they are set, as `git --git-dir` and `git --work-tree`
/// set them, and upwards from the current directory otherwise.
pub fn discover_superproject(overrides: &[String]) -> Result<Repository> {
    let options = gix::open::Options::default().cli_overrides(overrides.iter().map(String::as_str));
    map_discover_error(
        Path::new("."),
        ThreadSafeRepository::discover_with_environment_overrides_opts(
            ".",
            gix::discover::upwards::Options::default(),
            gix::sec::trust::Mapping {
                full: options.clone(),
                reduced: options,
            },
        )
        .map(Into::into),
    )
}

/// git's own wording for the two ordinary cases; gix states them again in its
/// own words, naming the path a second time.
fn map_discover_error(
    at: &Path,
    result: std::result::Result<Repository, gix::discover::Error>,
) -> Result<Repository> {
    use gix::discover::{Error::Discover, upwards::Error as Upwards};

    result.map_err(|err| match err {
        Discover(
            Upwards::NoGitRepository { .. }
            | Upwards::NoGitRepositoryWithinCeiling { .. }
            | Upwards::NoGitRepositoryWithinFs { .. },
        ) => anyhow!(
            "not a git repository (or any of the parent directories): {}",
            report::quoted(at.display())
        ),
        Discover(Upwards::InaccessibleDirectory { path }) => {
            anyhow!("cannot access {}", report::quoted(path.display()))
        }
        other => anyhow::Error::new(other).context(format!(
            "not a git repository: {}",
            report::quoted(at.display())
        )),
    })
}

pub fn resolve_commit_id(repo: &Repository, rev: &str) -> Result<ObjectId> {
    Ok(repo
        .rev_parse_single(rev)
        .with_context(|| format!("cannot resolve revision {}", report::quoted(rev)))?
        .object()
        .context("cannot read the resolved object")?
        .peel_to_commit()
        .with_context(|| {
            format!(
                "revision {} does not point at a commit",
                report::quoted(rev)
            )
        })?
        .id)
}

/// The absolute path of a local repository, for relative submodule URLs to be
/// resolved against: the path `git clone` records for a local source, and the
/// one git falls back to for a superproject without a remote. It does not
/// depend on how the path was spelled or on which subdirectory it named. `.`
/// and `..` are folded first, since resolving takes every component for a
/// directory.
pub fn local_url(repo: &Repository) -> Result<Url> {
    let path = repo.workdir().unwrap_or_else(|| repo.git_dir());
    let absolute = std::path::absolute(path)
        .with_context(|| format!("cannot make {} absolute", report::quoted(path.display())))?;
    let mut folded = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                folded.pop();
            }
            other => folded.push(other),
        }
    }
    gix::url::parse(fs::path_bytes(&folded))
        .with_context(|| format!("cannot use {} as a URL", report::quoted(folded.display())))
}

pub fn default_remote_url(repo: &Repository) -> Option<Url> {
    repo.find_default_remote(gix::remote::Direction::Fetch)
        .and_then(Result::ok)
        .and_then(|remote| {
            remote
                .url(gix::remote::Direction::Fetch)
                .map(ToOwned::to_owned)
        })
}
