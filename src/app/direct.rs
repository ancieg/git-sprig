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

//! Direct mode: export the complete tree of a local or remote repository
//! with its submodules, as `git clone --recurse-submodules` would without
//! leaving any `.git` behind, replacing the destination as a whole.

use std::path::Path;

use anyhow::{Context, Result, anyhow};
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

struct DirectSource {
    repository: RootRepository,
    commit: ObjectId,
    base_url: Url,
    fetcher: Option<Fetcher>,
}

pub(super) fn run(
    config: &Config,
    direct: &Direct,
    reporter: &Reporter,
    cancel: &CancelToken,
) -> Result<()> {
    let Direct { from, into } = direct;
    // Patterns are relative to the root here; check them before anything is fetched.
    plan::selection(&config.includes, &config.excludes, None)?;
    if config.checkouts == Checkouts::Refuse {
        protect::refuse_holding_checkouts(into)?;
    }
    let source = open(config, direct, reporter, cancel)?;
    let repo = source.repository.repo();
    let local = repo.to_thread_local();
    let tree_id = local
        .find_commit(source.commit)
        .context("cannot read the resolved commit")?
        .tree_id()
        .context("cannot read the repository tree")?
        .detach();
    report::status!(
        reporter,
        "Revision {} of {} resolved to {}",
        report::quoted(config.direct_rev()),
        report::quoted(from),
        report::quoted(source.commit)
    );

    if config.mode == Mode::DryRun {
        return dry_run(
            config,
            direct,
            &local,
            tree_id,
            source.commit,
            &source.base_url,
            reporter,
        );
    }

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
        &source.base_url,
        snapshot.path(),
        reporter,
    )?;
    let jobs = jobs(
        config.jobs,
        local.config_snapshot().plumbing(),
        Scope::Clone,
    );
    drop(local);

    let fetcher = source
        .fetcher
        .as_ref()
        .ok_or_else(|| anyhow!("temporary storage was not initialized"))?;
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
        report::quoted(source.commit)
    );
    Ok(())
}

fn open(
    config: &Config,
    direct: &Direct,
    reporter: &Reporter,
    cancel: &CancelToken,
) -> Result<DirectSource> {
    let Direct { from, into } = direct;
    let root_url = gix::url::parse(gix::bstr::BStr::new(from)).with_context(|| {
        format!(
            "source repository {} is not a valid git URL or path",
            report::quoted(from)
        )
    })?;
    let remote_url = config
        .remote_url
        .as_ref()
        .map(|url| {
            gix::url::parse(gix::bstr::BStr::new(url.as_str())).with_context(|| {
                format!(
                    "--remote-url {} is not a valid git URL",
                    report::quoted(url)
                )
            })
        })
        .transpose()?;

    let local_source = Path::new(from).exists();
    let fetcher = (config.mode == Mode::Plant || !local_source)
        .then(|| Fetcher::new(config.tempdir.clone(), config.overrides.clone(), cancel))
        .transpose()
        .context("cannot create temporary storage")?;
    let (repository, commit, base_url) = if local_source {
        let repo = repo::discover(Path::new(from), &config.overrides)?;
        if config.checkouts == Checkouts::Refuse {
            protect::refuse_enclosing_destination(into, &repo)?;
        }
        let commit = repo::resolve_commit_id(&repo, config.direct_rev())?;
        let base_url = match remote_url {
            Some(url) => url,
            None => repo::local_url(&repo)?,
        };
        (RootRepository::Local(repo.into_sync()), commit, base_url)
    } else {
        let fetch = fetcher
            .as_ref()
            .ok_or_else(|| anyhow!("temporary storage was not initialized"))?;
        let (repo, commit, strategy) = fetch
            .fetch_revision(&root_url, config.direct_rev(), reporter)
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
        (
            RootRepository::Remote(repo),
            commit,
            remote_url.unwrap_or(root_url),
        )
    };
    Ok(DirectSource {
        repository,
        commit,
        base_url,
        fetcher,
    })
}

fn dry_run(
    config: &Config,
    direct: &Direct,
    repo: &Repository,
    tree_id: ObjectId,
    commit: ObjectId,
    base_url: &Url,
    reporter: &Reporter,
) -> Result<()> {
    let Direct { from, into } = direct;
    let scan = tree::scan_tree(repo, tree_id).context("cannot scan the repository")?;
    let planned = plan_submodules(
        config,
        repo,
        tree_id,
        &scan.gitlinks,
        base_url,
        into,
        reporter,
    )?;
    report::status!(
        reporter,
        "Would materialize repository {} at revision {} ({}) into {}",
        report::quoted(from),
        report::quoted(config.direct_rev()),
        report::quoted(commit),
        report::quoted(into.display())
    );
    report_dry_run(&planned, IndexUpdate::Leave, config.recursion, reporter);
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
    modules.configure(git_config.plumbing(), Scope::Clone);
    let mut selection = plan::selection(&config.includes, &config.excludes, None)?;
    plan::plan(
        gitlinks,
        &modules,
        Some(base_url),
        destination,
        git_config.plumbing(),
        reporter,
        selection.as_mut(),
    )
}
