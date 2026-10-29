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

use clap::{Parser, builder::TypedValueParser};

use crate::{
    config::{Action, Checkouts, Config, Direct, IndexUpdate, Mode, Source},
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

    /// The former name of -C, which now changes directory as git does.
    #[arg(long, value_name = "DIR", hide = true, conflicts_with = "chdir")]
    pub(crate) repo: Option<PathBuf>,

    /// Local or remote repository to materialize completely.
    #[arg(
        value_name = "REPOSITORY",
        requires = "directory",
        conflicts_with = "stage",
        conflicts_with = "basedir",
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

    /// Commit, branch or tag of the source repository to use.
    #[arg(long, alias = "rev", default_value = "HEAD", value_name = "REV")]
    pub(crate) revision: String,

    /// Where submodule paths start in submodule-only mode (default: the top of the work tree).
    #[arg(long = "work-tree", value_name = "DIR")]
    pub(crate) work_tree: Option<PathBuf>,

    /// The former name of --work-tree.
    #[arg(
        short = 'b',
        long,
        value_name = "DIR",
        hide = true,
        conflicts_with = "work_tree"
    )]
    pub(crate) basedir: Option<PathBuf>,

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

/// Fetches mostly wait on the network, so they gain from running side by side;
/// the cap keeps a run from hammering one server.
pub(crate) fn default_jobs() -> u16 {
    u16::try_from(crate::app::available_cpus().min(8)).unwrap_or(8)
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
        let mut warnings = Vec::new();
        if self.basedir.is_some() {
            warnings.push("-b and --basedir are deprecated; use --work-tree".to_owned());
        }
        if self.repo.is_some() {
            warnings
                .push("--repo is deprecated; use -C, which changes directory as in git".to_owned());
        }
        let source = match self.repository {
            Some(from) => Source::Direct(Direct {
                from,
                // clap requires the destination along with the repository.
                into: self.directory.unwrap_or_default(),
            }),
            None => Source::Superproject {
                work_tree: self.work_tree.or(self.basedir),
            },
        };
        // Those from `git -c` come first, so that our own -c wins, as in git.
        let mut overrides = inherited_overrides()?;
        overrides.extend(self.config);
        Ok(Action::Plant(Box::new(Config {
            source,
            chdir: self.chdir.into_iter().chain(self.repo).collect(),
            warnings,
            rev: self.revision,
            includes: self.includes,
            excludes: self.excludes,
            remote_url: self.remote_url,
            overrides,
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

/// Only parsed here: a pattern is normalized once it is known where in the
/// work tree it is relative to, so `../lib` works from a subdirectory.
fn valid_pathspec(value: &str) -> Result<String, String> {
    gix::pathspec::parse(value.as_bytes(), gix::pathspec::Defaults::default())
        .map(|_| value.to_owned())
        .map_err(|err| err.to_string())
}

/// The settings that `git -c` hands down to `git sprig` in
/// `GIT_CONFIG_PARAMETERS`, as `-c` arguments.
fn inherited_overrides() -> anyhow::Result<Vec<String>> {
    match std::env::var("GIT_CONFIG_PARAMETERS") {
        Ok(parameters) => config_parameters(&parameters)
            .ok_or_else(|| anyhow::anyhow!("bogus format in GIT_CONFIG_PARAMETERS")),
        Err(std::env::VarError::NotPresent) => Ok(Vec::new()),
        Err(std::env::VarError::NotUnicode(_)) => {
            anyhow::bail!("GIT_CONFIG_PARAMETERS is not valid UTF-8")
        }
    }
}

/// Parse `GIT_CONFIG_PARAMETERS` as git does: shell-quoted `'key'='value'`
/// pairs, `'key'=` for an implicit true, or `'key=value'` from older git.
fn config_parameters(mut rest: &str) -> Option<Vec<String>> {
    let mut overrides = Vec::new();
    rest = rest.trim_start();
    while !rest.is_empty() {
        let (key, after) = sq_dequote(rest)?;
        rest = after;
        match rest.strip_prefix('=') {
            Some(after) if after.starts_with('\'') => {
                let (value, after) = sq_dequote(after)?;
                overrides.push(format!("{key}={value}"));
                rest = after;
            }
            Some(after) => {
                overrides.push(key);
                rest = after;
            }
            None => overrides.push(key),
        }
        if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
            return None;
        }
        rest = rest.trim_start();
    }
    Some(overrides)
}

/// One word quoted as git's `sq_quote` writes it: within `'…'`, a quote is
/// `'\''` and an exclamation mark `'\!'`.
fn sq_dequote(input: &str) -> Option<(String, &str)> {
    let mut rest = input.strip_prefix('\'')?;
    let mut word = String::new();
    loop {
        let end = rest.find('\'')?;
        word.push_str(&rest[..end]);
        rest = &rest[end + 1..];
        match rest.as_bytes() {
            [b'\\', quoted @ (b'\'' | b'!'), b'\'', ..] => {
                word.push(char::from(*quoted));
                rest = &rest[3..];
            }
            _ => return Some((word, rest)),
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_parameters_are_read_as_git_writes_them() {
        let parse = |text: &str| config_parameters(text).unwrap();
        assert_eq!(parse("'a.b'='1' 'c.d'='x y'"), ["a.b=1", "c.d=x y"]);
        assert_eq!(parse("'a.b=1'"), ["a.b=1"]);
        assert_eq!(parse("'a.b'= 'c.d'"), ["a.b", "c.d"]);
        assert_eq!(parse(r"'it'\''s.x'='a'\!'b'"), ["it's.x=a!b"]);
        assert_eq!(parse("  "), Vec::<String>::new());
    }

    #[test]
    fn bogus_config_parameters_are_refused() {
        for text in ["a.b=1", "'a.b", "'a.b'x", "'a.b'='1'x", "'a.b'=1"] {
            assert!(config_parameters(text).is_none(), "{text}");
        }
    }
}
