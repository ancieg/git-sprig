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

//! Running the plan with `--jobs` worker threads.
//!
//! One thread — the coordinator, running [`run`] — owns all the bookkeeping:
//! the queue, the counters, the per-group snapshots and the fetch cache. It
//! spawns a scoped worker thread per job, up to `--jobs` at once, and each
//! worker reports back over a channel. Nothing shared needs a lock.
//!
//! A top-level submodule and everything nested in it are written into one
//! snapshot directory, which replaces the destination only once all of it is done.

use std::{
    collections::HashMap,
    num::NonZeroUsize,
    path::{Path, PathBuf},
    sync::{Arc, mpsc},
    thread,
    time::Instant,
};

use anyhow::{Context, Result};
use gix::ObjectId;

use crate::{
    cancel::CancelToken,
    error,
    fetch::{Ephemeral, Fetcher},
    fs,
    report::{self, Reporter},
    snapshot::Snapshot,
    submodule::{
        gitmodules::Modules,
        plan::{self, Planned},
    },
    tree::{self, Conversion},
};

/// How far to descend into submodules of submodules.
#[derive(Debug, Clone, Copy)]
pub enum Recursion {
    /// Only the top-level submodules.
    Off,
    /// Down to this many levels.
    Limited(NonZeroUsize),
    Unlimited,
}

pub struct Options {
    pub conversion: Conversion,
    pub recursion: Recursion,
    pub jobs: NonZeroUsize,
    /// Threads writing the files of one submodule.
    pub write_threads: usize,
}

/// Materialize `planned` and everything nested in it.
pub fn run(
    planned: Vec<Planned>,
    basedir: &Path,
    options: &Options,
    fetcher: &Fetcher,
    reporter: &Reporter,
    cancel: &CancelToken,
) -> Result<()> {
    let mut coordinator = Coordinator::new(basedir, reporter, cancel, planned);

    thread::scope(|scope| {
        let (tx, rx) = mpsc::channel::<Done>();
        let mut inflight = 0usize;
        loop {
            while inflight < options.jobs.get() && !coordinator.stopping() {
                let Some((job, cached)) = coordinator.next_dispatch() else {
                    break;
                };
                coordinator.announce(&job);
                inflight += 1;
                let tx = tx.clone();
                scope.spawn(move || {
                    let result = run_job(&job, basedir, options, fetcher, cached, reporter, cancel);
                    // The only receiver lives until every worker has reported.
                    tx.send(Done { job, result }).ok();
                });
            }
            if inflight == 0 {
                break;
            }
            // Errs only once every worker has dropped its sender, which cannot
            // happen while `inflight > 0`; break defensively rather than unwrap.
            let Ok(done) = rx.recv() else { break };
            inflight -= 1;
            coordinator.handle(done);
        }
    });

    if let Some(err) = coordinator.error {
        return Err(err);
    }
    if coordinator.outstanding > 0 {
        return Err(error::Interrupted.into());
    }
    Ok(())
}

fn shown_path<'a>(basedir: &Path, destination: &'a Path) -> &'a Path {
    destination.strip_prefix(basedir).unwrap_or(destination)
}

/// Submodules pinning the same commit of the same URL share one fetch.
#[derive(Clone, PartialEq, Eq, Hash)]
struct FetchKey {
    url: String,
    commit: ObjectId,
}

impl FetchKey {
    fn of(planned: &Planned) -> Self {
        Self {
            url: planned.url.to_bstring().to_string(),
            commit: planned.commit,
        }
    }
}

struct Job {
    planned: Planned,
    depth: usize,
    place: Place,
    /// Index into the coordinator's `groups`.
    group: usize,
}

enum Place {
    TopLevel,
    /// The mount point at `planned.path` below `parent`.
    Nested {
        parent: PathBuf,
    },
}

/// A top-level submodule with everything nested in it, replaced as a whole.
struct Group {
    snapshot: Option<Snapshot>,
    /// Jobs of the group not yet finished; the snapshot goes in place at zero.
    pending: usize,
    /// Submodules of the group already written, reported once it is in place.
    planted: Vec<(PathBuf, ObjectId)>,
}

/// The shared fetch for one [`FetchKey`], and how many jobs still need it.
struct Slot {
    users: usize,
    state: SlotState,
}

enum SlotState {
    /// Claimed, but no job has started fetching it yet.
    Pending,
    /// A job is fetching it now; jobs that need it wait rather than fetch again.
    Fetching,
    Ready(Arc<Ephemeral>),
}

/// What a worker did with one job.
struct Produced {
    /// `Some` for a top-level job, which builds the group's snapshot.
    snapshot: Option<Snapshot>,
    /// Where the files were written, which nested submodules hang off.
    target: PathBuf,
    fetched: Fetched,
    children: Vec<Planned>,
}

enum Fetched {
    /// Freshly fetched; the repository is handed back to share with other jobs.
    Fresh(Arc<Ephemeral>),
    Reused,
}

struct Done {
    job: Job,
    result: Result<Produced>,
}

