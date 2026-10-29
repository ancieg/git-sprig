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

//! Walking a git tree and writing it out as plain files.

mod convert;
mod objects;

use std::{
    collections::HashSet,
    io::Write,
    path::Path,
    sync::{
        Mutex, PoisonError,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
};

use anyhow::{Context, Result, bail};
use gix::{ObjectId, Repository, ThreadSafeRepository, bstr::ByteSlice, objs::tree::EntryKind};

use self::{
    convert::{Conversions, convert},
    objects::Objects,
};
use crate::{
    cancel::CancelToken,
    error, fs,
    name::{EntryName, TreePath},
    report::{self, Reporter},
};

/// Below this many files per thread, another thread costs more than it saves.
const FILES_PER_THREAD: usize = 64;

/// Whether files pass through `.gitattributes` conversion or are written as stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Conversion {
    Gitattributes,
    Raw,
}

/// What [`extract_tree`] wrote: the tree it saw, and how many blobs it streamed.
pub struct Extracted {
    pub scan: Scan,
    pub streamed: usize,
}

/// A `160000` entry: the commit another repository is pinned to.
#[derive(Debug, Clone)]
pub struct Gitlink {
    pub path: TreePath,
    pub commit: ObjectId,
}

#[derive(Debug, Default)]
pub struct Scan {
    pub gitlinks: Vec<Gitlink>,
    /// Regular files, executables and symlinks.
    pub files: usize,
}

impl Scan {
    fn of(entries: &[Entry]) -> Self {
        let mut scan = Self::default();
        for entry in entries {
            match entry.kind {
                EntryKind::Tree => {}
                EntryKind::Commit => scan.gitlinks.push(Gitlink {
                    path: entry.path.clone(),
                    commit: entry.id,
                }),
                EntryKind::Blob | EntryKind::BlobExecutable | EntryKind::Link => scan.files += 1,
            }
        }
        scan
    }
}

struct Entry {
    kind: EntryKind,
    id: ObjectId,
    path: TreePath,
}

/// Validate every entry of `tree_id` and find its gitlinks, without touching the filesystem.
pub fn scan_tree(repo: &Repository, tree_id: ObjectId) -> Result<Scan> {
    Ok(Scan::of(&list_tree(repo, tree_id)?))
}

/// The gitlinks staged in `index`, their paths validated name by name as a
/// tree's are, since an index can be crafted as well. An unmerged gitlink is
/// skipped, as `git submodule update` skips it.
pub fn staged_gitlinks(index: &gix::index::State, reporter: &Reporter) -> Result<Vec<Gitlink>> {
    let mut gitlinks = Vec::new();
    let mut unmerged: Option<&gix::bstr::BStr> = None;
    for entry in index.entries() {
        if !entry.mode.is_submodule() {
            continue;
        }
        let raw = entry.path(index);
        if entry.stage() != gix::index::entry::Stage::Unconflicted {
            // Each side of a conflict has an entry of its own.
            if unmerged != Some(raw) {
                report::status!(
                    reporter,
                    "Skipping unmerged submodule {}",
                    report::quoted(raw)
                );
                unmerged = Some(raw);
            }
            continue;
        }
        let mut path = TreePath::root();
        for name in raw.split_str("/") {
            let name = EntryName::new(name.as_bstr(), false)
                .with_context(|| format!("in index entry {}", report::quoted(raw)))?;
            path = path.join(name);
        }
        gitlinks.push(Gitlink {
            path,
            commit: entry.id,
        });
    }
    Ok(gitlinks)
}

/// Write `tree_id` into the empty directory `destination` with up to `threads`
/// threads; gitlinks become empty directories. Returns what the tree holds and
/// how many files were streamed.
pub fn extract_tree(
    repo: &ThreadSafeRepository,
    tree_id: ObjectId,
    destination: &Path,
    conversion: Conversion,
    threads: usize,
    cancel: &CancelToken,
) -> Result<Extracted> {
    let local = repo.to_thread_local();
    // Validate first: looking up attributes indexes the tree, which would
    // otherwise be the first to trip over a hostile entry, less clearly.
    let entries = list_tree(&local, tree_id)?;
    let scan = Scan::of(&entries);

    let mut conversions = (conversion == Conversion::Gitattributes)
        .then(|| Conversions::new(&local, tree_id))
        .transpose()?;
    let mut files = Vec::with_capacity(scan.files);
    for entry in &entries {
        match entry.kind {
            EntryKind::Tree | EntryKind::Commit => {
                fs::ensure_dir(&destination.join(&entry.path))?;
            }
            EntryKind::Link => files.push(File {
                entry,
                convert: false,
            }),
            EntryKind::Blob | EntryKind::BlobExecutable => {
                let convert = match conversions.as_mut() {
                    Some(conversions) => !conversions.is_verbatim(entry.path.as_path())?,
                    None => false,
                };
                files.push(File { entry, convert });
            }
        }
    }
    drop(conversions);

    let writer = Writer {
        repo,
        tree_id,
        destination,
        packs: Objects::open(&local)?,
        files,
        cancel,
        next: AtomicUsize::new(0),
        streamed: AtomicUsize::new(0),
        stop: AtomicBool::new(false),
        error: Mutex::new(None),
    };
    // Directories exist and names are unique, so no two threads touch the same path.
    let threads = threads
        .min(writer.files.len().div_ceil(FILES_PER_THREAD))
        .max(1);
    thread::scope(|scope| {
        for _ in 1..threads {
            scope.spawn(|| writer.work());
        }
        writer.work();
    });

    if let Some(err) = writer
        .error
        .into_inner()
        .unwrap_or_else(PoisonError::into_inner)
    {
        return Err(err);
    }
    Ok(Extracted {
        scan,
        streamed: writer.streamed.into_inner(),
    })
}

