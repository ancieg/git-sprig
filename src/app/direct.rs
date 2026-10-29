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

//! Direct mode: export the complete tree of a local or remote repository
//! with its submodules, as `git clone --recurse-submodules` would without
//! leaving any `.git` behind, replacing the destination as a whole.

use std::path::Path;

use anyhow::{Context, Result};
use gix::{ObjectId, Repository, ThreadSafeRepository, Url};

use super::{available_cpus, jobs, report_dry_run, schedule_options};
use crate::{
    cancel::CancelToken,
    config::{Checkouts, Config, Direct, IndexUpdate, Mode, Submodules},
    fetch::{Ephemeral, Fetcher},
    protect, repo,
    report::{self, Reporter},
    schedule,
    snapshot::Snapshot,
    submodule::{
        gitmodules::{Modules, Scope},
        plan::{self, Planned},
    },
    tree,
};

enum RootRepository {
    Local(ThreadSafeRepository),
    Remote(Ephemeral),
}

impl RootRepository {
    fn repo(&self) -> &ThreadSafeRepository {
        match self {
            RootRepository::Local(repo) => repo,
            RootRepository::Remote(repo) => repo.repo(),
        }
    }
}

/// The repository exported, at the commit asked for.
struct Root {
    repository: RootRepository,
    commit: ObjectId,
    /// Where relative submodule URLs start.
    base_url: Url,
}

pub(super) fn run(
    config: &Config,
    direct: &Direct,
    reporter: &Reporter,
    cancel: &CancelToken,
) -> Result<()> {
    // Patterns are relative to the root here; check them before anything is fetched.
    plan::selection(&config.includes, &config.excludes, None)?;
    if config.checkouts == Checkouts::Refuse {
        protect::refuse_holding_checkouts(&direct.into)?;
    }
    if Path::new(&direct.from).exists() {
        // Read where it is; only its submodules need temporary storage.
        let root = open_local(config, direct, reporter)?;
        match config.mode {
            Mode::DryRun => dry_run(config, direct, &root, reporter),
            Mode::Plant => {
                let fetcher = new_fetcher(config, cancel)?;
                plant(config, direct, &root, &fetcher, reporter, cancel)
            }
        }
    } else {
        // Fetched even for a dry run, to see what it holds. The fetcher is
        // dropped last, as the fetched root lives in its storage.
        let fetcher = new_fetcher(config, cancel)?;
        let root = open_remote(config, direct, &fetcher, reporter)?;
        match config.mode {
            Mode::DryRun => dry_run(config, direct, &root, reporter),
            Mode::Plant => plant(config, direct, &root, &fetcher, reporter, cancel),
        }
    }
}

fn new_fetcher(config: &Config, cancel: &CancelToken) -> Result<Fetcher> {
    Fetcher::new(config.tempdir.clone(), config.overrides.clone(), cancel)
        .context("cannot create temporary storage")
}

fn open_local(config: &Config, direct: &Direct, reporter: &Reporter) -> Result<Root> {
    let repo = repo::discover(Path::new(&direct.from), &config.overrides)?;
    if config.checkouts == Checkouts::Refuse {
        protect::refuse_enclosing_destination(&direct.into, &repo)?;
    }
    let commit = repo::resolve_commit_id(&repo, config.direct_rev())?;
    let base_url = match &config.remote_url {
        Some(url) => url.clone(),
        None => repo::local_url(&repo)?,
    };
    resolved(config, direct, commit, reporter);
    Ok(Root {
        repository: RootRepository::Local(repo.into_sync()),
        commit,
        base_url,
    })
}

fn open_remote(
    config: &Config,
    direct: &Direct,
    fetcher: &Fetcher,
    reporter: &Reporter,
) -> Result<Root> {
    let from = &direct.from;
    let url = gix::url::parse(gix::bstr::BStr::new(from)).with_context(|| {
        format!(
            "source repository {} is not a valid git URL or path",
            report::quoted(from)
        )
    })?;
    let (repo, commit, strategy) = fetcher
        .fetch_revision(&url, config.direct_rev(), reporter)
        .with_context(|| {
            format!(
                "cannot fetch revision {} from {}",
                report::quoted(config.direct_rev()),
                report::quoted(from)
            )
        })?;
    report::detail!(
        reporter,
        "Repository {}: fetched {strategy}",
        report::quoted(from)
    );
    resolved(config, direct, commit, reporter);
    Ok(Root {
        repository: RootRepository::Remote(repo),
        commit,
        base_url: config.remote_url.clone().unwrap_or(url),
    })
}

