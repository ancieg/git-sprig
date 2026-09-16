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

//! Typed errors that a caller acts on, rather than matching a message string.

use std::fmt;

/// Work was stopped, by Ctrl-C or after another job failed. Carried as an error
/// so a stopped step unwinds like any other, without a stringly-typed sentinel.
#[derive(Debug)]
pub struct Interrupted;

impl fmt::Display for Interrupted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("interrupted")
    }
}

impl std::error::Error for Interrupted {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interrupted_reads_as_one_word() {
        assert_eq!(Interrupted.to_string(), "interrupted");
        // Usable anywhere an anyhow error is expected.
        let err: anyhow::Error = Interrupted.into();
        assert_eq!(err.to_string(), "interrupted");
    }
}
