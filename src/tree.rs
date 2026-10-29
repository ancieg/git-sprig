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

use std::{
    collections::HashSet,
    io::{self, Write},
    path::Path,
    sync::{
        Mutex, PoisonError,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
};

use anyhow::{Context, Result, anyhow, bail};
use gix::{
    AttributeStack, ObjectId, Repository, ThreadSafeRepository,
    attrs::StateRef,
    bstr::ByteSlice,
    filter::plumbing::eol::{self, AttributesDigest, Mode},
    objs::tree::EntryKind,
    odb::pack,
    worktree::stack::state::attributes::Source,
    zlib,
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

fn convert(
    pipeline: &mut gix::filter::Pipeline<'_>,
    data: &[u8],
    relative: &TreePath,
    out: &mut std::fs::File,
) -> Result<()> {
    let rela_path = fs::path_bytes(relative.as_path());
    let mut outcome = pipeline
        .convert_to_worktree(
            data,
            rela_path,
            gix::filter::plumbing::pipeline::convert::to_worktree::Options {
                can_delay: gix::filter::plumbing::driver::apply::Delay::Forbid,
                ..Default::default()
            },
        )
        .with_context(|| format!("cannot filter {}", report::quoted(relative.display())))?;
    if let Some(bytes) = outcome.as_bytes() {
        out.write_all(bytes)?;
    } else {
        let reader = outcome.as_read().ok_or_else(|| {
            anyhow!(
                "filter for {} produced no output",
                report::quoted(relative.display())
            )
        })?;
        io::copy(reader, out)?;
    }
    Ok(())
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

const CONVERSIONS: [&str; 6] = [
    "crlf",
    "text",
    "eol",
    "ident",
    "filter",
    "working-tree-encoding",
];

struct Conversions<'repo> {
    attributes: AttributeStack<'repo>,
    matches: gix::attrs::search::Outcome,
    eol: eol::Configuration,
}

impl<'repo> Conversions<'repo> {
    fn new(repo: &'repo Repository, tree_id: ObjectId) -> Result<Self> {
        let index = repo
            .index_from_tree(&tree_id)
            .context("cannot index the tree for .gitattributes")?;
        let attributes = repo
            .attributes_only(&index, Source::IdMapping)
            .context("cannot read .gitattributes")?;
        let matches = attributes.selected_attribute_matches(CONVERSIONS);
        let eol = gix::filter::Pipeline::options(repo)
            .context("cannot read the checkout configuration")?
            .eol_config;
        Ok(Self {
            attributes,
            matches,
            eol,
        })
    }

    /// Whether checkout writes `relative` byte for byte, decided as gix-filter decides it.
    fn is_verbatim(&mut self, relative: &Path) -> Result<bool> {
        self.attributes
            .at_entry(relative, None)?
            .matching_attributes(&mut self.matches);
        let states: Vec<StateRef<'_>> = self
            .matches
            .iter_selected()
            .map(|found| found.assignment.state)
            .collect();
        let [crlf, text, eol, ident, filter, encoding] = states[..] else {
            return Ok(false);
        };
        if ident.is_set()
            || matches!(filter, StateRef::Value(_))
            || !matches!(encoding, StateRef::Unspecified)
        {
            return Ok(false);
        }
        Ok(line_endings(crlf, text, eol, self.eol).to_eol(self.eol) != Some(Mode::CrLf))
    }
}

/// gix-filter's line-ending decision for a path's attributes, which it keeps private.
fn line_endings(
    crlf: StateRef<'_>,
    text: StateRef<'_>,
    eol: StateRef<'_>,
    config: eol::Configuration,
) -> AttributesDigest {
    let digest_of = |state: StateRef<'_>| match state {
        StateRef::Set => Some(AttributesDigest::Text),
        StateRef::Unset => Some(AttributesDigest::Binary),
        StateRef::Value(value) if value.as_bstr() == "input" => Some(AttributesDigest::TextInput),
        StateRef::Value(value) if value.as_bstr() == "auto" => Some(AttributesDigest::TextAuto),
        StateRef::Value(_) | StateRef::Unspecified => None,
    };
    let mut digest = digest_of(text).or_else(|| digest_of(crlf));

    if digest != Some(AttributesDigest::Binary) {
        let mode = match eol {
            StateRef::Value(value) if value.as_bstr() == "lf" => Some(Mode::Lf),
            StateRef::Value(value) if value.as_bstr() == "crlf" => Some(Mode::CrLf),
            _ => None,
        };
        digest = match (digest, mode) {
            (Some(AttributesDigest::TextAuto), Some(Mode::Lf)) => {
                Some(AttributesDigest::TextAutoInput)
            }
            (Some(AttributesDigest::TextAuto), Some(Mode::CrLf)) => {
                Some(AttributesDigest::TextAutoCrlf)
            }
            (_, Some(mode)) => Some(mode.into()),
            (digest, None) => digest,
        };
    }

    match digest {
        None => config.auto_crlf.into(),
        Some(AttributesDigest::Text) => config.to_eol().into(),
        Some(digest) => digest,
    }
}

/// Packed and loose objects that can be inflated without buffering a whole blob.
struct Objects {
    packs: Vec<pack::Bundle>,
    loose: std::path::PathBuf,
}

impl Objects {
    fn open(repo: &Repository) -> Result<Self> {
        // A linked worktree keeps its objects in the common directory.
        let loose = repo.common_dir().join("objects");
        let dir = loose.join("pack");
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                return Ok(Self {
                    packs: Vec::new(),
                    loose,
                });
            }
            Err(err) => {
                return Err(err)
                    .with_context(|| format!("cannot list {}", report::quoted(dir.display())));
            }
        };
        let mut packs = Vec::new();
        for entry in entries {
            let path = entry?.path();
            if path.extension().is_some_and(|extension| extension == "idx") {
                let bundle = pack::Bundle::at(&path, repo.object_hash())
                    .with_context(|| format!("cannot open {}", report::quoted(path.display())))?;
                packs.push(bundle);
            }
        }
        Ok(Self { packs, loose })
    }

    /// Inflate blob `id` straight into `out`. Returns `false` if it is only
    /// available as a packed delta or in another object database.
    fn stream(&self, id: ObjectId, out: &mut impl Write) -> Result<bool> {
        let Some((pack, entry)) = self.packs.iter().find_map(|bundle| {
            let index = bundle.index.lookup(id)?;
            let offset = bundle.index.pack_offset_at_index(index);
            Some((&bundle.pack, bundle.pack.entry(offset)))
        }) else {
            return self.stream_loose(id, out);
        };
        let entry = entry?;
        if !matches!(entry.header, pack::data::entry::Header::Blob) {
            return Ok(false);
        }

        let end = u64::try_from(pack.pack_end())?;
        let data = pack
            .entry_slice(entry.data_offset..end)
            .with_context(|| format!("pack entry of {id} is out of bounds"))?;
        let mut inflate = zlib::Decompress::new();
        let mut buf = vec![0; 1 << 16];
        loop {
            let consumed = usize::try_from(inflate.total_in())?;
            let before = inflate.total_out();
            let status =
                inflate.decompress(&data[consumed..], &mut buf, zlib::FlushDecompress::None)?;
            let produced = usize::try_from(inflate.total_out() - before)?;
            out.write_all(&buf[..produced])?;
            if status == zlib::Status::StreamEnd {
                break;
            }
            if produced == 0 && usize::try_from(inflate.total_in())? == consumed {
                bail!("pack entry of {id} is truncated");
            }
        }

        if inflate.total_out() != entry.decompressed_size {
            bail!(
                "pack entry of {id} inflated to {} bytes instead of {}",
                inflate.total_out(),
                entry.decompressed_size
            );
        }
        Ok(true)
    }

    fn stream_loose(&self, id: ObjectId, out: &mut impl Write) -> Result<bool> {
        let hex = id.to_string();
        let path = self.loose.join(&hex[..2]).join(&hex[2..]);
        let mut file = match std::fs::File::open(&path) {
            Ok(file) => file,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(err) => {
                return Err(err)
                    .with_context(|| format!("cannot open {}", report::quoted(path.display())));
            }
        };

        let mut inflate = zlib::Decompress::new();
        let mut input = [0; 1 << 14];
        let mut input_len = 0;
        let mut input_pos = 0;
        let mut input_done = false;
        let mut header = [0; 64];
        let mut header_len = 0;
        let mut output = vec![0; 1 << 16];
        let mut body = None;
        let mut body_size = 0;
        let mut body_written = 0_u64;

        loop {
            if input_pos == input_len && !input_done {
                input_len = std::io::Read::read(&mut file, &mut input)
                    .with_context(|| format!("cannot read {}", report::quoted(path.display())))?;
                input_pos = 0;
                input_done = input_len == 0;
            }
            let consumed = usize::try_from(inflate.total_in())?;
            let before = inflate.total_out();
            let status = if body.is_some() {
                inflate.decompress(
                    &input[input_pos..input_len],
                    &mut output,
                    zlib::FlushDecompress::None,
                )
            } else {
                inflate.decompress(
                    &input[input_pos..input_len],
                    &mut header[header_len..],
                    zlib::FlushDecompress::None,
                )
            }
            .with_context(|| format!("cannot inflate loose object {id}"))?;
            input_pos += usize::try_from(inflate.total_in())? - consumed;
            let produced = usize::try_from(inflate.total_out() - before)?;

            if body.is_some() {
                out.write_all(&output[..produced])?;
                body_written = body_written
                    .checked_add(u64::try_from(produced)?)
                    .ok_or_else(|| anyhow!("loose object {id} is too large"))?;
                if body_written > body_size {
                    bail!("loose object {id} exceeds its declared size of {body_size}");
                }
            } else {
                header_len += produced;
                if header[..header_len].contains(&0) {
                    let (kind, size, header_size) =
                        gix::objs::decode::loose_header(&header[..header_len])
                            .with_context(|| format!("invalid loose object {id}"))?;
                    if kind != gix::objs::Kind::Blob {
                        bail!("loose object {id} is {kind:?}, not a blob");
                    }
                    body_size = size;
                    body = Some(header_size);
                    out.write_all(&header[header_size..header_len])?;
                    body_written = u64::try_from(header_len - header_size)?;
                    if body_written > body_size {
                        bail!("loose object {id} exceeds its declared size of {body_size}");
                    }
                } else if header_len == header.len() {
                    bail!("loose object {id} has an overlong header");
                }
            }

            if status == zlib::Status::StreamEnd {
                if body.is_none() {
                    bail!("loose object {id} has no header");
                }
                if body_written != body_size {
                    bail!(
                        "loose object {id} inflated to {body_written} bytes instead of {body_size}"
                    );
                }
                return Ok(true);
            }
            if produced == 0 && usize::try_from(inflate.total_in())? == consumed {
                bail!("loose object {id} is truncated");
            }
        }
    }
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
    use gix::filter::plumbing::eol::AutoCrlf;

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
    fn loose_blobs_stream_across_buffer_boundaries() {
        for size in [
            0,
            1,
            55,
            56,
            57,
            58,
            59,
            60,
            63,
            64,
            65,
            1 << 14,
            (1 << 16) + 17,
            200_000,
        ] {
            let dir = tempfile::tempdir().unwrap();
            let repo = gix::init_bare(dir.path()).unwrap();
            let data: Vec<u8> = (0..size)
                .map(|index| u8::try_from(index % 251).unwrap())
                .collect();
            let id = repo.write_blob(&data).unwrap().detach();
            let objects = Objects::open(&repo).unwrap();
            let mut out = Vec::new();
            assert!(objects.stream(id, &mut out).unwrap(), "size {size}");
            assert_eq!(out, data, "size {size}");
        }
    }

    #[test]
    fn truncated_loose_objects_are_rejected() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let repo = gix::init_bare(dir.path()).unwrap();
        let id = repo.write_blob(vec![42; 100_000]).unwrap().detach();
        let hex = id.to_string();
        let path = dir.path().join("objects").join(&hex[..2]).join(&hex[2..]);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .unwrap()
            .set_len(2)
            .unwrap();

        let objects = Objects::open(&repo).unwrap();
        let mut out = Vec::new();
        assert!(objects.stream(id, &mut out).is_err());
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

    fn converts(attributes: [StateRef<'_>; 3], auto_crlf: AutoCrlf, eol: Option<Mode>) -> bool {
        let config = eol::Configuration { auto_crlf, eol };
        let [crlf, text, eol_attr] = attributes;
        line_endings(crlf, text, eol_attr, config).to_eol(config) == Some(Mode::CrLf)
    }

    #[test]
    fn line_endings_follow_gix_filter() {
        use StateRef::{Set, Unset, Unspecified, Value};
        let value = |v: &'static str| Value(v.into());
        let none = [Unspecified, Unspecified, Unspecified];

        assert!(!converts(none, AutoCrlf::Disabled, None));
        assert!(!converts(none, AutoCrlf::Input, None));
        assert!(converts(none, AutoCrlf::Enabled, None));
        assert!(converts(
            [Unspecified, Set, Unspecified],
            AutoCrlf::Disabled,
            Some(Mode::CrLf)
        ));
        assert!(!converts(
            [Unspecified, Set, Unspecified],
            AutoCrlf::Disabled,
            None
        ));

        assert!(!converts(
            [Unspecified, value("auto"), value("lf")],
            AutoCrlf::Enabled,
            None
        ));
        assert!(converts(
            [Unspecified, value("auto"), value("crlf")],
            AutoCrlf::Disabled,
            None
        ));
        assert!(converts(
            [Unspecified, Unspecified, value("crlf")],
            AutoCrlf::Disabled,
            None
        ));
        assert!(!converts(
            [Unspecified, Unset, value("crlf")],
            AutoCrlf::Enabled,
            None
        ));
        // `text` is checked before `crlf`, as in git.
        assert!(converts([Unset, Set, Unspecified], AutoCrlf::Enabled, None));
        assert!(!converts(
            [value("input"), Unspecified, Unspecified],
            AutoCrlf::Enabled,
            None
        ));
    }
}
