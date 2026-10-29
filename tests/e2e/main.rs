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

//! End-to-end tests, one binary split by topic. Shared fixtures live in
//! `common`; `set_child_url` is here because both planting and fetching use it.

#[path = "../common/mod.rs"]
mod common;

mod attributes;
mod cli;
mod direct;
mod fetching;
mod hostile;
mod interrupts;
mod planting;
mod selection;
mod stage;

use std::path::Path;

use common::{commit_all, git};

pub(crate) fn set_child_url(sup: &Path, url: &str) {
    git(
        sup,
        &[
            "config",
            "--file",
            ".gitmodules",
            "submodule.vendor/child.url",
            url,
        ],
    );
    commit_all(sup, "change the submodule url");
    // Drop what `git submodule add` configured, which would win over
    // `.gitmodules` as in git, where `git submodule sync` updates it.
    git(sup, &["config", "--unset", "submodule.vendor/child.url"]);
}