struct Coordinator<'a> {
    basedir: &'a Path,
    reporter: &'a Reporter,
    cancel: &'a CancelToken,
    /// Ready to run: the fetch is either unclaimed or already cached.
    queue: Vec<Job>,
    /// Parked because another job is fetching the same commit.
    waiting: HashMap<FetchKey, Vec<Job>>,
    fetches: HashMap<FetchKey, Slot>,
    groups: Vec<Group>,
    /// Jobs queued, waiting or running.
    outstanding: usize,
    /// Total found so far, for the `(n/total)` counter; grows as nesting appears.
    discovered: usize,
    /// How many jobs have started, for the counter's `n`.
    started: usize,
    /// The first failure; later ones are usually fetches it cut short.
    error: Option<anyhow::Error>,
}

impl<'a> Coordinator<'a> {
    fn new(
        basedir: &'a Path,
        reporter: &'a Reporter,
        cancel: &'a CancelToken,
        planned: Vec<Planned>,
    ) -> Self {
        let mut coordinator = Coordinator {
            basedir,
            reporter,
            cancel,
            queue: Vec::new(),
            waiting: HashMap::new(),
            fetches: HashMap::new(),
            groups: Vec::new(),
            outstanding: 0,
            discovered: planned.len(),
            started: 0,
            error: None,
        };
        // A group per top-level submodule; children join their parent's group.
        // Reversed so the queue, popped from the end, keeps the tree's order.
        for planned in planned.into_iter().rev() {
            let group = coordinator.groups.len();
            coordinator.groups.push(Group {
                snapshot: None,
                pending: 1,
                planted: Vec::new(),
            });
            coordinator.enqueue(Job {
                planned,
                depth: 0,
                place: Place::TopLevel,
                group,
            });
        }
        coordinator
    }

    /// After a failure or Ctrl-C, stop dispatching and let running jobs drain.
    fn stopping(&self) -> bool {
        self.error.is_some() || self.cancel.requested()
    }

    fn enqueue(&mut self, job: Job) {
        self.fetches
            .entry(FetchKey::of(&job.planned))
            .or_insert(Slot {
                users: 0,
                state: SlotState::Pending,
            })
            .users += 1;
        self.outstanding += 1;
        self.queue.push(job);
    }

    /// The next job to run, with a shared repository if one is already fetched.
    /// Jobs whose commit is mid-fetch are parked and skipped, holding no slot.
    fn next_dispatch(&mut self) -> Option<(Job, Option<Arc<Ephemeral>>)> {
        enum Act {
            Fetch,
            Wait,
            Reuse(Arc<Ephemeral>),
        }
        while let Some(job) = self.queue.pop() {
            let key = FetchKey::of(&job.planned);
            let act = match self.fetches.get_mut(&key) {
                Some(slot) => match &slot.state {
                    SlotState::Pending => {
                        slot.state = SlotState::Fetching;
                        Act::Fetch
                    }
                    SlotState::Fetching => Act::Wait,
                    SlotState::Ready(repo) => Act::Reuse(Arc::clone(repo)),
                },
                // Every enqueued job claims a slot, so this arm is unreachable.
                None => Act::Fetch,
            };
            match act {
                Act::Fetch => return Some((job, None)),
                Act::Reuse(repo) => return Some((job, Some(repo))),
                Act::Wait => self.waiting.entry(key).or_default().push(job),
            }
        }
        None
    }

    fn announce(&mut self, job: &Job) {
        self.started += 1;
        report::status!(
            self.reporter,
            "Fetching submodule {} from {} {}",
            report::quoted(shown_path(self.basedir, &job.planned.destination).display()),
            report::quoted(&job.planned.url),
            report::Counter {
                number: self.started,
                total: self.discovered,
            }
        );
    }

    fn handle(&mut self, done: Done) {
        let Done { job, result } = done;
        self.outstanding -= 1;
        let key = FetchKey::of(&job.planned);

        match result {
            Ok(produced) => self.settle(&job, produced),
            Err(err) => {
                self.error.get_or_insert(err);
                self.cancel.request();
            }
        }

        if let Some(slot) = self.fetches.get_mut(&key) {
            slot.users -= 1;
            if slot.users == 0 {
                // Dropping the last handle removes the temporary repository.
                self.fetches.remove(&key);
            }
        }
    }

    fn settle(&mut self, job: &Job, produced: Produced) {
        let key = FetchKey::of(&job.planned);
        if let Fetched::Fresh(repo) = produced.fetched {
            if let Some(slot) = self.fetches.get_mut(&key) {
                slot.state = SlotState::Ready(repo);
            }
            // Jobs that were waiting for this commit can now reuse it.
            for job in self.waiting.remove(&key).unwrap_or_default() {
                self.queue.push(job);
            }
        }

        let group = job.group;
        let shown = shown_path(self.basedir, &job.planned.destination).to_owned();
        self.groups[group].planted.push((shown, job.planned.commit));
        if produced.snapshot.is_some() {
            self.groups[group].snapshot = produced.snapshot;
        }

        let children = produced.children.len();
        self.discovered += children;
        self.groups[group].pending += children;
        for child in produced.children {
            self.enqueue(Job {
                planned: child,
                depth: job.depth + 1,
                place: Place::Nested {
                    parent: produced.target.clone(),
                },
                group,
            });
        }

        self.groups[group].pending -= 1;
        if self.groups[group].pending == 0 {
            self.put_in_place(group);
        }
    }

