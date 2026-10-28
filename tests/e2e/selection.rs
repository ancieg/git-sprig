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

    for (index, extra) in [
        vec!["--exclude", "vendor/*"],
        vec!["--exclude", "vendor/child", "--exclude", "vendor/grand"],
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
