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

//! Reading `.gitmodules`, letting the configuration override it as git does,
//! and deciding which submodules are active.

use std::{
    collections::{HashMap, hash_map},
    os::unix::ffi::OsStrExt,
    path::Path,
};

use anyhow::{Context, Result};
use gix::{
    ObjectId, Repository,
    bstr::{BStr, BString, ByteSlice, ByteVec},
};

use crate::report::{self, Reporter};

#[derive(Debug, Clone)]
pub struct Entry {
    pub name: BString,
    /// Normalized, as the lookup by path uses it.
    pub path: BString,
    pub url: BString,
    /// `submodule.<name>.update`; `none` keeps git from checking it out.
    pub update: Option<BString>,
    /// Whether git would check it out, as the configuration decides.
    pub active: bool,
}

/// Whose `submodule.*` settings count, such as those that take precedence
/// over `.gitmodules`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// The superproject's own configuration too, as `git submodule` reads it.
    Repository,
    /// Only what a fresh clone would see: `-c`, the environment and the user's
    /// and system files, never the configuration of the repository read from.
    Clone,
}

impl Scope {
    /// Whether settings from where `meta` describes count in this scope.
    pub fn admits(self, meta: &gix::config::file::Metadata) -> bool {
        self == Scope::Repository
            || !matches!(
                meta.source,
                gix::config::Source::Local | gix::config::Source::Worktree
            )
    }
}

#[derive(Debug, Default)]
pub struct Modules {
    by_path: HashMap<BString, Entry>,
}

impl Modules {
    /// Parse the `.gitmodules` at the root of `tree_id`; a missing file means no submodules.
    pub fn from_tree(repo: &Repository, tree_id: ObjectId, reporter: &Reporter) -> Result<Self> {
        let tree = repo
            .find_tree(tree_id)
            .with_context(|| format!("missing tree {tree_id}"))?;

        let Some(entry) = tree.find_entry(".gitmodules") else {
            return Ok(Self::default());
        };
        if !entry.mode().is_blob() {
            return Ok(Self::default());
        }
        let blob = repo
            .find_blob(entry.object_id())
            .context("missing .gitmodules blob")?;

        Self::from_bytes(&blob.data, reporter)
    }

    pub fn from_bytes(bytes: &[u8], reporter: &Reporter) -> Result<Self> {
        let file = gix::submodule::File::from_bytes(bytes, None, &gix::config::File::default())
            .context("cannot parse .gitmodules")?;

        let mut by_path = HashMap::new();
        for name in file.names() {
            let name = name.to_owned();
            let path = match file.path(name.as_ref()) {
                Ok(path) => normalize_path(path.as_ref()),
                Err(err) => {
                    report::warning!(
                        reporter,
                        "ignoring submodule {} without a usable path: {err}",
                        report::quoted(&name)
                    );
                    continue;
                }
            };
            let Some(url) = file
                .config()
                .string(format!("submodule.{name}.url").as_str())
                .filter(|url| !url.is_empty())
            else {
                report::warning!(
                    reporter,
                    "ignoring submodule {} without a url",
                    report::quoted(&name)
                );
                continue;
            };
            match by_path.entry(path.clone()) {
                hash_map::Entry::Vacant(slot) => {
                    let update = file
                        .config()
                        .string(format!("submodule.{name}.update").as_str());
                    slot.insert(Entry {
                        name,
                        path,
                        url,
                        update,
                        active: true,
                    });
                }
                hash_map::Entry::Occupied(slot) => report::warning!(
                    reporter,
                    "submodules {} and {} are both configured for path {}, using {}",
                    report::quoted(&slot.get().name),
                    report::quoted(&name),
                    report::quoted(slot.key()),
                    report::quoted(&slot.get().name)
                ),
            }
        }
        Ok(Self { by_path })
    }

    /// Let `config` override `.gitmodules`, as git lets a `submodule.<name>.url`
    /// that `git submodule init` wrote, or that points at a mirror, win, and a
    /// `submodule.<name>.update` bring back a submodule `.gitmodules` leaves out;
    /// and decide from it which submodules are active.
    pub fn configure(&mut self, config: &gix::config::File, scope: Scope) -> Result<()> {
        let mut in_scope = |meta: &gix::config::file::Metadata| scope.admits(meta);
        let mut pathspecs = active_pathspecs(config, scope)?;
        for entry in self.by_path.values_mut() {
            let key = format!("submodule.{}.url", entry.name);
            if let Some(url) = config
                .string_filter(key.as_str(), &mut in_scope)
                .filter(|url| !url.is_empty())
            {
                entry.url = url;
            }
            let key = format!("submodule.{}.update", entry.name);
            if let Some(update) = config.string_filter(key.as_str(), &mut in_scope) {
                entry.update = Some(update);
            }
            entry.active =
                is_active(entry, config, scope, pathspecs.as_mut()).with_context(|| {
                    format!(
                        "cannot tell whether submodule {} is active",
                        report::quoted(&entry.name)
                    )
                })?;
        }
        Ok(())
    }

    pub fn by_path(&self, path: &Path) -> Option<&Entry> {
        self.by_path
            .get(&normalize_path(path.as_os_str().as_bytes().as_bstr()))
    }
}

/// The `submodule.active` pathspecs in `scope`, if any are set.
fn active_pathspecs(
    config: &gix::config::File,
    scope: Scope,
) -> Result<Option<gix::pathspec::Search>> {
    let Some(patterns) = config.strings_filter("submodule.active", |meta| scope.admits(meta))
    else {
        return Ok(None);
    };
    let patterns = patterns
        .iter()
        .map(|pattern| gix::pathspec::parse(pattern, gix::pathspec::Defaults::default()))
        .collect::<Result<Vec<_>, _>>()
        .context("invalid submodule.active pathspec")?;
    let search = gix::pathspec::Search::from_specs(patterns, None, Path::new(""))
        .context("invalid submodule.active pathspec")?;
    Ok(Some(search))
}

