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

//! Inflating blobs from a repository's own storage straight into a file, so a
//! large one is never held in memory whole, which gix cannot do yet.

use std::io::{self, Write};

use anyhow::{Context, Result, anyhow, bail};
use gix::{ObjectId, Repository, odb::pack, zlib};

use crate::report;

/// Packed and loose objects that can be inflated without buffering a whole blob.
pub(super) struct Objects {
    packs: Vec<pack::Bundle>,
    loose: std::path::PathBuf,
}

impl Objects {
    pub(super) fn open(repo: &Repository) -> Result<Self> {
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
    pub(super) fn stream(&self, id: ObjectId, out: &mut impl Write) -> Result<bool> {
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
