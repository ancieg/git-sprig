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

//! Git-style output on stderr: plain sentences with quoted paths, and
//! `warning:`, `hint:` or `fatal:` in front of what needs attention.

use std::{
    fmt::{self, Write as _},
    io::Write as _,
    sync::{Arc, Mutex, PoisonError},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Verbosity {
    Quiet,
    Normal,
    Verbose,
}

/// Where the messages go, and how much to show. Cheap to clone (shares one
/// destination) and safe to share across the worker threads. Created once and
/// passed down, rather than read from a global.
#[derive(Clone)]
pub struct Reporter {
    verbosity: Verbosity,
    to: Destination,
}

#[derive(Clone)]
enum Destination {
    Stderr,
    /// Collected in memory, so a test can read back what was printed.
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "constructed only by Reporter::buffered, under cfg(test)"
        )
    )]
    Buffer(Arc<Mutex<String>>),
}

impl Reporter {
    pub fn stderr(verbosity: Verbosity) -> Self {
        Self {
            verbosity,
            to: Destination::Stderr,
        }
    }

    fn emit(&self, level: Verbosity, prefix: &str, message: fmt::Arguments<'_>) {
        if self.verbosity < level {
            return;
        }
        match &self.to {
            // One write to the locked handle, so parallel jobs never interleave.
            Destination::Stderr => {
                writeln!(std::io::stderr().lock(), "{prefix}{message}").ok();
            }
            Destination::Buffer(buffer) => {
                let mut buffer = buffer.lock().unwrap_or_else(PoisonError::into_inner);
                writeln!(buffer, "{prefix}{message}").ok();
            }
        }
    }

    pub fn status(&self, message: fmt::Arguments<'_>) {
        self.emit(Verbosity::Normal, "", message);
    }

    pub fn detail(&self, message: fmt::Arguments<'_>) {
        self.emit(Verbosity::Verbose, "", message);
    }

    pub fn warning(&self, message: fmt::Arguments<'_>) {
        self.emit(Verbosity::Normal, "warning: ", message);
    }

    pub fn hint(&self, message: fmt::Arguments<'_>) {
        self.emit(Verbosity::Normal, "hint: ", message);
    }
}

/// `(n/total)`, where the total grows as nested submodules are discovered.
#[derive(Debug, Clone, Copy)]
pub struct Counter {
    pub number: usize,
    pub total: usize,
}

impl fmt::Display for Counter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "({}/{})", self.number, self.total)
    }
}

pub fn plural(count: usize, noun: &str) -> String {
    if count == 1 {
        format!("{count} {noun}")
    } else {
        format!("{count} {noun}s")
    }
}

/// A value in single quotes, the way git quotes paths and names. Control
/// characters are escaped, so a crafted name cannot drive the terminal.
pub struct Quoted<T>(T);

pub fn quoted<T: fmt::Display>(value: T) -> Quoted<T> {
    Quoted(value)
}

impl<T: fmt::Display> fmt::Display for Quoted<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Forward the inner value's Display through an escaping adapter, so
        // nothing is buffered into an intermediate String.
        f.write_char('\'')?;
        write!(Escaping(f), "{}", self.0)?;
        f.write_char('\'')
    }
}

/// A [`fmt::Write`] that escapes control characters as it forwards to another.
struct Escaping<'a, 'b>(&'a mut fmt::Formatter<'b>);

impl fmt::Write for Escaping<'_, '_> {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        for c in text.chars() {
            if c.is_control() {
                write!(self.0, "{}", c.escape_default())?;
            } else {
                self.0.write_char(c)?;
            }
        }
        Ok(())
    }
}

/// What git-sprig was trying to do, then what finally went wrong.  Causes
/// between the two are left out: gix stacks its own layers on an error, and
/// they restate one end or the other, often naming the same path or revision
/// a third time.  Each cause is formatted with `{:#}`, which keeps gix from
/// appending the place in its sources where the error was raised.
pub struct Causes<'a>(&'a anyhow::Error);

