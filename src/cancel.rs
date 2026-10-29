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

//! Stopping early, on Ctrl-C or after a failure. A [`CancelToken`] holds the
//! interrupt flag that running work checks, the "stopped by the user" flag, and
//! the directories to remove if the process has to exit before work winds down.
//! It is created once in `main` and shared with everything that can be
//! cancelled.

use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError},
    },
    thread,
    time::Duration,
};

use anyhow::{Context, Result};
use signal_hook::{consts::TERM_SIGNALS, iterator::Signals};

use crate::report;

/// How long work gets to wind down after Ctrl-C.
const GRACE: Duration = Duration::from_secs(3);

/// A shared handle to cancellation. Cloning shares the same state, so the signal
/// thread, the coordinator and the workers all see one another's flags.
#[derive(Clone, Default)]
pub struct CancelToken(Arc<State>);

#[derive(Default)]
struct State {
    /// Set once work should stop; the flag gix checks while receiving a pack.
    interrupted: AtomicBool,
    /// Whether the stop came from the user (Ctrl-C), not a failure.
    by_user: AtomicBool,
    /// Held by whoever is ending the process, so it happens once.
    exiting: AtomicBool,
    /// Directories to remove if git-sprig has to exit before its work winds down.
    leftovers: Mutex<Vec<PathBuf>>,
}

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }

    /// Watch for termination signals on a thread, using signal-hook's safe API.
    /// The first signal asks work to wind down; a second one, or the grace
    /// period elapsing, cleans up and exits for work that could not notice.
    pub fn install_handler(&self) -> Result<()> {
        let mut signals = Signals::new(TERM_SIGNALS).context("cannot install a signal handler")?;
        let (tx, rx) = mpsc::channel();
        thread::Builder::new()
            .name("signals".into())
            .spawn(move || {
                for _ in &mut signals {
                    if tx.send(()).is_err() {
                        break;
                    }
                }
            })
            .context("cannot start the signal thread")?;
        let token = self.clone();
        thread::Builder::new()
            .name("interrupt".into())
            .spawn(move || token.watchdog(&rx))
            .context("cannot start the interrupt watchdog")?;
        Ok(())
    }

    /// Some work cannot notice Ctrl-C, such as a read from a server that went
    /// silent. If it has not wound down in time, clean up for it and exit.
    fn watchdog(&self, signals: &Receiver<()>) {
        if signals.recv().is_err() {
            return;
        }
        self.0.by_user.store(true, Ordering::SeqCst);
        self.request();
        // A second signal cuts the grace period short; a disconnect only happens
        // as the process is already ending, so treat it like the period elapsing.
        match signals.recv_timeout(GRACE) {
            Ok(()) | Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => {}
        }
        let leftovers = std::mem::take(
            &mut *self
                .0
                .leftovers
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        );
        for dir in leftovers {
            std::fs::remove_dir_all(dir).ok();
        }
        self.exit_interrupted();
    }

    /// Claim the sole right to print the interruption notice and end the process.
    /// Returns `false` to a later caller, which must not print or exit again.
    pub fn claim_exit(&self) -> bool {
        !self.0.exiting.swap(true, Ordering::SeqCst)
    }

    /// Exit with 130, the status a shell reports for git stopped by Ctrl-C.
    ///
    /// The one place that ends the process itself: the watchdog reaches it when
    /// work could not wind down, so there is nothing left to unwind. Everywhere
    /// else `main` returns an `ExitCode` instead.
    pub fn exit_interrupted(&self) -> ! {
        if !self.claim_exit() {
            // Another caller is already exiting; `exit` must not run twice at once.
            loop {
                thread::park();
            }
        }
        report::fatal(format_args!("interrupted"));
        std::process::exit(130);
    }

    /// Remove `dir` if the process is forced to exit; see [`Self::forget`].
    pub fn register(&self, dir: &Path) {
        self.0
            .leftovers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(dir.to_owned());
    }

    /// Stop tracking `dir`, once it has been cleaned up in the ordinary way.
    pub fn forget(&self, dir: &Path) {
        self.0
            .leftovers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|known| known != dir);
    }

    pub fn request(&self) {
        self.0.interrupted.store(true, Ordering::SeqCst);
    }

    pub fn requested(&self) -> bool {
        self.0.interrupted.load(Ordering::SeqCst)
    }

    /// The flag to hand to gix, which checks it while receiving a pack.
    pub fn flag(&self) -> &AtomicBool {
        &self.0.interrupted
    }

    pub fn by_user(&self) -> bool {
        self.0.by_user.load(Ordering::SeqCst)
    }
}
