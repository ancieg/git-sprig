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

use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use gix::{ObjectId, Repository, ThreadSafeRepository, Url};

use crate::{
    cancel::CancelToken,
    config::{Checkouts, Config, Direct, IndexUpdate, Mode, Source},
    fetch::{Ephemeral, Fetcher},
    fs, index,
    report::{self, Reporter},
    schedule::{self, Recursion},
    snapshot::Snapshot,
    submodule::{
        gitmodules::{Modules, Scope},
        plan::{self, Planned},
    },
    tree,
};

/// Open the repository at or above `at`, with the `-c` settings applied as
/// `git -c` applies them.
fn discover(at: &Path, overrides: &[String]) -> Result<Repository> {
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
fn discover_superproject(overrides: &[String]) -> Result<Repository> {
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
    for warning in &config.warnings {
        report::warning!(reporter, "{warning}");
    }
    // Like `git -C`, so every relative path given is taken from there.
    for dir in &config.chdir {
        std::env::set_current_dir(dir)
            .with_context(|| format!("cannot change to {}", report::quoted(dir.display())))?;
    }
    match &config.source {
        Source::Superproject { work_tree } => {
            run_superproject(config, work_tree.as_deref(), reporter, cancel)
        }
        Source::Direct(direct) => run_direct(config, direct, reporter, cancel),
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
    work_tree: Option<&Path>,
    reporter: &Reporter,
    cancel: &CancelToken,
) -> Result<()> {
    let repo = discover_superproject(&config.overrides)?;
    let commit = resolve_commit_id(&repo, &config.rev)?;
    // As in git, submodule paths start at the top of the work tree, wherever
    // in it git-sprig runs.
    let basedir = match work_tree {
        Some(work_tree) => work_tree.to_owned(),
        None => repo.workdir().map(Path::to_owned).ok_or_else(|| {
            anyhow!(
                "{} is a bare repository, which has no work tree to write submodules into; \
                 pass --work-tree",
                report::quoted(repo.git_dir().display())
            )
        })?,
    };

    // Without a remote, git takes the superproject for its own upstream.
    let (base_url, own_upstream) = match &config.remote_url {
        Some(url) => (
            gix::url::parse(gix::bstr::BStr::new(url.as_str())).with_context(|| {
                format!(
                    "--remote-url {} is not a valid git URL",
                    report::quoted(url)
                )
            })?,
            false,
        ),
        None => match default_remote_url(&repo) {
            Some(url) => (url, false),
            None => (local_url(&repo)?, true),
        },
    };

    let tree_id = repo
        .find_commit(commit)
        .context("cannot read the resolved commit")?
        .tree_id()
        .context("cannot read the superproject tree")?
        .detach();
    let scan = tree::scan_tree(&repo, tree_id).context("cannot scan the superproject")?;
    let mut modules = Modules::from_tree(&repo, tree_id, reporter)
        .context("cannot read .gitmodules of the superproject")?;
    let git_config = repo.config_snapshot();
    modules.configure(git_config.plumbing(), Scope::Repository);
    let mut selection = plan::selection(
        &config.includes,
        &config.excludes,
        prefix(&basedir).as_deref(),
    )?;
    let planned = plan::plan(
        &scan.gitlinks,
        &modules,
        Some(&base_url),
        &basedir,
        git_config.plumbing(),
        reporter,
        selection.as_mut(),
    )?;
    if own_upstream && planned.iter().any(|planned| planned.relative) {
        report::warning!(
            reporter,
            "the superproject has no remote; assuming it is its own authoritative upstream, \
             as git does, and resolving relative submodule URLs against {}",
            report::quoted(&base_url)
        );
    }

    // Both checked before anything is fetched.
    let staged_tree = (config.index == IndexUpdate::Stage)
        .then(|| index::work_tree(&repo, &basedir))
        .transpose()?;
    if config.checkouts == Checkouts::Refuse {
        refuse_checkouts(&planned)?;
    }

    if config.mode == Mode::DryRun {
        report_dry_run(&planned, config.index, config.recursion, reporter);
        return Ok(());
    }

    std::fs::create_dir_all(&basedir)
        .with_context(|| format!("cannot create {}", report::quoted(basedir.display())))?;

    let submodules: Vec<index::Submodule> = if staged_tree.is_some() {
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
        &basedir,
        &schedule_options(config),
        &fetcher,
        reporter,
        cancel,
    )?;

    if let Some(work_tree) = &staged_tree {
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
    direct: &Direct,
    reporter: &Reporter,
    cancel: &CancelToken,
) -> Result<()> {
    let Direct { from, into } = direct;
    // Patterns are relative to the root here; check them before anything is fetched.
    plan::selection(&config.includes, &config.excludes, None)?;
    // The whole destination goes, so a checkout anywhere inside it would too.
    if config.checkouts == Checkouts::Refuse
        && let Some(checkout) = fs::find_checkout(into)?
    {
        if checkout == *into {
            bail!(
                "refusing to replace {}, which is a git checkout with a .git of its own\n\
                 Replacing it discards any uncommitted or unpushed work; pass --force to do it \
                 anyway.",
                report::quoted(into.display())
            );
        }
        bail!(
            "refusing to replace {}, which holds the git checkout {}\n\
             Replacing it discards any uncommitted or unpushed work there; pass --force to do \
             it anyway.",
            report::quoted(into.display()),
            report::quoted(checkout.display())
        );
    }
    let source = open_direct(config, direct, reporter, cancel)?;
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
    let planned = plan_direct(
        config,
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
        report::quoted(into.display()),
        report::quoted(source.commit)
    );
    Ok(())
}

fn open_direct(
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
        let repo = discover(Path::new(from), &config.overrides)?;
        if config.checkouts == Checkouts::Refuse {
            refuse_enclosing_destination(into, &repo)?;
        }
        let commit = resolve_commit_id(&repo, &config.rev)?;
        let base_url = match remote_url {
            Some(url) => url,
            None => local_url(&repo)?,
        };
        (RootRepository::Local(repo.into_sync()), commit, base_url)
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

/// The absolute path of a local repository, for relative submodule URLs to be
/// resolved against: the path `git clone` records for a local source, and the
/// one git falls back to for a superproject without a remote. It does not
/// depend on how the path was spelled or on which subdirectory it named. `.`
/// and `..` are folded first, since resolving takes every component for a
/// directory.
fn local_url(repo: &Repository) -> Result<Url> {
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

/// Replacing a destination that holds the local source deletes the source
/// too, along with whatever work is in it. A linked worktree needs its
/// common directory as well, which may live elsewhere.
fn refuse_enclosing_destination(destination: &Path, repo: &Repository) -> Result<()> {
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

fn dry_run_direct(
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
    let planned = plan_direct(
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
        report::quoted(&config.rev),
        report::quoted(commit),
        report::quoted(into.display())
    );
    report_dry_run(&planned, IndexUpdate::Leave, config.recursion, reporter);
    Ok(())
}

fn plan_direct(
    config: &Config,
    repo: &Repository,
    tree_id: ObjectId,
    gitlinks: &[tree::Gitlink],
    base_url: &Url,
    destination: &Path,
    reporter: &Reporter,
) -> Result<Vec<Planned>> {
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

/// Where git-sprig runs, relative to the top of the work tree, which git reads
/// pathspecs against; `None` at the top or outside of the work tree.
fn prefix(top: &Path) -> Option<PathBuf> {
    let cwd = std::env::current_dir().ok()?.canonicalize().ok()?;
    let prefix = cwd.strip_prefix(top.canonicalize().ok()?).ok()?;
    (!prefix.as_os_str().is_empty()).then(|| prefix.to_owned())
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
