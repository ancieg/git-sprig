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
use gix::{ObjectId, Repository, ThreadSafeRepository, Url};

use crate::{
    cancel::CancelToken,
    config::{Checkouts, Config, IndexUpdate, Mode, Source},
    fetch::{Ephemeral, Fetcher},
    fs, index,
    report::{self, Reporter},
    schedule::{self, Recursion},
    snapshot::Snapshot,
    submodule::{
        gitmodules::Modules,
        plan::{self, Planned},
    },
    tree,
};

/// git's own wording for the two ordinary cases; gix states them again in its
/// own words, naming the path a second time.
fn discover(at: &Path) -> Result<Repository> {
    map_discover_error(at, gix::discover(at))
}

fn discover_with_overrides(at: &Path, overrides: &[String]) -> Result<Repository> {
    map_discover_error(
        at,
        gix::discover_opts(
            at,
            gix::discover::upwards::Options::default(),
            gix::open::Options::default().cli_overrides(overrides.iter().map(String::as_str)),
        ),
    )
}

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

pub(crate) fn run(config: &Config, reporter: &Reporter, cancel: &CancelToken) -> Result<()> {
    cancel.install_handler()?;
    match &config.source {
        Source::Superproject { repo } => run_superproject(config, repo, reporter, cancel),
        Source::Direct { from } => run_direct(config, from, reporter, cancel),
    }
}

fn resolve_commit_id(repo: &Repository, rev: &str) -> Result<ObjectId> {
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

fn run_superproject(
    config: &Config,
    repo_path: &Path,
    reporter: &Reporter,
    cancel: &CancelToken,
) -> Result<()> {
    let repo = discover(repo_path)?;
    let commit = resolve_commit_id(&repo, &config.rev)?;

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

    let tree_id = repo
        .find_commit(commit)
        .context("cannot read the resolved commit")?
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

    let fetcher = Fetcher::new(config.tempdir.clone(), config.overrides.clone(), cancel)
        .context("cannot create temporary storage")?;
    schedule::run(
        planned,
        &config.basedir,
        &schedule_options(config),
        &fetcher,
        reporter,
        cancel,
    )?;

    if let Some(work_tree) = &work_tree {
        index::stage(work_tree, &submodules, reporter)?;
    }
    Ok(())
}

struct DirectSource {
    repository: RootRepository,
    commit: ObjectId,
    base_url: Url,
    fetcher: Option<Fetcher>,
}

fn run_direct(
    config: &Config,
    from: &str,
    reporter: &Reporter,
    cancel: &CancelToken,
) -> Result<()> {
    if config.checkouts == Checkouts::Refuse && fs::is_checkout(&config.basedir) {
        bail!(
            "refusing to replace {}, which is a git checkout with a .git of its own\n\
             Replacing it discards any uncommitted or unpushed work; pass --force to do it anyway.",
            report::quoted(config.basedir.display())
        );
    }
    let source = open_direct(config, from, reporter, cancel)?;
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
        report::quoted(&config.rev),
        report::quoted(from),
        report::quoted(source.commit)
    );

    if config.mode == Mode::DryRun {
        return dry_run_direct(
            config,
            from,
            &local,
            tree_id,
            source.commit,
            &source.base_url,
            reporter,
        );
    }

    let snapshot = Snapshot::new(&config.basedir, cancel)?;
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
            report::quoted(config.basedir.display())
        )
    })?;
    report::detail!(
        reporter,
        "Repository {}: wrote {}, {} streamed",
        report::quoted(from),
        report::plural(extracted.scan.files, "file"),
        extracted.streamed
    );
    let planned = plan_direct(
        &local,
        tree_id,
        &extracted.scan.gitlinks,
        &source.base_url,
        snapshot.path(),
        reporter,
    )?;
    drop(local);

    let fetcher = source
        .fetcher
        .as_ref()
        .ok_or_else(|| anyhow!("temporary storage was not initialized"))?;
    schedule::run(
        planned,
        snapshot.path(),
        &schedule_options(config),
        fetcher,
        reporter,
        cancel,
    )?;
    snapshot.commit(reporter)?;
    report::status!(
        reporter,
        "Repository path {}: planted {}",
        report::quoted(config.basedir.display()),
        report::quoted(source.commit)
    );
    Ok(())
}