fn resolved(config: &Config, direct: &Direct, commit: ObjectId, reporter: &Reporter) {
    report::status!(
        reporter,
        "Revision {} of {} resolved to {}",
        report::quoted(config.direct_rev()),
        report::quoted(&direct.from),
        report::quoted(commit)
    );
}

fn tree_of(repo: &Repository, commit: ObjectId) -> Result<ObjectId> {
    Ok(repo
        .find_commit(commit)
        .context("cannot read the resolved commit")?
        .tree_id()
        .context("cannot read the repository tree")?
        .detach())
}

fn dry_run(config: &Config, direct: &Direct, root: &Root, reporter: &Reporter) -> Result<()> {
    let Direct { from, into } = direct;
    let repo = root.repository.repo().to_thread_local();
    let tree_id = tree_of(&repo, root.commit)?;
    let scan = tree::scan_tree(&repo, tree_id).context("cannot scan the repository")?;
    let planned = plan_submodules(
        config,
        &repo,
        tree_id,
        &scan.gitlinks,
        &root.base_url,
        into,
        reporter,
    )?;
    report::status!(
        reporter,
        "Would materialize repository {} at revision {} ({}) into {}",
        report::quoted(from),
        report::quoted(config.direct_rev()),
        report::quoted(root.commit),
        report::quoted(into.display())
    );
    report_dry_run(&planned, IndexUpdate::Leave, config.recursion, reporter);
    Ok(())
}

/// Write the root's tree and its submodules into a snapshot, and swap it in
/// for the destination once all of it is done.
fn plant(
    config: &Config,
    direct: &Direct,
    root: &Root,
    fetcher: &Fetcher,
    reporter: &Reporter,
    cancel: &CancelToken,
) -> Result<()> {
    let Direct { from, into } = direct;
    let repo = root.repository.repo();
    let local = repo.to_thread_local();
    let tree_id = tree_of(&local, root.commit)?;
    let snapshot = Snapshot::new(into, cancel)?;
    let extracted = tree::extract_tree(
        repo,
        tree_id,
        snapshot.path(),
        config.conversion,
        available_cpus().min(8),
        cancel,
    )
    .with_context(|| {
        format!(
            "cannot write the snapshot to {}",
            report::quoted(into.display())
        )
    })?;
    report::detail!(
        reporter,
        "Repository {}: wrote {}, {} streamed",
        report::quoted(from),
        report::plural(extracted.scan.files, "file"),
        extracted.streamed
    );
    let planned = plan_submodules(
        config,
        &local,
        tree_id,
        &extracted.scan.gitlinks,
        &root.base_url,
        snapshot.path(),
        reporter,
    )?;
    let jobs = jobs(
        config.jobs,
        local.config_snapshot().plumbing(),
        Scope::Clone,
    );
    drop(local);

    schedule::run(
        planned,
        snapshot.path(),
        &schedule_options(config, jobs),
        fetcher,
        reporter,
        cancel,
    )?;
    snapshot.commit(reporter)?;
    report::status!(
        reporter,
        "Repository path {}: planted {}",
        report::quoted(into.display()),
        report::quoted(root.commit)
    );
    Ok(())
}

fn plan_submodules(
    config: &Config,
    repo: &Repository,
    tree_id: ObjectId,
    gitlinks: &[tree::Gitlink],
    base_url: &Url,
    destination: &Path,
    reporter: &Reporter,
) -> Result<Vec<Planned>> {
    if config.submodules == Submodules::Skip {
        return Ok(Vec::new());
    }
    let mut modules = Modules::from_tree(repo, tree_id, reporter)
        .context("cannot read .gitmodules of the repository")?;
    let git_config = repo.config_snapshot();
    modules.configure(git_config.plumbing(), Scope::Clone)?;
    let mut selection = plan::selection(&config.includes, &config.excludes, None)?;
    plan::plan(
        gitlinks,
        &modules,
        Some(base_url),
        destination,
        reporter,
        selection.as_mut(),
    )
}
