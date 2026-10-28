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

//! Fetching exactly one commit into a repository that is thrown away again.

use std::{fmt, num::NonZeroU32, path::PathBuf};

use anyhow::{Context, Result, bail};
use gix::{
    ObjectId, Repository, ThreadSafeRepository,
    progress::Discard,
    remote::{
        Direction,
        fetch::{Shallow, Tags},
        ref_map,
    },
};

use crate::{
    cancel::CancelToken,
    report::{self, Reporter},
};

pub struct Fetcher {
    root: tempfile::TempDir,
    /// `key=value` pairs, like `git -c`.
    overrides: Vec<String>,
    cancel: CancelToken,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Strategy {
    Direct,
    AllRefs,
}

impl fmt::Display for Strategy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Strategy::Direct => "directly by object id",
            Strategy::AllRefs => "with all branches and tags",
        })
    }
}

/// Marks failures before the commit is asked for: the fallback connects and
/// negotiates the same way, so it would fail again.
#[derive(Debug)]
struct NoFallback {
    stage: &'static str,
    url: String,
}

impl fmt::Display for NoFallback {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "cannot {} {}", self.stage, report::quoted(&self.url))
    }
}

/// Whether a failed direct fetch is worth retrying as a full one. A [`NoFallback`]
/// failure happened before the commit was asked for, so the full fetch, which
/// connects and negotiates the same way, would fail identically.
fn allows_fallback(err: &anyhow::Error) -> bool {
    err.downcast_ref::<NoFallback>().is_none()
}

impl Fetcher {
    pub fn new(
        tempdir: Option<PathBuf>,
        overrides: Vec<String>,
        cancel: &CancelToken,
    ) -> Result<Self> {
        let mut builder = tempfile::Builder::new();
        builder.prefix("git-sprig-");
        let root = match tempdir {
            Some(dir) => {
                std::fs::create_dir_all(&dir)
                    .with_context(|| format!("cannot create {}", report::quoted(dir.display())))?;
                builder.tempdir_in(dir)
            }
            None => builder.tempdir(),
        }?;
        cancel.register(root.path());
        Ok(Self {
            root,
            overrides,
            cancel: cancel.clone(),
        })
    }

    /// Fetch `commit` from `url` into a fresh bare repository that deletes itself on drop.
    pub fn fetch_commit(
        &self,
        url: &gix::Url,
        commit: ObjectId,
        reporter: &Reporter,
    ) -> Result<(Ephemeral, Strategy)> {
        // Asking for one object needs `uploadpack.allowReachableSHA1InWant` under
        // protocol v1; the fallback covers servers without it.
        let direct = self.create()?;
        let local = direct.repo().to_thread_local();
        match fetch(
            &local,
            url,
            Some(commit),
            Shallow::DepthAtRemote(NonZeroU32::MIN),
            &self.cancel,
        ) {
            Ok(()) if has_commit(&local, commit) => return Ok((direct, Strategy::Direct)),
            Ok(()) => report::warning!(
                reporter,
                "{} did not send {commit} although it accepted the request; \
                 falling back to fetching all branches and tags",
                report::quoted(url)
            ),
            Err(err) if self.cancel.requested() || !allows_fallback(&err) => {
                return Err(err);
            }
            Err(err) => report::warning!(
                reporter,
                "fetching {commit} directly from {} failed ({}); \
                 falling back to fetching all branches and tags",
                report::quoted(url),
                report::causes(&err)
            ),
        }

        // A new repository: the first attempt may have left a shallow boundary behind.
        drop(local);
        drop(direct);
        let full = self.create()?;
        let local = full.repo().to_thread_local();
        fetch(&local, url, None, Shallow::NoChange, &self.cancel)
            .with_context(|| format!("cannot fetch from {}", report::quoted(url)))?;

        if !has_commit(&local, commit) {
            bail!(
                "{} does not provide commit {commit} \
                 (it may have been rewritten or hidden behind a fork)",
                report::quoted(url)
            );
        }
        drop(local);
        Ok((full, Strategy::AllRefs))
    }

    /// Resolve a remote revision, then fetch its commit into a temporary repository.
    pub fn fetch_revision(
        &self,
        url: &gix::Url,
        rev: &str,
        reporter: &Reporter,
    ) -> Result<(Ephemeral, ObjectId, Strategy)> {
        let commit = match ObjectId::from_hex(rev.as_bytes()) {
            Ok(commit) => commit,
            Err(_) => self.resolve_revision(url, rev)?,
        };
        let (repo, strategy) = self.fetch_commit(url, commit, reporter)?;
        Ok((repo, commit, strategy))
    }

