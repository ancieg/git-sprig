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

//! The command line, as clap parses it.

pub mod generate;

use std::{num::NonZeroUsize, path::PathBuf};

use clap::Parser;

use crate::{
    config::{Action, Checkouts, Config, IndexUpdate, Mode},
    report::Verbosity,
    schedule::Recursion,
    tree::Conversion,
};

#[derive(Parser, Debug)]
#[command(
    name = "git-sprig",
    version,
    about = "Plant git submodules as plain files, without their histories",
    long_about = "Fetches exactly the commit each submodule is pinned to, writes its files and \
                  does the same for nested submodules. No history and no .git directories are \
                  left behind.\n\n\
                  Each submodule directory is replaced as a whole, once it has been written \
                  completely. All output goes to stderr."
)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "a flat mirror of the command line; the program works from Config instead"
)]
pub(crate) struct Args {
    /// Superproject to read the submodules from.
    #[arg(short = 'C', long, default_value = ".", value_name = "DIR")]
    pub(crate) repo: PathBuf,

    /// Revision of the superproject to use.
    #[arg(long, default_value = "HEAD", value_name = "REV")]
    pub(crate) rev: String,

    /// Directory the submodule paths are resolved against.
    #[arg(short, long, default_value = ".", value_name = "DIR")]
    pub(crate) basedir: PathBuf,

    /// Replace submodule directories that have a .git of their own, discarding work in them.
    #[arg(long)]
    pub(crate) force: bool,

    /// Replace the submodules in the superproject's index with their files, ready to commit.
    #[arg(long)]
    pub(crate) stage: bool,

    /// Write files as stored, without .gitattributes conversion.
    #[arg(long)]
    pub(crate) raw: bool,

    /// Do not recurse into submodules of submodules.
    #[arg(long)]
    pub(crate) no_recursive: bool,

    /// Levels of submodules to materialize (0 = unlimited).
    #[arg(long, default_value_t = 32, value_name = "N")]
    pub(crate) max_depth: usize,

    /// Base URL for relative submodule URLs; defaults to the superproject's remote.
    #[arg(long, value_name = "URL")]
    pub(crate) remote_url: Option<String>,

    /// Configuration for the fetches, like `git -c`.
    #[arg(short = 'c', long = "config", value_name = "KEY=VALUE")]
    pub(crate) config: Vec<String>,

    /// Directory for the temporary repositories.
    #[arg(long, value_name = "DIR")]
    pub(crate) tempdir: Option<PathBuf>,

    /// Number of submodules fetched at the same time (default: CPUs, at most 8).
    #[arg(
        short,
        long,
        default_value_t = default_jobs(),
        value_name = "N",
        value_parser = clap::value_parser!(u16).range(1..)
    )]
    pub(crate) jobs: u16,

    /// Show what would be fetched without changing anything.
    #[arg(long)]
    pub(crate) dry_run: bool,

    /// Only print errors.
    #[arg(short, long, conflicts_with = "verbose")]
    pub(crate) quiet: bool,

    /// Also print how each submodule was fetched and written.
    #[arg(short, long)]
    pub(crate) verbose: bool,

    /// Print the manual page, to install as git-sprig(1), and exit.
    #[arg(long, exclusive = true, help_heading = "Installation")]
    pub(crate) generate_man: bool,

    /// Print shell completions, for `git sprig` too, and exit.
    #[arg(
        long,
        value_name = "SHELL",
        exclusive = true,
        help_heading = "Installation"
    )]
    pub(crate) generate_completions: Option<generate::CompletionShell>,
}

/// Fetches mostly wait on the network, so they gain from running side by side;
/// the cap keeps a run from hammering one server.
pub(crate) fn default_jobs() -> u16 {
    u16::try_from(crate::app::available_cpus().min(8)).unwrap_or(8)
}

impl Args {
    /// Read the flags into what they mean, once, so nothing downstream has to.
    pub(crate) fn into_action(self) -> Action {
        if self.generate_man {
            return Action::GenerateMan;
        }
        if let Some(shell) = self.generate_completions {
            return Action::GenerateCompletions(shell);
        }
        Action::Plant(Box::new(Config {
            repo: self.repo,
            rev: self.rev,
            basedir: self.basedir,
            remote_url: self.remote_url,
            overrides: self.config,
            tempdir: self.tempdir,
            // clap keeps --jobs in 1..=u16::MAX, so it is never zero.
            jobs: NonZeroUsize::new(usize::from(self.jobs)).unwrap_or(NonZeroUsize::MIN),
            recursion: recursion(self.no_recursive, self.max_depth),
            conversion: if self.raw {
                Conversion::Raw
            } else {
                Conversion::Gitattributes
            },
            checkouts: if self.force {
                Checkouts::Replace
            } else {
                Checkouts::Refuse
            },
            index: if self.stage {
                IndexUpdate::Stage
            } else {
                IndexUpdate::Leave
            },
            mode: if self.dry_run {
                Mode::DryRun
            } else {
                Mode::Plant
            },
            verbosity: if self.quiet {
                Verbosity::Quiet
            } else if self.verbose {
                Verbosity::Verbose
            } else {
                Verbosity::Normal
            },
        }))
    }
}

/// How deep to recurse, from `--no-recursive` and `--max-depth` (0 = unlimited).
fn recursion(no_recursive: bool, max_depth: usize) -> Recursion {
    if no_recursive {
        Recursion::Off
    } else {
        match NonZeroUsize::new(max_depth) {
            Some(max) => Recursion::Limited(max),
            None => Recursion::Unlimited,
        }
    }
}
