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

//! Whether checkout changes a file's bytes, and changing them as it does:
//! line endings, `ident`, filter drivers and working-tree encodings, as
//! `.gitattributes` and the configuration ask for them.

use std::{
    io::{self, Write},
    path::Path,
};

use anyhow::{Context, Result, anyhow};
use gix::{
    AttributeStack, ObjectId, Repository,
    attrs::StateRef,
    filter::plumbing::eol::{self, AttributesDigest, Mode},
    worktree::stack::state::attributes::Source,
};

use crate::{fs, name::TreePath, report};

pub(super) fn convert(
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

const CONVERSIONS: [&str; 6] = [
    "crlf",
    "text",
    "eol",
    "ident",
    "filter",
    "working-tree-encoding",
];

pub(super) struct Conversions<'repo> {
    attributes: AttributeStack<'repo>,
    matches: gix::attrs::search::Outcome,
    eol: eol::Configuration,
}

impl<'repo> Conversions<'repo> {
    pub(super) fn new(repo: &'repo Repository, tree_id: ObjectId) -> Result<Self> {
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
    pub(super) fn is_verbatim(&mut self, relative: &Path) -> Result<bool> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use gix::filter::plumbing::eol::AutoCrlf;

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
