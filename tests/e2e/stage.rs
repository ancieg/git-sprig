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

use crate::common::*;

/// A fresh clone of the fixture's superproject: submodules not checked out, and
/// a second submodule `vendor/grand` that is marked inactive.
fn clone_with_inactive_submodule(fx: &Fixture) -> std::path::PathBuf {
    let sup = fx.superproject();
    add_submodule(&sup, &fx.path("remotes/grand"), "vendor/grand");
    commit_all(&sup, "second submodule");
    let clone = fx.path("clone");
    git(
        fx.root.path(),
        &[
            "clone",
            "-q",
            sup.to_str().unwrap(),
            clone.to_str().unwrap(),
        ],
    );
    git(
        &clone,
        &["config", "submodule.vendor/grand.active", "false"],
    );
    clone
}

#[test]
fn stage_turns_submodules_into_files_ready_to_commit() {
    let fx = Fixture::new();
    let clone = clone_with_inactive_submodule(&fx);

    let dry = plant(&clone, &clone, &["--stage", "--dry-run"]);
    dry.assert_ok();
    assert!(
        dry.stderr
            .contains("Would stage submodule path 'vendor/child' as plain files"),
        "{}",
        dry.stderr
    );
    assert_eq!(
        git(&clone, &["status", "--porcelain"]),
        "",
        "a dry run staged something"
    );

    let run = plant(&clone, &clone, &["--stage"]);
    run.assert_ok();
    assert!(
        run.stderr
            .contains("Submodule path 'vendor/child': staged as plain files"),
        "{}",
        run.stderr
    );
    git(&clone, &["commit", "-q", "-m", "vendor child"]);

    let tree = git(
        &clone,
        &["ls-tree", "-r", "HEAD", "--format=%(objectmode) %(path)"],
    );
    for expected in [
        "100644 vendor/child/hello.txt",
        "100755 vendor/child/bin/run.sh",
        "120000 vendor/child/link",
        "100644 vendor/child/third_party/grand/g.txt",
        "160000 vendor/grand",
    ] {
        assert!(
            tree.lines().any(|line| line == expected),
            "no {expected:?} in\n{tree}"
        );
    }
    assert!(!tree.contains("160000 vendor/child"), "{tree}");
    // The committed blob has the LF the child repository stores, not the
    // CRLF its attributes put into the work tree.
    assert_eq!(
        git(&clone, &["show", "HEAD:vendor/child/crlf.txt"]),
        "dos\nlines"
    );

    let gitmodules = read(&clone.join(".gitmodules"));
    assert!(!gitmodules.contains("vendor/child"), "{gitmodules}");
    assert!(gitmodules.contains("vendor/grand"), "{gitmodules}");
    assert_eq!(
        git(&clone, &["status", "--porcelain"]),
        "",
        "not everything was committed"
    );
}

#[test]
fn stage_removes_gitmodules_once_no_submodule_is_left() {
    let fx = Fixture::new();
    let clone = fx.path("clone");
    git(
        fx.root.path(),
        &[
            "clone",
            "-q",
            fx.superproject().to_str().unwrap(),
            clone.to_str().unwrap(),
        ],
    );

    let run = plant(&clone, &clone, &["--stage"]);
    run.assert_ok();
    assert!(!clone.join(".gitmodules").exists());
    let status = git(&clone, &["status", "--porcelain"]);
    assert!(
        status.lines().any(|line| line == "D  .gitmodules"),
        "{status}"
    );
    assert!(
        status
            .lines()
            .any(|line| line == "A  vendor/child/hello.txt"),
        "{status}"
    );
}

/// Submodules are named after their paths unless told otherwise, and a name may
/// hold dots: staging `vendor/child` must leave `vendor/child.old` in place.
#[test]
fn stage_keeps_a_submodule_whose_name_extends_a_staged_one() {
    let fx = Fixture::new();
    let sup = fx.superproject();
    add_submodule(&sup, &fx.path("remotes/grand"), "vendor/child.old");
    commit_all(&sup, "a submodule whose name extends another's");
    let clone = fx.path("clone");
    git(
        fx.root.path(),
        &[
            "clone",
            "-q",
            sup.to_str().unwrap(),
            clone.to_str().unwrap(),
        ],
    );
    git(
        &clone,
        &["config", "submodule.vendor/child.old.active", "false"],
    );

    plant(&clone, &clone, &["--stage"]).assert_ok();

    assert!(
        clone.join(".gitmodules").exists(),
        ".gitmodules was removed while vendor/child.old still needs it"
    );
    let keys = git(
        &clone,
        &["config", "--file", ".gitmodules", "--name-only", "--list"],
    );
    assert!(
        keys.lines()
            .any(|key| key == "submodule.vendor/child.old.path"),
        "{keys}"
    );
    assert!(
        !keys.lines().any(|key| key == "submodule.vendor/child.path"),
        "{keys}"
    );
}

#[test]
fn stage_needs_the_work_tree_as_basedir() {
    let fx = Fixture::new();
    let clone = clone_with_inactive_submodule(&fx);
    let out = fx.path("out");

    let run = plant(&clone, &out, &["--stage"]);
    run.assert_failed();
    run.assert_says("--work-tree");
    assert!(!out.exists(), "fetched before refusing");
}
