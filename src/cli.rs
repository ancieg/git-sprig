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
mod parameters;

use std::{num::NonZeroUsize, path::PathBuf};

use clap::{Parser, builder::TypedValueParser};

use crate::{
    config::{
        Action, Checkouts, Config, Direct, IndexUpdate, Mode, Source, Submodules, Superproject,
    },
    report::Verbosity,
    schedule::Recursion,
    tree::Conversion,
};

#[derive(Parser, Debug)]
#[command(
    name = "git-sprig",
    version,
    about = "Plant git repositories and submodules as plain files, without histories",
    long_about = "Fetches exactly the commit each submodule is pinned to, writes its files and \
                  does the same for nested submodules. A positional repository and destination \
                  materialize the complete root tree as well. No history and no .git directories \
                  are left behind.\n\n\
                  Each destination is replaced as a whole, once it has been written completely. \
                  Direct mode requires an explicit destination. Pathspecs after -- and --exclude \
                  filter only top-level submodules. All output goes to stderr.",
    override_usage = "git-sprig [OPTIONS] [REPOSITORY DIRECTORY] [-- PATHSPEC...]"
)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "a flat mirror of the command line; the program works from Config instead"
)]
pub(crate) struct Args {
    /// Run as if started in DIR, like `git -C`; may be repeated.
    #[arg(short = 'C', value_name = "DIR", action = clap::ArgAction::Append)]
    pub(crate) chdir: Vec<PathBuf>,

    /// Local or remote repository to materialize completely.
    #[arg(
        value_name = "REPOSITORY",
        requires = "directory",
        conflicts_with = "stage",
        conflicts_with = "work_tree"
    )]
    pub(crate) repository: Option<String>,

    /// Required destination for the complete repository tree.
    #[arg(
        value_name = "DIRECTORY",
        value_parser = clap::builder::PathBufValueParser::new().try_map(|path| destination(&path))
    )]
    pub(crate) directory: Option<PathBuf>,

    /// Top-level submodule pathspecs to include, after --.
    #[arg(skip)]
    pub(crate) includes: Vec<String>,

    /// Commit, branch or tag to use [default: the index, as for `git submodule update`,
    /// or HEAD of a REPOSITORY].
    #[arg(short = 'b', long, visible_alias = "branch", value_name = "REV")]
    pub(crate) revision: Option<String>,

    /// Where submodule paths start in submodule-only mode (default: the top of the work tree).
    #[arg(long = "work-tree", value_name = "DIR")]
    pub(crate) work_tree: Option<PathBuf>,

    /// Top-level submodule pathspec to exclude; may be repeated.
    #[arg(long = "exclude", value_name = "PATHSPEC", value_parser = valid_pathspec)]
    pub(crate) excludes: Vec<String>,

    /// Replace destinations that are or contain git checkouts, discarding work in them.
    #[arg(short, long)]
    pub(crate) force: bool,

    /// Replace the submodules in the superproject's index with their files, ready to commit.
    #[arg(long)]
    pub(crate) stage: bool,

    /// Write files as stored, without .gitattributes conversion.
    #[arg(long, visible_alias = "no-filters")]
    pub(crate) raw: bool,

    /// Materialize submodules, as by default, like `git clone --recurse-submodules`;
    /// =PATHSPEC limits the top-level ones and may be repeated.
    #[arg(
        long,
        value_name = "PATHSPEC",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "",
        action = clap::ArgAction::Append,
        value_parser = recurse_pathspec
    )]
    pub(crate) recurse_submodules: Vec<String>,

    /// Materialize the REPOSITORY alone, without its submodules, as `git clone` does.
    #[arg(long, requires = "repository", conflicts_with = "recurse_submodules")]
    pub(crate) no_recurse_submodules: bool,

    /// Do not recurse into submodules of submodules.
    #[arg(long)]
    pub(crate) no_recursive: bool,

    /// Levels of submodules to materialize (0 = unlimited).
    #[arg(long, default_value_t = 32, value_name = "N")]
    pub(crate) max_depth: usize,

    /// Base URL for relative submodule URLs; defaults to the remote, or the repository itself.
    #[arg(long, value_name = "URL")]
    pub(crate) remote_url: Option<String>,

    /// Set a git configuration value for this run, like `git -c`.
    #[arg(short = 'c', long = "config", value_name = "KEY=VALUE")]
    pub(crate) config: Vec<String>,

    /// Directory for the temporary repositories.
    #[arg(long, value_name = "DIR")]
    pub(crate) tempdir: Option<PathBuf>,

    /// Number of submodules fetched at the same time, 0 for the default
    /// [default: submodule.fetchJobs, or the number of CPUs up to 8].
    #[arg(short, long, value_name = "N")]
    pub(crate) jobs: Option<u16>,

    /// Show what would be fetched without changing anything.
    #[arg(short = 'n', long)]
    pub(crate) dry_run: bool,

    // Counted and overriding each other, so that, as in git, both may be
    // repeated and whichever comes last wins.
    /// Only print errors.
    #[arg(short, long, action = clap::ArgAction::Count, overrides_with = "verbose")]
    pub(crate) quiet: u8,

    /// Also print how each submodule was fetched and written.
    #[arg(short, long, action = clap::ArgAction::Count, overrides_with = "quiet")]
    pub(crate) verbose: u8,

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