fn open_direct(
    config: &Config,
    from: &str,
    reporter: &Reporter,
    cancel: &CancelToken,
) -> Result<DirectSource> {
    let root_url = gix::url::parse(gix::bstr::BStr::new(from)).with_context(|| {
        format!(
            "--from {} is not a valid git URL or path",
            report::quoted(from)
        )
    })?;
    let base_url = match &config.remote_url {
        Some(url) => gix::url::parse(gix::bstr::BStr::new(url.as_str())).with_context(|| {
            format!(
                "--remote-url {} is not a valid git URL",
                report::quoted(url)
            )
        })?,
        None => root_url.clone(),
    };

    let local_source = Path::new(from).exists();
    let fetcher = (config.mode == Mode::Plant || !local_source)
        .then(|| Fetcher::new(config.tempdir.clone(), config.overrides.clone(), cancel))
        .transpose()
        .context("cannot create temporary storage")?;
    let (repository, commit) = if local_source {
        let repo = discover_with_overrides(Path::new(from), &config.overrides)?;
        let commit = resolve_commit_id(&repo, &config.rev)?;
        (RootRepository::Local(repo.into_sync()), commit)
    } else {
        let fetch = fetcher
            .as_ref()
            .ok_or_else(|| anyhow!("temporary storage was not initialized"))?;
        let (repo, commit, strategy) = fetch
            .fetch_revision(&root_url, &config.rev, reporter)
            .with_context(|| {
                format!(
                    "cannot fetch revision {} from {}",
                    report::quoted(&config.rev),
                    report::quoted(from)
                )
            })?;
        report::detail!(
            reporter,
            "Repository {}: fetched {strategy}",
            report::quoted(from)
        );
        (RootRepository::Remote(repo), commit)
    };
    Ok(DirectSource {
        repository,
        commit,
        base_url,
        fetcher,
    })
}

fn dry_run_direct(
    config: &Config,
    from: &str,
    repo: &Repository,
    tree_id: ObjectId,
    commit: ObjectId,
    base_url: &Url,
    reporter: &Reporter,
) -> Result<()> {
    let scan = tree::scan_tree(repo, tree_id).context("cannot scan the repository")?;
    let planned = plan_direct(
        repo,
        tree_id,
        &scan.gitlinks,
        base_url,
        &config.basedir,
        reporter,
    )?;
    report::status!(
        reporter,
        "Would materialize repository {} at revision {} ({}) into {}",
        report::quoted(from),
        report::quoted(&config.rev),
        report::quoted(commit),
        report::quoted(config.basedir.display())
    );
    report_dry_run(&planned, IndexUpdate::Leave, config.recursion, reporter);
    Ok(())
}

fn plan_direct(
    repo: &Repository,
    tree_id: ObjectId,
    gitlinks: &[tree::Gitlink],
    base_url: &Url,
    destination: &Path,
    reporter: &Reporter,
) -> Result<Vec<Planned>> {
    let modules = Modules::from_tree(repo, tree_id, reporter)
        .context("cannot read .gitmodules of the repository")?;
    let git_config = repo.config_snapshot();
    plan::plan(
        gitlinks,
        &modules,
        Some(base_url),
        destination,
        git_config.plumbing(),
        reporter,
    )
}

fn schedule_options(config: &Config) -> schedule::Options {
    // Past eight threads, writing gets no faster.
    schedule::Options {
        conversion: config.conversion,
        recursion: config.recursion,
        jobs: config.jobs,
        write_threads: available_cpus().min(8),
    }
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
