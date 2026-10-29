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

use std::path::Path;

use crate::common::*;

fn two_top_levels() -> Fixture {
    let fx = Fixture::new();
    let sup = fx.superproject();
    add_submodule(&sup, &fx.path("remotes/grand"), "vendor/grand");
    commit_all(&sup, "second top-level submodule");
    fx
}

fn assert_selected_child(out: &Path) {
    assert_eq!(read(&out.join("vendor/child/hello.txt")), "child\n");
    assert_eq!(
        read(&out.join("vendor/child/third_party/grand/g.txt")),
        "grandchild\n"
    );
    assert!(!out.join("vendor/grand/g.txt").exists());
}

#[test]
fn includes_only_matching_top_level_submodules() {
    let fx = two_top_levels();
    let sup = fx.superproject();

    let legacy = fx.path("legacy");
    plant(&sup, &legacy, &["--", "vendor/child"]).assert_ok();
    assert_selected_child(&legacy);

    let direct_out = fx.path("direct");
    direct(
        sup.to_str().unwrap(),
        &direct_out,
        "HEAD",
        &["--", "vendor/child"],
    )
    .assert_ok();
    assert_selected_child(&direct_out);
    assert_eq!(read(&direct_out.join("super.txt")), "superproject\n");

    let dry_out = fx.path("dry");
    let dry = plant(&sup, &dry_out, &["--dry-run", "--", "vendor/child"]);
    dry.assert_ok();
    dry.assert_says("Would fetch submodule 'vendor/child'");
    dry.assert_silent_about("Would fetch submodule 'vendor/grand'");
    assert!(!dry_out.exists());
}

#[test]
fn excludes_top_level_submodules_with_paths_globs_and_magic() {
    let fx = two_top_levels();
    let sup = fx.superproject();

    let direct_out = fx.path("direct-exclude");
    direct(
        sup.to_str().unwrap(),
        &direct_out,
        "HEAD",
        &["--exclude", "vendor/grand"],
    )
    .assert_ok();
    assert_selected_child(&direct_out);

    let magic = fx.path("magic");
    plant(&sup, &magic, &["--", ":(exclude)vendor/grand"]).assert_ok();
    assert_selected_child(&magic);

    let excluded_magic = fx.path("excluded-magic");
    plant(&sup, &excluded_magic, &["--exclude", ":(glob)vendor/gr*"]).assert_ok();
    assert_selected_child(&excluded_magic);

    for (index, extra) in [
        vec!["--exclude", "vendor/*"],
        vec!["--exclude", "vendor/child", "--exclude", "vendor/grand"],
        vec!["--exclude", ":(glob)vendor/*"],
        vec!["--exclude", ":/vendor/child", "--exclude", ":!vendor/grand"],
    ]
    .into_iter()
    .enumerate()
    {
        let out = fx.path(&format!("exclude-all-{index}"));
        direct(sup.to_str().unwrap(), &out, "HEAD", &extra).assert_ok();
        assert_eq!(read(&out.join("super.txt")), "superproject\n");
        assert!(!out.join("vendor/child/hello.txt").exists());
        assert!(!out.join("vendor/grand/g.txt").exists());
    }

    let no_match = fx.path("no-match");
    let run = plant(&sup, &no_match, &["--", "missing/path"]);
    run.assert_ok();
    run.assert_silent_about("Fetching submodule");
    assert_eq!(std::fs::read_dir(&no_match).unwrap().count(), 0);
}

/// A clone of `two_top_levels` with empty submodule folders and a `docs`
/// directory, as `git clone` without `--recurse-submodules` leaves it.
fn fresh_clone(fx: &Fixture, name: &str) -> std::path::PathBuf {
    let sup = fx.superproject();
    if !sup.join("docs").exists() {
        write(&sup, "docs/readme.txt", "docs\n");
        commit_all(&sup, "docs");
    }
    let clone = fx.path(name);
    git(
        fx.root.path(),
        &[
            "clone",
            "-q",
            sup.to_str().unwrap(),
            clone.to_str().unwrap(),
        ],
    );
    clone
}

#[test]
fn runs_from_a_subdirectory_like_git() {
    let fx = two_top_levels();

    // Submodules land at the top of the work tree, not below where it runs.
    let clone = fresh_clone(&fx, "everything");
    run_in(&clone.join("docs"), &[]).assert_ok();
    assert_eq!(read(&clone.join("vendor/child/hello.txt")), "child\n");
    assert_eq!(read(&clone.join("vendor/grand/g.txt")), "grandchild\n");
    assert_eq!(std::fs::read_dir(clone.join("docs")).unwrap().count(), 1);

    // Pathspecs are relative to where it runs, as in git.
    let clone = fresh_clone(&fx, "relative");
    run_in(&clone.join("vendor"), &["--", "child"]).assert_ok();
    assert_selected_child(&clone);
    let clone = fresh_clone(&fx, "parent");
    run_in(&clone.join("docs"), &["--exclude", "../vendor/grand"]).assert_ok();
    assert_selected_child(&clone);
}

#[test]
fn a_bare_superproject_needs_a_directory_to_write_into() {
    let fx = Fixture::new();
    let bare = fx.path("bare.git");
    let sup = fx.superproject();
    git(
        fx.root.path(),
        &[
            "clone",
            "-q",
            "--bare",
            sup.to_str().unwrap(),
            bare.to_str().unwrap(),
        ],
    );
    let run = run_in(&bare, &[]);
    run.assert_failed();
    run.assert_says("bare repository");
    assert_eq!(leftovers(&bare), Vec::<String>::new());
    assert!(!bare.join("vendor").exists());

    plant(&bare, &fx.path("out"), &[]).assert_ok();
    assert_eq!(read(&fx.path("out/vendor/child/hello.txt")), "child\n");
}

#[test]
fn selection_is_not_applied_inside_included_submodules() {
    let fx = two_top_levels();
    let sup = fx.superproject();
    let out = fx.path("out");

    direct(
        sup.to_str().unwrap(),
        &out,
        "HEAD",
        &["--exclude", "third_party/grand"],
    )
    .assert_ok();
    assert_eq!(read(&out.join("vendor/child/hello.txt")), "child\n");
    assert_eq!(
        read(&out.join("vendor/child/third_party/grand/g.txt")),
        "grandchild\n"
    );
    assert_eq!(read(&out.join("vendor/grand/g.txt")), "grandchild\n");
}

#[test]
fn stage_replaces_only_selected_gitlinks() {
    let fx = two_top_levels();
    let sup = fx.superproject();
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

    plant(&clone, &clone, &["--stage", "--exclude", "vendor/grand"]).assert_ok();
    assert_eq!(read(&clone.join("vendor/child/hello.txt")), "child\n");
    assert!(git(&clone, &["ls-files", "--stage", "vendor/grand"]).starts_with("160000"));
    let modules = read(&clone.join(".gitmodules"));
    assert!(modules.contains("vendor/grand"), "{modules}");
    assert!(!modules.contains("vendor/child"), "{modules}");
}

#[test]
fn invalid_pathspecs_are_rejected_before_fetching() {
    let root = tempfile::tempdir().unwrap();
    let out = root.path().join("out");
    let missing = format!("file://{}", root.path().join("missing").display());
    for args in [
        vec![&missing, out.to_str().unwrap(), "--exclude", ":(bad"],
        vec![&missing, out.to_str().unwrap(), "--", ":(bad"],
    ] {
        let run = run(&args);
        run.assert_code(129);
        run.assert_silent_about("cannot fetch");
        assert!(!out.exists());
    }
}