    /// Read advertised refs without receiving a pack, peeling annotated tags.
    fn resolve_revision(&self, url: &gix::Url, rev: &str) -> Result<ObjectId> {
        let scratch = self.create()?;
        let local = scratch.repo().to_thread_local();
        let mut remote = local
            .remote_at(url.clone())
            .with_context(|| format!("cannot set up a remote for {}", report::quoted(url)))?
            .with_fetch_tags(Tags::None);
        remote
            .replace_refspecs(
                [
                    "HEAD",
                    "refs/heads/*:refs/remotes/origin/*",
                    "refs/tags/*:refs/tags/*",
                ],
                Direction::Fetch,
            )
            .context("cannot install refspecs")?;
        let prepared = remote
            .connect(Direction::Fetch)
            .with_context(|| NoFallback {
                stage: "connect to",
                url: url.to_string(),
            })?
            .prepare_fetch(Discard, ref_map::Options::default())
            .with_context(|| NoFallback {
                stage: "negotiate with",
                url: url.to_string(),
            })?;

        let candidates = if rev == "HEAD" || rev.starts_with("refs/") {
            vec![rev.to_owned()]
        } else {
            vec![format!("refs/tags/{rev}"), format!("refs/heads/{rev}")]
        };
        let resolved = candidates.iter().find_map(|candidate| {
            prepared.ref_map().remote_refs.iter().find_map(|remote| {
                let (name, target, peeled) = remote.unpack();
                if name == gix::bstr::BStr::new(candidate.as_str()) {
                    peeled.or(target).map(ToOwned::to_owned)
                } else {
                    None
                }
            })
        });
        drop(prepared);
        resolved.ok_or_else(|| {
            anyhow::anyhow!(
                "cannot resolve remote revision {} at {}",
                report::quoted(rev),
                report::quoted(url)
            )
        })
    }

    fn create(&self) -> Result<Ephemeral> {
        let dir = tempfile::Builder::new()
            .prefix("odb-")
            .tempdir_in(self.root.path())
            .context("cannot create a temporary repository")?;
        let repo = ThreadSafeRepository::init_opts(
            dir.path(),
            gix::create::Kind::Bare,
            gix::create::Options::default(),
            gix::open::Options::default().cli_overrides(self.overrides.iter().map(String::as_str)),
        )
        .context("cannot create a temporary repository")?;
        Ok(Ephemeral { repo, _dir: dir })
    }
}

/// Fetch one commit, or all branches and tags if `commit` is `None`.
fn fetch(
    repo: &Repository,
    url: &gix::Url,
    commit: Option<ObjectId>,
    shallow: Shallow,
    cancel: &CancelToken,
) -> Result<()> {
    let spec = commit.map(|id| id.to_string());
    let specs: Vec<&str> = match &spec {
        Some(id) => vec![id.as_str()],
        None => vec![
            "refs/heads/*:refs/remotes/origin/*",
            "refs/tags/*:refs/tags/*",
        ],
    };

    let mut remote = repo
        .remote_at(url.clone())
        .with_context(|| format!("cannot set up a remote for {}", report::quoted(url)))?
        .with_fetch_tags(if commit.is_some() {
            Tags::None
        } else {
            Tags::All
        });
    remote
        .replace_refspecs(specs, Direction::Fetch)
        .context("cannot install refspecs")?;

    let outcome = remote
        .connect(Direction::Fetch)
        .with_context(|| NoFallback {
            stage: "connect to",
            url: url.to_string(),
        })?
        .prepare_fetch(Discard, ref_map::Options::default())
        .with_context(|| NoFallback {
            stage: "negotiate with",
            url: url.to_string(),
        })?
        .with_shallow(shallow)
        .receive(Discard, cancel.flag())
        .with_context(|| format!("cannot receive the pack from {}", report::quoted(url)))?;

    if let gix::remote::fetch::Status::NoPackReceived { .. } = outcome.status {
        bail!("{} sent no pack", report::quoted(url));
    }
    Ok(())
}

fn has_commit(repo: &Repository, commit: ObjectId) -> bool {
    repo.find_commit(commit).is_ok()
}

/// A fetched bare repository that deletes itself when dropped. `repo` comes
/// before `dir` so its packs are unmapped before the directory is removed.
pub struct Ephemeral {
    repo: ThreadSafeRepository,
    _dir: tempfile::TempDir,
}

impl Ephemeral {
    pub fn repo(&self) -> &ThreadSafeRepository {
        &self.repo
    }
}

impl Drop for Fetcher {
    fn drop(&mut self) {
        self.cancel.forget(self.root.path());
    }
}
