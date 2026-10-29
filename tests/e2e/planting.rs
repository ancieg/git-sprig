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
use crate::set_child_url;

#[test]
fn materializes_nested_submodules_as_plain_files() {
    let fx = Fixture::new();
    let out = fx.path("out");
    let run = plant(&fx.superproject(), &out, &[]);
    run.assert_ok();

    let lines: Vec<&str> = run.stderr.lines().collect();
    let fetching = |path: &str, counter: &str| {
        lines.iter().any(|line| {
            line.starts_with(&format!("Fetching submodule '{path}' from '"))
                && line.ends_with(&format!("' {counter}"))
        })
    };
    // The nested submodule only counts once its parent has been written.
    assert!(fetching("vendor/child", "(1/1)"), "{}", run.stderr);
    assert!(
        fetching("vendor/child/third_party/grand", "(2/2)"),
        "{}",
        run.stderr
    );

    let child_commit = git(&fx.superproject(), &["rev-parse", "HEAD:vendor/child"]);
    let grand_commit = git(
        &fx.path("remotes/child"),
        &[
            "rev-parse",
            &format!("{}:third_party/grand", child_commit.trim()),
        ],
    );
    for (path, commit) in [
        ("vendor/child", &child_commit),
        ("vendor/child/third_party/grand", &grand_commit),
    ] {
        let planted = format!("Submodule path '{path}': planted '{}'", commit.trim());
        assert!(lines.contains(&planted.as_str()), "{}", run.stderr);
    }
    run.assert_silent_about("falling back");

    let child = out.join("vendor/child");
    assert_eq!(read(&child.join("hello.txt")), "child\n");
    assert_eq!(read(&child.join("deep/nested/file.txt")), "nested\n");
    assert!(is_executable(&child.join("bin/run.sh")));
    assert!(!is_executable(&child.join("hello.txt")));
    assert_eq!(
        std::fs::read_link(child.join("link")).unwrap(),
        Path::new("hello.txt")
    );
    assert_eq!(read(&child.join("third_party/grand/g.txt")), "grandchild\n");

    // Pinned commits, not branch tips.
    assert!(!child.join("future.txt").exists());
    assert!(!child.join("third_party/grand/future.txt").exists());

    assert!(!child.join(".git").exists());
    assert!(!child.join("third_party/grand/.git").exists());
    assert!(child.join(".gitmodules").exists());
    assert!(!out.join("super.txt").exists());
    assert_eq!(leftovers(&out.join("vendor")), Vec::<String>::new());
}

#[test]
fn directories_get_umask_permissions() {
    let fx = Fixture::new();
    let out = fx.path("out");
    let run = plant(&fx.superproject(), &out, &[]);
    run.assert_ok();

    let mode = |path: &Path| {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    };
    let reference = fx.path("reference");
    std::fs::create_dir(&reference).unwrap();
    assert_eq!(mode(&out.join("vendor/child")), mode(&reference));
    assert_eq!(mode(&out.join("vendor/child/deep")), mode(&reference));
}

#[test]
fn destination_is_replaced_not_merged() {
    let fx = Fixture::new();
    let out = fx.path("out");
    let child = out.join("vendor/child");
    write(&child, "stale.txt", "left over");
    write(&child, "deep/nested/file.txt", "old");

    let run = plant(&fx.superproject(), &out, &[]);
    run.assert_ok();
    assert!(!child.join("stale.txt").exists());
    assert_eq!(read(&child.join("deep/nested/file.txt")), "nested\n");
}

#[test]
fn never_writes_through_an_existing_symlink() {
    let fx = Fixture::new();
    let out = fx.path("out");
    let outside = fx.path("outside.txt");
    std::fs::write(&outside, "untouched").unwrap();
    let child = out.join("vendor/child");
    std::fs::create_dir_all(&child).unwrap();
    std::os::unix::fs::symlink(&outside, child.join("hello.txt")).unwrap();

    let run = plant(&fx.superproject(), &out, &[]);
    run.assert_ok();
    assert_eq!(read(&outside), "untouched");
    assert_eq!(read(&child.join("hello.txt")), "child\n");
}

