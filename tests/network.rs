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

//! Runs only with `cargo test -- --ignored`.

mod common;

use common::*;

/// `astral-sh/ty` pins `astral-sh/ruff`; the result must match what git checks out.
#[test]
#[ignore = "needs network access to github.com"]
fn matches_git_on_astral_sh_ty() {
    let root = tempfile::tempdir().unwrap();
    let ty = root.path().join("ty");
    let out = root.path().join("out");

    git(
        root.path(),
        &[
            "clone",
            "-q",
            "--depth",
            "1",
            "https://github.com/astral-sh/ty.git",
            ty.to_str().unwrap(),
        ],
    );

    let run = plant(&ty, &out, &[]);
    assert!(run.ok, "{}", run.stderr);
    assert!(!run.stderr.contains("falling back"), "{}", run.stderr);

    git(
        &ty,
        &["submodule", "update", "--init", "--depth", "1", "-q"],
    );

    let ours = tree_listing(&out.join("ruff"));
    assert!(!ours.is_empty(), "nothing was written");
    assert_eq!(ours, tree_listing(&ty.join("ruff")));
    assert!(!out.join("ruff/.git").exists());
}
