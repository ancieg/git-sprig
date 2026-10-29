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

//! One run of git-sprig: change directory as `-C` asks, then fill in the
//! submodules of a superproject or export a repository whole, with what both
//! modes share.

mod direct;
mod superproject;

use std::num::NonZeroUsize;

use anyhow::{Context, Result, anyhow};

use crate::{
    cancel::CancelToken,
    config::{Config, IndexUpdate, Source},
    report::{self, Reporter},
    schedule::{self, Recursion},
    submodule::{gitmodules::Scope, plan::Planned},
};

pub(crate) fn run(config: &Config, reporter: &Reporter, cancel: &CancelToken) -> Result<()> {
    cancel.install_handler()?;
    // Like `git -C`, so every relative path given is taken from there.
    for dir in &config.chdir {
        std::env::set_current_dir(dir)
            .with_context(|| format!("cannot change to {}", report::quoted(dir.display())))?;
    }
    match &config.source {
        Source::Superproject(superproject) => {
            superproject::run(config, superproject, reporter, cancel)
        }
        Source::Direct(direct) => direct::run(config, direct, reporter, cancel),
    }
}

fn schedule_options(config: &Config, jobs: NonZeroUsize) -> schedule::Options {
    // Past eight threads, writing gets no faster.
    schedule::Options {
        conversion: config.conversion,
        recursion: config.recursion,
        jobs,
        write_threads: available_cpus().min(8),
    }
}

/// How many submodules to fetch at once: -j, else `submodule.fetchJobs`, as
/// git reads them, where 0 asks for the default. Fetches mostly wait on the
/// network, so by default one runs per CPU; the cap of eight keeps a run from
/// hammering one server. Like git, a setting that is no count is an error.
fn jobs(given: Option<u16>, config: &gix::config::File, scope: Scope) -> Result<NonZeroUsize> {
    let wanted = match given {
        Some(given) => usize::from(given),
        None => match config
            .integer_filter("submodule.fetchJobs", |meta| scope.admits(meta))
            .context("bad numeric config value for submodule.fetchJobs")?
        {
            Some(jobs) => usize::try_from(jobs).map_err(|_| {
                anyhow!("negative values are not allowed for submodule.fetchJobs: {jobs}")
            })?,
            None => 0,
        },
    };
    Ok(NonZeroUsize::new(wanted)
        .or_else(|| NonZeroUsize::new(available_cpus().min(8)))
        .unwrap_or(NonZeroUsize::MIN))
}

fn report_dry_run(
    planned: &[Planned],
    index: IndexUpdate,
    recursion: Recursion,
    reporter: &Reporter,
) {
    for planned in planned {
        report::status!(
            reporter,
            "Would fetch submodule {} from {} at {}",
            report::quoted(planned.path.display()),
            report::quoted(&planned.url),
            report::quoted(planned.commit)
        );
    }
    if index == IndexUpdate::Stage {
        for planned in planned {
            report::status!(
                reporter,
                "Would stage submodule path {} as plain files",
                report::quoted(planned.path.display())
            );
        }
    }
    if !matches!(recursion, Recursion::Off) && !planned.is_empty() {
        report::hint!(
            reporter,
            "nested submodules are only found once their parents are fetched"
        );
    }
}

fn available_cpus() -> usize {
    std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jobs_follow_dash_j_then_fetch_jobs_as_in_git() {
        use gix::config::Source::{Cli, Local};

        let config = |text: &str, source| {
            let mut text = text.as_bytes().to_vec();
            gix::config::File::from_bytes_owned(
                &mut text,
                gix::config::file::Metadata::from(source),
                gix::config::file::init::Options::default(),
            )
            .unwrap()
        };
        let n = |n| NonZeroUsize::new(n).unwrap();
        let default = n(available_cpus().min(8));
        let unset = config("", Local);
        let local = config("[submodule]\n\tfetchJobs = 2\n", Local);
        let given = config("[submodule]\n\tfetchJobs = 3\n", Cli);
        let zero = config("[submodule]\n\tfetchJobs = 0\n", Cli);

        assert_eq!(jobs(Some(5), &local, Scope::Repository).unwrap(), n(5));
        assert_eq!(jobs(Some(0), &local, Scope::Repository).unwrap(), default);
        assert_eq!(jobs(None, &local, Scope::Repository).unwrap(), n(2));
        assert_eq!(jobs(None, &local, Scope::Clone).unwrap(), default);
        assert_eq!(jobs(None, &given, Scope::Clone).unwrap(), n(3));
        assert_eq!(jobs(None, &zero, Scope::Clone).unwrap(), default);
        assert_eq!(jobs(None, &unset, Scope::Repository).unwrap(), default);

        // As in git, a setting that is no count is an error, unless -j decides.
        let words = config("[submodule]\n\tfetchJobs = lots\n", Cli);
        let negative = config("[submodule]\n\tfetchJobs = -1\n", Cli);
        assert!(jobs(None, &words, Scope::Clone).is_err());
        assert!(jobs(None, &negative, Scope::Clone).is_err());
        assert_eq!(jobs(Some(4), &words, Scope::Clone).unwrap(), n(4));
    }
}