impl Args {
    pub(crate) fn try_parse() -> Result<Self, clap::Error> {
        let mut argv: Vec<_> = std::env::args_os().collect();
        let includes = argv.iter().position(|arg| arg == "--").map(|index| {
            let includes = argv.split_off(index + 1);
            argv.pop();
            includes
        });
        let mut parsed = <Self as Parser>::try_parse_from(argv)?;
        for value in includes.into_iter().flatten() {
            let value = value.into_string().map_err(|_| {
                clap::Error::raw(
                    clap::error::ErrorKind::InvalidValue,
                    "pathspecs must be valid UTF-8",
                )
            })?;
            let value = valid_pathspec(&value).map_err(|message| {
                clap::Error::raw(clap::error::ErrorKind::InvalidValue, message)
            })?;
            parsed.includes.push(value);
        }
        Ok(parsed)
    }

    /// Read the flags into what they mean, once, so nothing downstream has to.
    pub(crate) fn into_action(self) -> anyhow::Result<Action> {
        if self.generate_man {
            return Ok(Action::GenerateMan);
        }
        if let Some(shell) = self.generate_completions {
            return Ok(Action::GenerateCompletions(shell));
        }
        let mut includes = self.includes;
        includes.extend(
            self.recurse_submodules
                .into_iter()
                .filter(|pattern| !pattern.is_empty()),
        );
        if self.no_recurse_submodules && !(includes.is_empty() && self.excludes.is_empty()) {
            return Err(conflict(
                "--no-recurse-submodules leaves every submodule out, so pathspecs cannot select any",
            ));
        }
        let source = match self.repository {
            Some(from) => Source::Direct(Direct {
                from,
                // clap requires the destination along with the repository.
                into: self.directory.unwrap_or_default(),
            }),
            None => Source::Superproject(Superproject {
                work_tree: self.work_tree,
            }),
        };
        // Those from `git -c` come first, so that our own -c wins, as in git.
        let mut overrides = parameters::inherited_overrides()?;
        overrides.extend(self.config);
        Ok(Action::Plant(Box::new(Config {
            source,
            chdir: self.chdir,
            rev: self.revision,
            includes,
            excludes: self.excludes,
            remote_url: self.remote_url.as_deref().map(remote_url).transpose()?,
            overrides,
            tempdir: self.tempdir,
            jobs: self.jobs,
            recursion: recursion(self.no_recursive, self.max_depth),
            submodules: if self.no_recurse_submodules {
                Submodules::Skip
            } else {
                Submodules::Include
            },
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
            verbosity: if self.quiet > 0 {
                Verbosity::Quiet
            } else if self.verbose > 0 {
                Verbosity::Verbose
            } else {
                Verbosity::Normal
            },
        })))
    }
}

/// The destination is replaced by renaming it. Renaming `.`, `..` or `/`
/// fails only after everything has been fetched, so they are refused here;
/// a trailing `/.` would fail the same way and is dropped.
fn destination(path: &std::path::Path) -> Result<PathBuf, String> {
    let path: PathBuf = path.components().collect();
    if path.file_name().is_none() {
        return Err("the destination is replaced as a whole and must end in a name".to_owned());
    }
    Ok(path)
}

/// `--remote-url`, read once for both modes.
fn remote_url(url: &str) -> anyhow::Result<gix::Url> {
    gix::url::parse(gix::bstr::BStr::new(url)).map_err(|err| {
        anyhow::Error::new(err).context(format!(
            "--remote-url {} is not a valid git URL",
            crate::report::quoted(url)
        ))
    })
}

/// A conflict clap cannot see, since pathspecs after `--` never reach it.
fn conflict(message: &str) -> anyhow::Error {
    clap::Error::raw(
        clap::error::ErrorKind::ArgumentConflict,
        format!("{message}\n"),
    )
    .into()
}

/// `--recurse-submodules` without a pathspec selects nothing in particular.
fn recurse_pathspec(value: &str) -> Result<String, String> {
    if value.is_empty() {
        Ok(String::new())
    } else {
        valid_pathspec(value)
    }
}

/// Only parsed here: a pattern is normalized once it is known where in the
/// work tree it is relative to, so `../lib` works from a subdirectory.
fn valid_pathspec(value: &str) -> Result<String, String> {
    gix::pathspec::parse(value.as_bytes(), gix::pathspec::Defaults::default())
        .map(|_| value.to_owned())
        .map_err(|err| err.to_string())
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