pub fn causes(err: &anyhow::Error) -> Causes<'_> {
    Causes(err)
}

impl fmt::Display for Causes<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let last = self.0.chain().count().saturating_sub(1);
        for (index, cause) in self.0.chain().enumerate() {
            if index != 0 {
                if index != last {
                    continue;
                }
                f.write_str(": ")?;
            }
            write!(f, "{cause:#}")?;
        }
        Ok(())
    }
}

/// Printed even with `--quiet`, right before exiting. On stderr, without a
/// reporter, so the signal thread can use it as it winds the process down.
pub fn fatal(message: fmt::Arguments<'_>) {
    writeln!(std::io::stderr().lock(), "fatal: {message}").ok();
}

macro_rules! status {
    ($reporter:expr, $($arg:tt)*) => {
        $reporter.status(format_args!($($arg)*))
    };
}

macro_rules! detail {
    ($reporter:expr, $($arg:tt)*) => {
        $reporter.detail(format_args!($($arg)*))
    };
}

macro_rules! warning {
    ($reporter:expr, $($arg:tt)*) => {
        $reporter.warning(format_args!($($arg)*))
    };
}

macro_rules! hint {
    ($reporter:expr, $($arg:tt)*) => {
        $reporter.hint(format_args!($($arg)*))
    };
}

pub(crate) use {detail, hint, status, warning};

#[cfg(test)]
impl Reporter {
    /// A reporter that collects its output for a test to read back.
    pub fn buffered(verbosity: Verbosity) -> Self {
        Self {
            verbosity,
            to: Destination::Buffer(Arc::default()),
        }
    }

    pub fn captured(&self) -> String {
        match &self.to {
            Destination::Buffer(buffer) => buffer
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone(),
            Destination::Stderr => String::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verbosity_gates_what_is_shown() {
        let normal = Reporter::buffered(Verbosity::Normal);
        status!(normal, "shown");
        detail!(normal, "hidden unless verbose");
        warning!(normal, "careful");
        hint!(normal, "try this");
        assert_eq!(
            normal.captured(),
            "shown\nwarning: careful\nhint: try this\n"
        );

        let verbose = Reporter::buffered(Verbosity::Verbose);
        detail!(verbose, "now shown");
        assert_eq!(verbose.captured(), "now shown\n");

        let quiet = Reporter::buffered(Verbosity::Quiet);
        status!(quiet, "nothing");
        warning!(quiet, "not even warnings");
        assert_eq!(quiet.captured(), "");
    }

    #[test]
    fn counters_look_like_git_progress() {
        let counter = |number, total| Counter { number, total }.to_string();
        assert_eq!(counter(1, 1), "(1/1)");
        assert_eq!(counter(9, 10), "(9/10)");
    }

    #[test]
    fn nouns_are_pluralized() {
        assert_eq!(plural(1, "file"), "1 file");
        assert_eq!(plural(0, "file"), "0 files");
        assert_eq!(plural(7, "submodule"), "7 submodules");
    }

    #[test]
    fn causes_are_shown_without_source_locations() {
        #[derive(Debug)]
        struct Located;

        impl fmt::Display for Located {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("not found")?;
                if !f.alternate() {
                    f.write_str(", at src/lib.rs:1")?;
                }
                Ok(())
            }
        }

        impl std::error::Error for Located {}

        let err = anyhow::Error::new(Located).context("cannot resolve revision 'x'");
        assert_eq!(
            causes(&err).to_string(),
            "cannot resolve revision 'x': not found"
        );
    }

    #[test]
    fn quoting_escapes_control_characters() {
        assert_eq!(quoted("vendor/lib foo").to_string(), "'vendor/lib foo'");
        assert_eq!(quoted("a\u{1b}[31mb\n").to_string(), "'a\\u{1b}[31mb\\n'");
    }
}
