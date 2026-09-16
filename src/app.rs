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

//! One run of git-sprig: resolve the revision, plan, check, plant and stage.

use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use gix::{Repository, Url};

use crate::{
    cancel::CancelToken,
    config::{Checkouts, Config, IndexUpdate, Mode},
    fetch::Fetcher,
    fs, index,
    report::{self, Reporter},
    schedule::{self, Recursion},
    submodule::{
        gitmodules::Modules,
        plan::{self, Planned},
    },
    tree,
};

/// git's own wording for the two ordinary cases; gix states them again in its
/// own words, naming the path a second time.
fn discover(at: &Path) -> Result<Repository> {
    use gix::discover::{Error::Discover, upwards::Error as Upwards};

    gix::discover(at).map_err(|err| match err {
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

pub(crate) fn run(config: Config, reporter: &Reporter, cancel: &CancelToken) -> Result<()> {
    cancel.install_handler()?;

    let repo = discover(&config.repo)?;

    let commit = repo
        .rev_parse_single(config.rev.as_str())
        .with_context(|| format!("cannot resolve revision {}", report::quoted(&config.rev)))?
        .object()
        .context("cannot read the resolved object")?
        .peel_to_commit()
        .with_context(|| {
            format!(
                "revision {} does not point at a commit",
                report::quoted(&config.rev)
            )
        })?;

    let base_url = match &config.remote_url {
        Some(url) => Some(
            gix::url::parse(gix::bstr::BStr::new(url.as_str())).with_context(|| {
                format!(
                    "--remote-url {} is not a valid git URL",
                    report::quoted(url)
                )
            })?,
        ),
        None => default_remote_url(&repo),
    };

    let tree_id = commit
        .tree_id()
        .context("cannot read the superproject tree")?
        .detach();
    let scan = tree::scan_tree(&repo, tree_id).context("cannot scan the superproject")?;
    let modules = Modules::from_tree(&repo, tree_id, reporter)
        .context("cannot read .gitmodules of the superproject")?;
    let git_config = repo.config_snapshot();
    let planned = plan::plan(
        &scan.gitlinks,
        &modules,
        base_url.as_ref(),
        &config.basedir,
        git_config.plumbing(),
        reporter,
    )?;

    // Both checked before anything is fetched.
    let work_tree = (config.index == IndexUpdate::Stage)
        .then(|| index::work_tree(&repo, &config.basedir))
        .transpose()?;
    if config.checkouts == Checkouts::Refuse {
        refuse_checkouts(&planned)?;
    }

    if config.mode == Mode::DryRun {
        report_dry_run(&planned, config.index, config.recursion, reporter);
        return Ok(());
    }

    std::fs::create_dir_all(&config.basedir)
        .with_context(|| format!("cannot create {}", report::quoted(config.basedir.display())))?;

    let submodules: Vec<index::Submodule> = if work_tree.is_some() {
        planned
            .iter()
            .map(|planned| index::Submodule {
                name: planned.name.clone(),
                path: planned.path.clone(),
            })
            .collect()
    } else {
        Vec::new()
    };

    let fetcher = Fetcher::new(config.tempdir, config.overrides, cancel)
        .context("cannot create temporary storage")?;
    // Past eight threads, writing gets no faster.
    let options = schedule::Options {
        conversion: config.conversion,
        recursion: config.recursion,
        jobs: config.jobs,
        write_threads: available_cpus().min(8),
    };
    schedule::run(
        planned,
        &config.basedir,
        &options,
        &fetcher,
        reporter,
        cancel,
    )?;

    if let Some(work_tree) = &work_tree {
        index::stage(work_tree, &submodules, reporter)?;
    }
    Ok(())
}

/// Replacing a submodule checkout discards whatever work is in it.
fn refuse_checkouts(planned: &[Planned]) -> Result<()> {
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

fn report_dry_run(
    planned: &[Planned],
    index: IndexUpdate,
    recursion: Recursion,
    reporter: &Reporter,
) {
    for planned in planned {
        report::status!(
            reporter,
            "Would fetch submodule {} from {} at {}",
            report::quoted(planned.path.display()),
            report::quoted(&planned.url),
            report::quoted(planned.commit)
        );
    }
    if index == IndexUpdate::Stage {
        for planned in planned {
            report::status!(
                reporter,
                "Would stage submodule path {} as plain files",
                report::quoted(planned.path.display())
            );
        }
    }
    if !matches!(recursion, Recursion::Off) && !planned.is_empty() {
        report::hint!(
            reporter,
            "nested submodules are only found once their parents are fetched"
        );
    }
}

pub(crate) fn available_cpus() -> usize {
    std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get)
}

fn default_remote_url(repo: &Repository) -> Option<Url> {
    repo.find_default_remote(gix::remote::Direction::Fetch)
        .and_then(Result::ok)
        .and_then(|remote| {
            remote
                .url(gix::remote::Direction::Fetch)
                .map(ToOwned::to_owned)
        })
}
