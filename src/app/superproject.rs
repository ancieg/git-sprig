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

//! Submodule-only mode: fill in the submodules of a local superproject, as
//! `git submodule update --init --recursive` would, leaving its own files
//! alone unless `--stage` asks to commit them as plain files.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use gix::{Repository, Url};

use super::{jobs, report_dry_run, schedule_options};
use crate::{
    cancel::CancelToken,
    config::{Checkouts, Config, IndexUpdate, Mode, Superproject},
    fetch::Fetcher,
    protect, repo,
    report::{self, Reporter},
    schedule, stage,
    submodule::{
        gitmodules::{Modules, Scope},
        plan::{self, Planned},
    },
    tree,
};

pub(super) fn run(
    config: &Config,
    superproject: &Superproject,
    reporter: &Reporter,
    cancel: &CancelToken,
) -> Result<()> {
    let repo = repo::discover_superproject(&config.overrides)?;
    let work_tree = find_work_tree(&repo, superproject.work_tree.as_deref())?;
    let planned = plan_submodules(config, &repo, &work_tree, reporter)?;

    // Both checked before anything is fetched.
    let staged_tree = (config.index == IndexUpdate::Stage)
        .then(|| stage::work_tree(&repo, &work_tree))
        .transpose()?;
    if config.checkouts == Checkouts::Refuse {
        protect::refuse_checkouts(&planned)?;
    }

    if config.mode == Mode::DryRun {
        report_dry_run(&planned, config.index, config.recursion, reporter);
        return Ok(());
    }

    std::fs::create_dir_all(&work_tree)
        .with_context(|| format!("cannot create {}", report::quoted(work_tree.display())))?;

    let submodules: Vec<stage::Submodule> = if staged_tree.is_some() {
        planned
            .iter()
            .map(|planned| stage::Submodule {
                name: planned.name.clone(),
                path: planned.path.clone(),
            })
            .collect()
    } else {
        Vec::new()
    };

    let jobs = jobs(
        config.jobs,
        repo.config_snapshot().plumbing(),
        Scope::Repository,
    );
    let fetcher = Fetcher::new(config.tempdir.clone(), config.overrides.clone(), cancel)
        .context("cannot create temporary storage")?;
    schedule::run(
        planned,
        &work_tree,
        &schedule_options(config, jobs),
        &fetcher,
        reporter,
        cancel,
    )?;

    if let Some(canonical) = &staged_tree {
        stage::replace_gitlinks(canonical, &submodules, reporter)?;
    }
    Ok(())
}

/// As in git, submodule paths start at the top of the work tree, wherever in
/// it git-sprig runs.
fn find_work_tree(repo: &Repository, given: Option<&Path>) -> Result<PathBuf> {
    match given {
        Some(work_tree) => Ok(work_tree.to_owned()),
        None => repo.workdir().map(Path::to_owned).ok_or_else(|| {
            anyhow!(
                "{} is a bare repository, which has no work tree to write submodules into; \
                 pass --work-tree",
                report::quoted(repo.git_dir().display())
            )
        }),
    }
}

/// Pair the pinned gitlinks that the pathspecs select with their settings.
fn plan_submodules(
    config: &Config,
    repo: &Repository,
    work_tree: &Path,
    reporter: &Reporter,
) -> Result<Vec<Planned>> {
    let (base_url, own_upstream) = base_url(config, repo)?;
    let (gitlinks, mut modules) = match &config.rev {
        Some(rev) => pinned_in_commit(repo, rev, reporter)?,
        None => pinned_in_index(repo, reporter)?,
    };
    let git_config = repo.config_snapshot();
    modules.configure(git_config.plumbing(), Scope::Repository);
    let mut selection = plan::selection(
        &config.includes,
        &config.excludes,
        prefix(work_tree).as_deref(),
    )?;
    let planned = plan::plan(
        &gitlinks,
        &modules,
        Some(&base_url),
        work_tree,
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
    Ok(planned)
}

/// Where relative submodule URLs start, and whether it is the superproject
/// itself: without a remote, git takes the superproject for its own upstream.
fn base_url(config: &Config, repo: &Repository) -> Result<(Url, bool)> {
    Ok(match &config.remote_url {
        Some(url) => (url.clone(), false),
        None => match repo::default_remote_url(repo) {
            Some(url) => (url, false),
            None => (repo::local_url(repo)?, true),
        },
    })
}

/// The gitlinks of `rev` and the `.gitmodules` committed alongside them.
fn pinned_in_commit(
    repo: &Repository,
    rev: &str,
    reporter: &Reporter,
) -> Result<(Vec<tree::Gitlink>, Modules)> {
    let tree_id = repo
        .find_commit(repo::resolve_commit_id(repo, rev)?)
        .context("cannot read the resolved commit")?
        .tree_id()
        .context("cannot read the superproject tree")?
        .detach();
    let scan = tree::scan_tree(repo, tree_id).context("cannot scan the superproject")?;
    let modules = Modules::from_tree(repo, tree_id, reporter)
        .context("cannot read .gitmodules of the superproject")?;
    Ok((scan.gitlinks, modules))
}

/// The gitlinks staged in the index, as `git submodule update` reads them,
/// with `.gitmodules` from the work tree, or as staged if it has none there.
fn pinned_in_index(
    repo: &Repository,
    reporter: &Reporter,
) -> Result<(Vec<tree::Gitlink>, Modules)> {
    let index = repo
        .index_or_load_from_head_or_empty()
        .context("cannot read the index of the superproject")?;
    let gitlinks = tree::staged_gitlinks(&index, reporter)?;

    let in_work_tree = match repo.workdir().map(|dir| dir.join(".gitmodules")) {
        Some(file) => match std::fs::read(&file) {
            Ok(bytes) => Some(bytes),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
            Err(err) => {
                return Err(err)
                    .with_context(|| format!("cannot read {}", report::quoted(file.display())));
            }
        },
        None => None,
    };
    let bytes = match in_work_tree {
        Some(bytes) => bytes,
        None => match index.entry_by_path(".gitmodules".into()) {
            Some(entry) => repo
                .find_blob(entry.id)
                .context("missing .gitmodules blob")?
                .take_data(),
            None => return Ok((gitlinks, Modules::default())),
        },
    };
    let modules = Modules::from_bytes(&bytes, reporter)
        .context("cannot read .gitmodules of the superproject")?;
    Ok((gitlinks, modules))
}

/// Where git-sprig runs, relative to the top of the work tree, which git reads
/// pathspecs against; `None` at the top or outside of the work tree.
fn prefix(top: &Path) -> Option<PathBuf> {
    let cwd = std::env::current_dir().ok()?.canonicalize().ok()?;
    let prefix = cwd.strip_prefix(top.canonicalize().ok()?).ok()?;
    (!prefix.as_os_str().is_empty()).then(|| prefix.to_owned())
}
