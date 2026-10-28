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

//! `git-sprig` — plant git repositories and submodules as plain files, without their histories.

// Keep program code from growing panics, silent exits or stray output; unit
// tests are exempt through clippy.toml.
#![warn(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::exit,
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::dbg_macro,
    clippy::todo,
    clippy::allow_attributes,
    clippy::allow_attributes_without_reason
)]

#[cfg(not(unix))]
compile_error!("git-sprig relies on Unix filesystem semantics and supports Unix only");

mod app;
mod cancel;
mod cli;
mod config;
mod error;
mod fetch;
mod fs;
mod index;
mod name;
mod report;
mod schedule;
mod snapshot;
mod submodule;
mod tree;

use std::{io::Write, process::ExitCode};

use anyhow::Result;
use clap::CommandFactory;

use crate::{cancel::CancelToken, cli::Args, config::Action, report::Reporter};

/// Exit statuses as git uses them: 128 after `fatal:`, 129 for a wrong command line.
const EXIT_FATAL: u8 = 128;
const EXIT_USAGE: u8 = 129;
const EXIT_INTERRUPTED: u8 = 130;

fn main() -> ExitCode {
    let args = match Args::try_parse() {
        Ok(args) => args,
        // `--help` and `--version` end up here too, and are no failure.
        Err(err) => {
            let status = if err.use_stderr() { EXIT_USAGE } else { 0 };
            err.print().ok();
            return ExitCode::from(status);
        }
    };
    let config = match args.into_action() {
        Action::GenerateMan => return generated(cli::generate::man(Args::command())),
        Action::GenerateCompletions(shell) => {
            return generated(Ok(cli::generate::completions(shell, Args::command())));
        }
        Action::Plant(config) => config,
    };

    // Temporary repositories and staging directories are dropped before `run` returns.
    let reporter = Reporter::stderr(config.verbosity);
    let cancel = CancelToken::new();
    match app::run(&config, &reporter, &cancel) {
        Ok(()) => ExitCode::SUCCESS,
        Err(_) if cancel.by_user() => {
            // The watchdog may be racing to exit too; only one side prints.
            if cancel.claim_exit() {
                report::fatal(format_args!("interrupted"));
            }
            ExitCode::from(EXIT_INTERRUPTED)
        }
        Err(err) => fatal(&err),
    }
}

fn generated(text: Result<Vec<u8>>) -> ExitCode {
    match text.and_then(|text| print_generated(&text)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => fatal(&err),
    }
}

fn fatal(err: &anyhow::Error) -> ExitCode {
    report::fatal(format_args!("{}", report::causes(err)));
    ExitCode::from(EXIT_FATAL)
}

/// Generated in memory first: a reader that stops early, like `| head`, is not an error.
fn print_generated(text: &[u8]) -> Result<()> {
    let mut stdout = std::io::stdout().lock();
    match stdout.write_all(text).and_then(|()| stdout.flush()) {
        Err(err) if err.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        written => Ok(written?),
    }
}