/// git's own default cap on how deeply trees may nest (`core.maxTreeDepth`).
const DEFAULT_MAX_TREE_DEPTH: usize = 4096;

/// The children of one tree, decoded into owned values so the tree can be dropped.
type Children = Vec<(EntryKind, ObjectId, gix::bstr::BString)>;

/// One open tree during the walk, standing in for a recursion frame.
struct Frame {
    tree_id: ObjectId,
    /// The tree's own path, which its children hang off; empty for the root.
    relative: TreePath,
    children: Children,
    next: usize,
    /// Names already seen in this tree, so a duplicate cannot slip through.
    seen: HashSet<gix::bstr::BString>,
}

/// Every entry below `tree_id`, validated, directories before their contents.
///
/// Walks with an explicit stack rather than recursion, so a tree crafted to
/// nest thousands deep cannot overflow the stack; anything past git's
/// `core.maxTreeDepth` is refused, as git refuses it.
fn list_tree(repo: &Repository, tree_id: ObjectId) -> Result<Vec<Entry>> {
    let max_depth = max_tree_depth(repo);
    let mut out = Vec::new();
    let mut stack = vec![Frame {
        tree_id,
        relative: TreePath::root(),
        children: read_children(repo, tree_id, &TreePath::root())?,
        next: 0,
        seen: HashSet::new(),
    }];

    while let Some(frame) = stack.last_mut() {
        let Some((kind, id, filename)) = frame.children.get(frame.next).cloned() else {
            stack.pop();
            continue;
        };
        frame.next += 1;

        let name =
            EntryName::new(filename.as_bstr(), kind == EntryKind::Link).with_context(|| {
                format!(
                    "in tree {} below {}",
                    frame.tree_id,
                    display(&frame.relative)
                )
            })?;
        // A name listed twice lets a symlink replace a directory that a nested
        // submodule is later written into. Git refuses such trees too.
        if !frame.seen.insert(filename.clone()) {
            bail!(
                "tree {} lists {} twice below {}",
                frame.tree_id,
                report::quoted(filename.as_bstr()),
                display(&frame.relative)
            );
        }
        let path = frame.relative.join(name);

        out.push(Entry {
            kind,
            id,
            path: path.clone(),
        });
        if kind == EntryKind::Tree {
            if stack.len() >= max_depth {
                bail!("tree {id} nests more than {max_depth} levels deep");
            }
            let children = read_children(repo, id, &path)?;
            stack.push(Frame {
                tree_id: id,
                relative: path,
                children,
                next: 0,
                seen: HashSet::new(),
            });
        }
    }

    Ok(out)
}

/// Decode one tree's entries into owned values.
fn read_children(repo: &Repository, tree_id: ObjectId, relative: &TreePath) -> Result<Children> {
    let tree = repo
        .find_tree(tree_id)
        .with_context(|| format!("missing tree {tree_id} for {}", display(relative)))?;
    tree.iter()
        .map(|entry| {
            let entry = entry.with_context(|| format!("damaged tree {tree_id}"))?;
            Ok((entry.kind(), entry.object_id(), entry.filename().to_owned()))
        })
        .collect()
}

/// git's `core.maxTreeDepth`, or its default when the setting is absent.
fn max_tree_depth(repo: &Repository) -> usize {
    repo.config_snapshot()
        .integer("core.maxTreeDepth")
        .and_then(|depth| usize::try_from(depth).ok())
        .filter(|&depth| depth > 0)
        .unwrap_or(DEFAULT_MAX_TREE_DEPTH)
}