/// Whether `entry` is active, as `git submodule update --init` decides:
/// `submodule.<name>.active` first, then a `submodule.active` pathspec, and
/// active otherwise. git's last rule, "only if `submodule.<name>.url` is
/// configured", holds only after `git submodule init`, the step git-sprig
/// stands in for. gix implements the same rules, but reads the whole
/// configuration, so settings outside `scope` could not be left out.
fn is_active(
    entry: &Entry,
    config: &gix::config::File,
    scope: Scope,
    pathspecs: Option<&mut gix::pathspec::Search>,
) -> Result<bool> {
    let key = format!("submodule.{}.active", entry.name);
    if let Some(active) = config.boolean_filter(key.as_str(), |meta| scope.admits(meta))? {
        return Ok(active);
    }
    // git matches `submodule.active` against paths, not names.
    Ok(pathspecs.is_none_or(|search| {
        search
            .pattern_matching_relative_path(entry.path.as_ref(), Some(true), &mut |_, _, _, _| {
                false
            })
            .is_some_and(|matched| !matched.is_excluded())
    }))
}

/// `a//b/` and `./a/b` both describe `a/b`.
fn normalize_path(path: &BStr) -> BString {
    let mut out = BString::from(Vec::new());
    for component in path.split_str("/") {
        if component.is_empty() || component == b"." {
            continue;
        }
        if !out.is_empty() {
            out.push_byte(b'/');
        }
        out.extend_from_slice(component);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::report::{Reporter, Verbosity};

    fn reporter() -> Reporter {
        Reporter::buffered(Verbosity::Verbose)
    }

    #[test]
    fn gitmodules_are_parsed() {
        let modules = Modules::from_bytes(
            b"[submodule \"ruff\"]\n\tpath = ruff\n\turl = ../ruff.git\n\
              [submodule \"empty\"]\n\tpath = empty\n",
            &reporter(),
        )
        .unwrap();
        let entry = modules.by_path(Path::new("ruff")).expect("found by path");
        assert_eq!(entry.name, "ruff");
        assert_eq!(entry.url, "../ruff.git");
        assert!(modules.by_path(Path::new("empty")).is_none());
        assert!(modules.by_path(Path::new("nope")).is_none());
    }

    #[test]
    fn paths_are_normalized_and_the_first_entry_wins() {
        let modules = Modules::from_bytes(
            b"[submodule \"one\"]\n\tpath = ./vendor//x/\n\turl = https://a/one\n\
              [submodule \"two\"]\n\tpath = vendor/x\n\turl = https://a/two\n",
            &reporter(),
        )
        .unwrap();
        assert_eq!(modules.by_path(Path::new("vendor/x")).unwrap().name, "one");
    }

    #[test]
    fn configured_urls_win_within_their_scope() {
        use gix::config::Source::{Cli, Local};

        let gitmodules = b"[submodule \"lib\"]\n\tpath = lib\n\turl = https://a/lib\n";
        let config = |source: gix::config::Source| {
            let mut text = b"[submodule \"lib\"]\n\turl = https://mirror/lib\n".to_vec();
            gix::config::File::from_bytes_owned(
                &mut text,
                gix::config::file::Metadata::from(source),
                gix::config::file::init::Options::default(),
            )
            .unwrap()
        };
        let url = |source, scope| {
            let mut modules = Modules::from_bytes(gitmodules, &reporter()).unwrap();
            modules.configure(&config(source), scope).unwrap();
            modules.by_path(Path::new("lib")).unwrap().url.to_string()
        };
        assert_eq!(url(Local, Scope::Repository), "https://mirror/lib");
        assert_eq!(url(Local, Scope::Clone), "https://a/lib");
        assert_eq!(url(Cli, Scope::Clone), "https://mirror/lib");
    }

    #[test]
    fn submodules_are_active_unless_configured_otherwise() {
        use gix::config::Source::{Cli, Local};

        // Whether `docs` and `lib` are active with `text` from `source` in `scope`.
        let active = |text: &str, source, scope| {
            let mut modules = Modules::from_bytes(
                b"[submodule \"docs\"]\n\tpath = extra/docs\n\turl = https://a/docs\n\
                  [submodule \"lib\"]\n\tpath = vendor/lib\n\turl = https://a/lib\n",
                &reporter(),
            )
            .unwrap();
            let mut text = text.as_bytes().to_vec();
            let config = gix::config::File::from_bytes_owned(
                &mut text,
                gix::config::file::Metadata::from(source),
                gix::config::file::init::Options::default(),
            )
            .unwrap();
            modules.configure(&config, scope).unwrap();
            let active = |path| modules.by_path(Path::new(path)).unwrap().active;
            (active("extra/docs"), active("vendor/lib"))
        };
        let repo = Scope::Repository;

        assert_eq!(active("", Local, repo), (true, true));
        let by_name = "[submodule \"docs\"]\n\tactive = false\n";
        assert_eq!(active(by_name, Local, repo), (false, true));
        let by_path = "[submodule]\n\tactive = vendor/*\n";
        assert_eq!(active(by_path, Local, repo), (false, true));
        let name_wins = "[submodule]\n\tactive = vendor/*\n[submodule \"docs\"]\n\tactive = true\n";
        assert_eq!(active(name_wins, Local, repo), (true, true));

        // A clone sees no settings of the repository it is made from, only -c.
        assert_eq!(active(by_name, Local, Scope::Clone), (true, true));
        assert_eq!(active(by_path, Local, Scope::Clone), (true, true));
        assert_eq!(active(by_name, Cli, Scope::Clone), (false, true));
    }
}