    fn put_in_place(&mut self, group: usize) {
        let Some(snapshot) = self.groups[group].snapshot.take() else {
            return;
        };
        if let Err(err) = snapshot.commit(self.reporter) {
            self.error.get_or_insert(err);
            self.cancel.request();
            return;
        }
        for (path, commit) in std::mem::take(&mut self.groups[group].planted) {
            report::status!(
                self.reporter,
                "Submodule path {}: planted {}",
                report::quoted(path.display()),
                report::quoted(commit)
            );
        }
    }
}

/// Fetch and write one submodule, and read the submodules nested in it. Runs on
/// a worker thread; the coordinator owns everything shared.
fn run_job(
    job: &Job,
    basedir: &Path,
    options: &Options,
    fetcher: &Fetcher,
    cached: Option<Arc<Ephemeral>>,
    reporter: &Reporter,
    cancel: &CancelToken,
) -> Result<Produced> {
    let Planned {
        url,
        commit,
        path,
        destination,
        ..
    } = &job.planned;
    let shown = shown_path(basedir, destination);
    let cannot_write = || {
        format!(
            "cannot write the snapshot to {}",
            report::quoted(destination.display())
        )
    };

    let (snapshot, target) = match &job.place {
        Place::TopLevel => {
            let snapshot = Snapshot::new(destination, cancel)?;
            let target = snapshot.path().to_owned();
            (Some(snapshot), target)
        }
        Place::Nested { parent } => {
            // A crafted tree could have put a symlink on the way.
            fs::ensure_real_dirs(parent, path).with_context(cannot_write)?;
            (None, parent.join(path))
        }
    };

    // `fresh` holds the repository to hand back for sharing when we fetched it.
    let fresh;
    let ephemeral = if let Some(repo) = cached {
        report::detail!(
            reporter,
            "Submodule path {}: reusing the fetch of another submodule at the same commit",
            report::quoted(shown.display())
        );
        fresh = None;
        repo
    } else {
        let started = Instant::now();
        let (repo, strategy) = fetcher
            .fetch_commit(url, *commit, reporter)
            .with_context(|| format!("cannot fetch {commit} from {}", report::quoted(url)))?;
        report::detail!(
            reporter,
            "Submodule path {}: fetched {strategy} in {:.2}s",
            report::quoted(shown.display()),
            started.elapsed().as_secs_f64()
        );
        let repo = Arc::new(repo);
        fresh = Some(Arc::clone(&repo));
        repo
    };

    let repo = ephemeral.repo().to_thread_local();
    let tree_id = repo
        .find_commit(*commit)
        .with_context(|| {
            format!(
                "{commit} was fetched from {} but is not a commit",
                report::quoted(url)
            )
        })?
        .tree_id()
        .context("cannot read the tree of the fetched commit")?
        .detach();

    let extracted = tree::extract_tree(
        ephemeral.repo(),
        tree_id,
        &target,
        options.conversion,
        options.write_threads,
        cancel,
    )
    .with_context(cannot_write)?;
    report::detail!(
        reporter,
        "Submodule path {}: wrote {}, {} streamed",
        report::quoted(shown.display()),
        report::plural(extracted.scan.files, "file"),
        extracted.streamed
    );

    let children = nested(job, &repo, tree_id, &extracted, options, reporter)?;
    Ok(Produced {
        snapshot,
        target,
        fetched: fresh.map_or(Fetched::Reused, Fetched::Fresh),
        children,
    })
}

/// The submodules nested in `job`, unless recursion is off or too deep already.
fn nested(
    job: &Job,
    repo: &gix::Repository,
    tree_id: ObjectId,
    extracted: &tree::Extracted,
    options: &Options,
    reporter: &Reporter,
) -> Result<Vec<Planned>> {
    let Planned {
        url, destination, ..
    } = &job.planned;
    let gitlinks = &extracted.scan.gitlinks;
    if gitlinks.is_empty() {
        return Ok(Vec::new());
    }
    match options.recursion {
        Recursion::Off => return Ok(Vec::new()),
        Recursion::Limited(max) if job.depth + 1 >= max.get() => {
            report::warning!(
                reporter,
                "not planting {} of {}: --max-depth {} reached",
                report::plural(gitlinks.len(), "nested submodule"),
                report::quoted(destination.display()),
                max.get()
            );
            return Ok(Vec::new());
        }
        Recursion::Limited(_) | Recursion::Unlimited => {}
    }

    let modules = Modules::from_tree(repo, tree_id, reporter)
        .with_context(|| format!("cannot read .gitmodules of {}", report::quoted(url)))?;
    plan::plan(
        gitlinks,
        &modules,
        Some(url),
        destination,
        repo.config_snapshot().plumbing(),
        reporter,
    )
}