#[test]
fn a_symlinked_destination_is_replaced_not_followed() {
    let fx = Fixture::new();
    let out = fx.path("out");
    let elsewhere = fx.path("elsewhere");
    write(&elsewhere, "mine.txt", "untouched");
    std::fs::create_dir_all(out.join("vendor")).unwrap();
    std::os::unix::fs::symlink(&elsewhere, out.join("vendor/child")).unwrap();

    let run = plant(&fx.superproject(), &out, &[]);
    run.assert_ok();
    let child = out.join("vendor/child");
    assert!(
        !std::fs::symlink_metadata(&child)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(read(&child.join("hello.txt")), "child\n");
    assert_eq!(read(&elsewhere.join("mine.txt")), "untouched");
    assert!(!elsewhere.join("hello.txt").exists());
}

#[test]
fn no_recursive_stops_at_the_first_level() {
    let fx = Fixture::new();
    let out = fx.path("out");
    let run = plant(&fx.superproject(), &out, &["--no-recursive"]);
    run.assert_ok();
    run.assert_silent_about("warning");
    assert_eq!(read(&out.join("vendor/child/hello.txt")), "child\n");
    assert!(out.join("vendor/child/third_party/grand").is_dir());
    assert!(!out.join("vendor/child/third_party/grand/g.txt").exists());
}

#[test]
fn max_depth_limits_the_nesting() {
    let fx = Fixture::new();
    let out = fx.path("out");
    let run = plant(&fx.superproject(), &out, &["--max-depth", "1"]);
    run.assert_ok();
    run.assert_says("--max-depth 1 reached");
    assert_eq!(read(&out.join("vendor/child/hello.txt")), "child\n");
    assert!(!out.join("vendor/child/third_party/grand/g.txt").exists());
}

#[test]
fn dry_run_touches_nothing() {
    let fx = Fixture::new();
    let out = fx.path("out");
    for flag in ["--dry-run", "-n"] {
        let run = plant(&fx.superproject(), &out, &[flag]);
        run.assert_ok();
        assert!(!out.exists());
        assert!(
            run.stderr
                .contains("Would fetch submodule 'vendor/child' from '"),
            "{}",
            run.stderr
        );
        assert!(
            run.stderr
                .contains("hint: nested submodules are only found once their parents are fetched"),
            "{}",
            run.stderr
        );
    }
}

#[test]
fn an_older_revision_can_be_materialized() {
    let fx = Fixture::new();
    let child = fx.path("remotes/child");
    let sup = fx.superproject();
    let out = fx.path("out");
    let old = git(&sup, &["rev-parse", "HEAD"]);

    write(&child, "hello.txt", "child v2\n");
    commit_all(&child, "child v2");
    git(
        &sup.join("vendor/child"),
        &["fetch", "-q", "origin", "main"],
    );
    git(&sup.join("vendor/child"), &["checkout", "-q", "FETCH_HEAD"]);
    commit_all(&sup, "bump");

    let run = plant(&sup, &out, &["--rev", &old]);
    run.assert_ok();
    assert_eq!(read(&out.join("vendor/child/hello.txt")), "child\n");

    let run = plant(&sup, &out, &[]);
    run.assert_ok();
    assert_eq!(read(&out.join("vendor/child/hello.txt")), "child v2\n");
}

#[test]
fn gitlink_without_a_gitmodules_entry_is_skipped() {
    let fx = Fixture::new();
    let sup = fx.superproject();
    std::fs::remove_file(sup.join(".gitmodules")).unwrap();
    commit_all(&sup, "drop .gitmodules");

    let out = fx.path("out");
    let run = plant(&sup, &out, &[]);
    run.assert_ok();
    run.assert_says("no .gitmodules entry");
    assert!(!out.join("vendor/child/hello.txt").exists());
}

/// `path =` is controlled by the remote, so only the tree decides where things go.
#[test]
fn gitmodules_path_never_decides_the_destination() {
    let fx = Fixture::new();
    let sup = fx.superproject();
    git(
        &sup,
        &[
            "config",
            "--file",
            ".gitmodules",
            "submodule.vendor/child.path",
            "elsewhere",
        ],
    );
    commit_all(&sup, "point path = somewhere else");

    let out = fx.path("out");
    let run = plant(&sup, &out, &[]);
    run.assert_ok();
    run.assert_says("no .gitmodules entry");
    assert!(!out.join("vendor/child/hello.txt").exists());
    assert!(!out.join("elsewhere").exists());
}

#[test]
fn quiet_prints_nothing_but_errors() {
    let fx = Fixture::new();
    let sup = fx.superproject();
    let out = fx.path("out");

    // As in git, the last of -q and -v wins.
    for flags in [&["-q"][..], &["-v", "-q"], &["-qq"]] {
        let run = plant(&sup, &out, flags);
        run.assert_ok();
        assert_eq!(run.stderr, "", "{flags:?}");
    }

    std::fs::remove_file(sup.join(".gitmodules")).unwrap();
    commit_all(&sup, "drop .gitmodules");
    let run = plant(&sup, &out, &["--quiet"]);
    run.assert_ok();
    assert_eq!(run.stderr, "", "a warning got through");

    let run = run_in_nowhere(&fx);
    run.assert_failed();
    assert!(run.stderr.starts_with("fatal: "), "{}", run.stderr);
    run.assert_code(128);
}

fn run_in_nowhere(fx: &Fixture) -> Run {
    run(&["-C", fx.path("nowhere").to_str().unwrap(), "--quiet"])
}

#[test]
fn verbose_says_how_each_submodule_was_fetched() {
    let fx = Fixture::new();
    let run = plant(&fx.superproject(), &fx.path("out"), &["-q", "-v", "-v"]);
    run.assert_ok();
    assert!(
        run.stderr
            .contains("Submodule path 'vendor/child': fetched directly by object id in "),
        "{}",
        run.stderr
    );
    assert!(
        run.stderr
            .contains("Submodule path 'vendor/child': wrote 7 files, "),
        "{}",
        run.stderr
    );
    assert!(
        run.stderr
            .contains("Submodule path 'vendor/child': planted '"),
        "{}",
        run.stderr
    );
}

#[test]
fn a_failed_fetch_leaves_the_destination_alone() {
    let fx = Fixture::new();
    let sup = fx.superproject();
    let out = fx.path("out");
    assert!(plant(&sup, &out, &[]).ok);
    let before = tree_listing(&out.join("vendor/child"));

    set_child_url(&sup, fx.path("remotes/gone").to_str().unwrap());
    plant(&sup, &out, &[]).assert_failed();
    assert_eq!(tree_listing(&out.join("vendor/child")), before);
    assert_eq!(leftovers(&out.join("vendor")), Vec::<String>::new());
}

#[test]
fn a_failure_below_keeps_the_whole_submodule_as_it_was() {
    let fx = Fixture::new();
    let sup = fx.superproject();
    let out = fx.path("out");
    assert!(plant(&sup, &out, &[]).ok);
    write(&out, "vendor/child/marker.txt", "from before");

    std::fs::rename(fx.path("remotes/grand"), fx.path("remotes/grand-moved")).unwrap();
    plant(&sup, &out, &[]).assert_failed();
    assert_eq!(read(&out.join("vendor/child/marker.txt")), "from before");
    assert_eq!(leftovers(&out.join("vendor")), Vec::<String>::new());
}

#[test]
fn no_temporary_directories_survive() {
    let fx = Fixture::new();
    let tmp = fx.path("tmp");
    let run = plant(
        &fx.superproject(),
        &fx.path("out"),
        &["--tempdir", tmp.to_str().unwrap()],
    );
    run.assert_ok();
    assert_eq!(std::fs::read_dir(&tmp).unwrap().count(), 0);
}

#[test]
fn refuses_to_replace_a_submodule_checkout_without_force() {
    let fx = Fixture::new();
    let sup = fx.superproject();
    let checkout = sup.join("vendor/child");
    write(&checkout, "wip.txt", "uncommitted");

    let run = plant(&sup, &sup, &[]);
    assert!(!run.ok, "a live checkout was replaced");
    run.assert_says("--force");
    assert_eq!(read(&checkout.join("wip.txt")), "uncommitted");
    assert!(checkout.join(".git").exists());

    let run = plant(&sup, &sup, &["-f"]);
    run.assert_ok();
    assert_eq!(read(&checkout.join("hello.txt")), "child\n");
    assert!(!checkout.join("wip.txt").exists());
    assert!(!checkout.join(".git").exists());
    assert!(sup.join(".git/modules/vendor/child").is_dir());
}

#[test]
fn parallel_jobs_produce_the_same_tree() {
    let fx = Fixture::new();
    let sup = fx.superproject();
    add_submodule(&sup, &fx.path("remotes/grand"), "vendor/grand");
    commit_all(&sup, "second submodule");

    let sequential = fx.path("out-1");
    let parallel = fx.path("out-4");
    for (out, jobs) in [(&sequential, "1"), (&parallel, "4")] {
        let run = plant(&sup, out, &["--jobs", jobs]);
        assert!(run.ok, "--jobs {jobs}:\n{}", run.stderr);
        assert_eq!(
            run.stderr.matches(": planted '").count(),
            3,
            "{}",
            run.stderr
        );
    }

    let listing = tree_listing(&sequential.join("vendor"));
    assert!(listing.iter().any(|line| line.starts_with("grand/g.txt ")));
    assert!(
        listing
            .iter()
            .any(|line| line.starts_with("child/third_party/grand/g.txt "))
    );
    assert_eq!(tree_listing(&parallel.join("vendor")), listing);
}

#[test]
fn exit_status_follows_git() {
    let fx = Fixture::new();
    let planted = plant(&fx.superproject(), &fx.path("out"), &["-q"]);
    planted.assert_code(0);

    let fatal = run_in_nowhere(&fx);
    fatal.assert_code(128);

    let usage: [&[&str]; 3] = [
        &["--bogus"],
        &["--jobs", "0"],
        &["--generate-man", "--stage"],
    ];
    for args in usage {
        let run = run(args);
        assert_eq!(run.code, Some(129), "{args:?}:\n{}", run.stderr);
        assert!(
            run.stderr.starts_with("error: "),
            "{args:?}:\n{}",
            run.stderr
        );
    }

    for args in [["--help"], ["--version"]] {
        let run = run(&args);
        assert_eq!(run.code, Some(0), "{args:?}:\n{}", run.stderr);
    }
}

/// gix errors name the place in its sources where they were raised, unless formatted with `{:#}`.
#[test]
fn errors_name_no_rust_source_locations() {
    let fx = Fixture::new();
    for (rev, expected) in [
        ("nope", "cannot resolve revision 'nope'"),
        ("HEAD:.gitmodules", "does not point at a commit"),
    ] {
        let run = plant(&fx.superproject(), &fx.path("out"), &["--rev", rev]);
        run.assert_code(128);
        run.assert_says(expected);
        run.assert_no_source_locations();
    }
}

// --- Phase 0 guard rails: pin down output the refactoring must not change. ---

/// A dry run finds only the top-level submodule and prints nothing else.
#[test]
fn dry_run_output_is_exactly_the_top_level_fetch() {
    let fx = Fixture::new();
    let run = plant(&fx.superproject(), &fx.path("out"), &["--dry-run"]);
    run.assert_ok();
    let lines: Vec<&str> = run.stderr.lines().collect();
    assert_eq!(lines.len(), 2, "{}", run.stderr);
    assert!(
        lines[0].starts_with("Would fetch submodule 'vendor/child' from '")
            && lines[0].contains("' at '"),
        "{}",
        run.stderr
    );
    assert_eq!(
        lines[1],
        "hint: nested submodules are only found once their parents are fetched"
    );
}

/// With one job, every line is printed in the tree's order: a submodule is
/// fetched before its child, and a group is planted parent-first once whole.
#[test]
fn jobs_one_reports_in_tree_order() {
    let fx = Fixture::new();
    let run = plant(&fx.superproject(), &fx.path("out"), &["-j", "1"]);
    run.assert_ok();
    let lines: Vec<&str> = run.stderr.lines().collect();
    let at = |needle: &str| {
        lines
            .iter()
            .position(|line| line.contains(needle))
            .unwrap_or_else(|| panic!("missing {needle:?}:\n{}", run.stderr))
    };
    let child = "Fetching submodule 'vendor/child' from";
    let grand = "Fetching submodule 'vendor/child/third_party/grand' from";
    let child_planted = "Submodule path 'vendor/child': planted";
    let grand_planted = "Submodule path 'vendor/child/third_party/grand': planted";
    assert!(at(child) < at(grand), "{}", run.stderr);
    assert!(at(grand) < at(child_planted), "{}", run.stderr);
    assert!(at(child_planted) < at(grand_planted), "{}", run.stderr);
}