struct File<'a> {
    entry: &'a Entry,
    /// Whether checkout changes the bytes, so the blob goes through the filter pipeline.
    convert: bool,
}

/// Files of one tree, shared out between threads.
struct Writer<'a> {
    repo: &'a ThreadSafeRepository,
    tree_id: ObjectId,
    destination: &'a Path,
    packs: Objects,
    files: Vec<File<'a>>,
    cancel: &'a CancelToken,
    next: AtomicUsize,
    streamed: AtomicUsize,
    stop: AtomicBool,
    error: Mutex<Option<anyhow::Error>>,
}

impl Writer<'_> {
    fn work(&self) {
        let repo = self.repo.to_thread_local();
        let mut pipeline = None;
        while !self.stop.load(Ordering::Relaxed) {
            let Some(file) = self.files.get(self.next.fetch_add(1, Ordering::Relaxed)) else {
                break;
            };
            let result = if self.cancel.requested() {
                Err(error::Interrupted.into())
            } else {
                self.write(&repo, &mut pipeline, file)
            };
            if let Err(err) = result {
                self.error
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .get_or_insert(err);
                self.stop.store(true, Ordering::Relaxed);
            }
        }
    }

    fn write<'repo>(
        &self,
        repo: &'repo Repository,
        pipeline: &mut Option<gix::filter::Pipeline<'repo>>,
        file: &File<'_>,
    ) -> Result<()> {
        let Entry {
            kind,
            id,
            path: relative,
        } = file.entry;
        let path = self.destination.join(relative);
        fs::remove_existing(&path)?;

        if *kind == EntryKind::Link {
            let target = find_blob(repo, *id, relative)?;
            return fs::symlink(&path, &target.data).with_context(|| {
                format!("cannot create symlink {}", report::quoted(path.display()))
            });
        }

        let mut out = fs::create_file(&path, *kind == EntryKind::BlobExecutable)?;
        let mut write = || -> Result<()> {
            if !file.convert && self.packs.stream(*id, &mut out)? {
                self.streamed.fetch_add(1, Ordering::Relaxed);
                return Ok(());
            }
            let blob = find_blob(repo, *id, relative)?;
            if !file.convert {
                return Ok(out.write_all(&blob.data)?);
            }
            let pipeline = if let Some(pipeline) = pipeline.as_mut() {
                pipeline
            } else {
                let (created, _index) = repo
                    .filter_pipeline(Some(self.tree_id))
                    .context("cannot set up the .gitattributes filter pipeline")?;
                pipeline.insert(created)
            };
            convert(pipeline, &blob.data, relative, &mut out)
        };
        write().with_context(|| format!("cannot write {}", report::quoted(path.display())))
    }
}

fn find_blob<'repo>(
    repo: &'repo Repository,
    id: ObjectId,
    relative: &TreePath,
) -> Result<gix::Blob<'repo>> {
    repo.find_blob(id).with_context(|| {
        format!(
            "missing blob {id} for {}",
            report::quoted(relative.display())
        )
    })
}

fn display(path: &TreePath) -> String {
    if path.is_root() {
        "<root>".into()
    } else {
        report::quoted(path.display()).to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a tree nested `depth` levels deep with one blob at the bottom.
    fn deep_tree(repo: &Repository, depth: usize) -> ObjectId {
        use gix::objs::{Tree, tree::Entry};
        let mut oid = repo.write_blob(b"x\n").unwrap().detach();
        let (mut mode, mut name) = (EntryKind::Blob, "f");
        for _ in 0..depth {
            let tree = Tree {
                entries: vec![Entry {
                    mode: mode.into(),
                    filename: name.into(),
                    oid,
                }],
            };
            oid = repo.write_object(&tree).unwrap().detach();
            (mode, name) = (EntryKind::Tree, "d");
        }
        oid
    }

    #[test]
    fn deep_trees_do_not_overflow_the_stack() {
        // A tree this deep overflowed the old recursive walk; the iterative one
        // handles it, and it stays under core.maxTreeDepth so the walk succeeds.
        let dir = tempfile::tempdir().unwrap();
        let repo = gix::init_bare(dir.path()).unwrap();
        let tree_id = deep_tree(&repo, 3000);
        let scan = scan_tree(&repo, tree_id).unwrap();
        assert_eq!(scan.files, 1);
        assert!(scan.gitlinks.is_empty());
    }

    #[test]
    fn trees_past_the_depth_limit_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let repo = gix::init_bare(dir.path()).unwrap();
        let tree_id = deep_tree(&repo, DEFAULT_MAX_TREE_DEPTH + 10);
        let err = scan_tree(&repo, tree_id).unwrap_err().to_string();
        assert!(err.contains("levels deep"), "{err}");
    }
}
