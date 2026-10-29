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

//! What the command line asked for, once read: the program works from this
//! rather than from clap's flat `Args`, so intent is in the types and each
//! setting is decided in one place ([`crate::cli`]).

use std::path::PathBuf;

use crate::{
    cli::generate::CompletionShell, report::Verbosity, schedule::Recursion, tree::Conversion,
};

/// What one run of git-sprig is meant to do.
pub enum Action {
    Plant(Box<Config>),
    GenerateMan,
    GenerateCompletions(CompletionShell),
}

/// Whether to replace a submodule directory that is a git checkout of its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Checkouts {
    Refuse,
    Replace,
}

/// Whether the superproject's index is left alone or the files are staged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexUpdate {
    Leave,
    Stage,
}

/// Whether a direct export takes the repository's submodules along.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Submodules {
    Include,
    Skip,
}

/// Whether to do the work or only say what it would be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Plant,
    DryRun,
}

pub enum Source {
    /// Read gitlinks from a local superproject, leaving its own files alone.
    Superproject {
        /// Where submodule paths are rooted; the top of the work tree if absent.
        work_tree: Option<PathBuf>,
    },
    /// Materialize the complete root tree of a local or remote repository.
    Direct(Direct),
}

pub struct Direct {
    /// A URL or a local path.
    pub from: String,
    /// The destination, replaced as a whole.
    pub into: PathBuf,
}

pub struct Config {
    pub source: Source,
    /// Directories to change into first, in order, like `git -C`.
    pub chdir: Vec<PathBuf>,
    /// The revision asked for; without one, submodule-only mode reads the index.
    pub rev: Option<String>,
    /// Top-level submodule paths to include, or all when empty.
    pub includes: Vec<String>,
    /// Top-level submodule paths to exclude.
    pub excludes: Vec<String>,
    /// Base for relative submodule URLs; the source's remote or URL if absent.
    pub remote_url: Option<String>,
    /// `key=value` pairs for the fetches, like `git -c`.
    pub overrides: Vec<String>,
    pub tempdir: Option<PathBuf>,
    /// From -j: 0 asks for the default, and `None` for `submodule.fetchJobs`.
    pub jobs: Option<u16>,
    pub recursion: Recursion,
    pub submodules: Submodules,
    pub conversion: Conversion,
    pub checkouts: Checkouts,
    pub index: IndexUpdate,
    pub mode: Mode,
    pub verbosity: Verbosity,
    /// About the command line, such as deprecated options; printed first.
    pub warnings: Vec<String>,
}

impl Config {
    /// The revision of a direct source: the one asked for, or `HEAD`.
    pub fn direct_rev(&self) -> &str {
        self.rev.as_deref().unwrap_or("HEAD")
    }
}
