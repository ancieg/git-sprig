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

use crate::common::*;
use crate::set_child_url;

#[test]
fn fetches_a_single_object_over_both_protocols() {
    let fx = Fixture::new();
    for version in ["1", "2"] {
        let out = fx.path(&format!("out-v{version}"));
        let config = format!("protocol.version={version}");
        let run = plant(&fx.superproject(), &out, &["-c", &config]);
        assert!(run.ok, "protocol v{version}:\n{}", run.stderr);
        assert!(
            !run.stderr.contains("falling back"),
            "protocol v{version}:\n{}",
            run.stderr
        );
        assert_eq!(read(&out.join("vendor/child/hello.txt")), "child\n");
    }
}

#[test]
fn falls_back_when_the_server_refuses_the_object() {
    let fx = Fixture::new();
    for repo in ["remotes/child", "remotes/grand"] {
        git(
            &fx.path(repo),
            &["config", "--unset", "uploadpack.allowAnySHA1InWant"],
        );
    }
    let out = fx.path("out");

    let run = plant(
        &fx.superproject(),
        &out,
        &["-c", "protocol.version=1", "-v"],
    );
    run.assert_ok();
    run.assert_says("falling back");
    run.assert_no_source_locations();
    run.assert_says("fetched with all branches and tags");
    assert_eq!(
        read(&out.join("vendor/child/third_party/grand/g.txt")),
        "grandchild\n"
    );
    assert!(!out.join("vendor/child/future.txt").exists());
}

#[test]
fn resolves_relative_submodule_urls() {
    let fx = Fixture::new();
    let sup = fx.superproject();
    git(&sup, &["remote", "add", "origin", sup.to_str().unwrap()]);
    set_child_url(&sup, "../child");

    let out = fx.path("out");
    let run = plant(&sup, &out, &[]);
    run.assert_ok();
    assert_eq!(read(&out.join("vendor/child/hello.txt")), "child\n");
}

#[test]
fn relative_url_without_a_remote_is_an_error() {
    let fx = Fixture::new();
    let sup = fx.superproject();
    set_child_url(&sup, "../child");

    let run = plant(&sup, &fx.path("out"), &[]);
    run.assert_failed();
    run.assert_says("--remote-url");
}

#[test]
fn remote_url_option_overrides_the_base() {
    let fx = Fixture::new();
    let sup = fx.superproject();
    set_child_url(&sup, "../child");

    let out = fx.path("out");
    let run = plant(&sup, &out, &["--remote-url", sup.to_str().unwrap()]);
    run.assert_ok();
    assert_eq!(read(&out.join("vendor/child/hello.txt")), "child\n");
}

#[test]
fn config_overrides_reach_the_fetch() {
    let fx = Fixture::new();
    let out = fx.path("out");
    let run = plant(
        &fx.superproject(),
        &out,
        &["-c", "protocol.file.allow=never"],
    );
    assert!(!run.ok, "the override was ignored:\n{}", run.stderr);
    run.assert_no_source_locations();
    run.assert_silent_about("falling back");
    assert!(!out.join("vendor/child/hello.txt").exists());
}

#[test]
fn a_missing_commit_is_reported_clearly() {
    let fx = Fixture::new();
    let sup = fx.superproject();
    let ghost = "0123456789abcdef0123456789abcdef01234567";
    git(
        &sup,
        &[
            "update-index",
            "--cacheinfo",
            &format!("160000,{ghost},vendor/child"),
        ],
    );
    git(&sup, &["commit", "-q", "-m", "ghost pin"]);

    let run = plant(&sup, &fx.path("out"), &[]);
    run.assert_failed();
    run.assert_says(ghost);
    run.assert_says("does not provide");
    run.assert_no_source_locations();
}

#[test]
fn submodules_pinning_the_same_commit_share_one_fetch() {
    let fx = Fixture::new();
    let sup = fx.superproject();
    let pin = git(&sup, &["rev-parse", "HEAD:vendor/child"]);
    git(
        &sup,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{pin},vendor/copy"),
        ],
    );
    git(
        &sup,
        &[
            "config",
            "--file",
            ".gitmodules",
            "submodule.vendor/copy.url",
            fx.path("remotes/child").to_str().unwrap(),
        ],
    );
    git(
        &sup,
        &[
            "config",
            "--file",
            ".gitmodules",
            "submodule.vendor/copy.path",
            "vendor/copy",
        ],
    );
    // Not `commit_all`: `git add -A` would drop the gitlink, which has no directory.
    git(&sup, &["add", ".gitmodules"]);
    git(&sup, &["commit", "-q", "-m", "the same commit twice"]);

    let out = fx.path("out");
    let tmp = fx.path("tmp");
    let run = plant(
        &sup,
        &out,
        &["-v", "-j", "1", "--tempdir", tmp.to_str().unwrap()],
    );
    run.assert_ok();
    assert!(
        run.stderr
            .contains("Submodule path 'vendor/child': fetched directly"),
        "{}",
        run.stderr
    );
    assert!(
        run.stderr
            .contains("Submodule path 'vendor/copy': reusing the fetch"),
        "{}",
        run.stderr
    );
    run.assert_silent_about("Submodule path 'vendor/copy': fetched");
    assert_eq!(
        tree_listing(&out.join("vendor/copy")),
        tree_listing(&out.join("vendor/child"))
    );
    assert_eq!(std::fs::read_dir(&tmp).unwrap().count(), 0);
}

/// A server that accepts the request for the pinned commit but sends another
/// one, rigged with `uploadpack.packObjectsHook` swapping the commit ids.
#[test]
fn falls_back_when_the_server_sends_a_different_commit() {
    use std::os::unix::fs::PermissionsExt;

    let fx = Fixture::new();
    let sup = fx.superproject();
    let pin = git(&sup, &["rev-parse", "HEAD:vendor/child"]);
    let tip = git(&fx.path("remotes/child"), &["rev-parse", "HEAD"]);
    assert_ne!(pin, tip);

    let hook = fx.path("swap-commit.sh");
    std::fs::write(
        &hook,
        format!("#!/bin/sh\nsed 's/{pin}/{tip}/g' | \"$@\"\n"),
    )
    .unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    let config = fx.path("gitconfig");
    std::fs::write(
        &config,
        format!("[uploadpack]\n\tpackObjectsHook = {}\n", hook.display()),
    )
    .unwrap();

    let out = fx.path("out");
    let output = git_sprig(&[
        "-C",
        sup.to_str().unwrap(),
        "-b",
        out.to_str().unwrap(),
        "-v",
        "--no-recursive",
    ])
    .env("GIT_CONFIG_GLOBAL", &config)
    .output()
    .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(output.status.success(), "{stderr}");
    assert!(stderr.contains("did not send"), "{stderr}");
    assert!(
        stderr.contains("fetched with all branches and tags"),
        "{stderr}"
    );
    assert_eq!(read(&out.join("vendor/child/hello.txt")), "child\n");
    assert!(
        !out.join("vendor/child/future.txt").exists(),
        "wrote the tip, not the pin"
    );
}

#[test]
fn configured_urls_win_over_gitmodules_as_in_git() {
    let fx = Fixture::new();
    let sup = fx.superproject();
    let mirror = fx.path("mirror/child");
    let child = fx.path("remotes/child");
    git(
        fx.root.path(),
        &[
            "clone",
            "-q",
            "--mirror",
            child.to_str().unwrap(),
            mirror.to_str().unwrap(),
        ],
    );
    git(
        &mirror,
        &["config", "uploadpack.allowAnySHA1InWant", "true"],
    );
    std::fs::rename(&child, fx.path("remotes/child-gone")).unwrap();
    let mirror_url = format!("submodule.vendor/child.url={}", mirror.display());

    // The superproject's own configuration counts, as for `git submodule`.
    git(
        &sup,
        &[
            "config",
            "submodule.vendor/child.url",
            mirror.to_str().unwrap(),
        ],
    );
    let out = fx.path("out");
    plant(&sup, &out, &[]).assert_ok();
    assert_eq!(read(&out.join("vendor/child/hello.txt")), "child\n");

    // A direct source's own configuration does not, as for `git clone`; -c does.
    let source = sup.to_str().unwrap();
    direct(source, &fx.path("direct"), "HEAD", &[]).assert_failed();
    direct(source, &fx.path("direct"), "HEAD", &["-c", &mirror_url]).assert_ok();
    assert_eq!(read(&fx.path("direct/vendor/child/hello.txt")), "child\n");
}

#[test]
fn inactive_submodules_are_skipped() {
    let fx = Fixture::new();
    let sup = fx.superproject();
    add_submodule(&sup, &fx.path("remotes/grand"), "vendor/grand");
    commit_all(&sup, "second submodule");
    // A fresh clone, so no `submodule.*` settings from `git submodule add` linger.
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
        &["config", "submodule.vendor/child.active", "false"],
    );
    let out = fx.path("out-by-name");
    let run = plant(&clone, &out, &["-v"]);
    run.assert_ok();
    run.assert_says("vendor/child': not active");
    assert!(!out.join("vendor/child/hello.txt").exists());
    assert_eq!(read(&out.join("vendor/grand/g.txt")), "grandchild\n");

    git(
        &clone,
        &["config", "--unset", "submodule.vendor/child.active"],
    );
    git(&clone, &["config", "submodule.active", "vendor/child"]);
    let out = fx.path("out-by-pathspec");
    let run = plant(&clone, &out, &["-v"]);
    run.assert_ok();
    assert_eq!(read(&out.join("vendor/child/hello.txt")), "child\n");
    // Nested submodules follow their own repository's settings, where none are set.
    assert_eq!(
        read(&out.join("vendor/child/third_party/grand/g.txt")),
        "grandchild\n"
    );
    assert!(!out.join("vendor/grand/g.txt").exists());

    // `git -c` reaches git-sprig through git, and our own -c comes later.
    let sup = clone.to_str().unwrap();
    let child_active = |value: &str| format!("submodule.vendor/child.active={value}");
    let out = fx.path("out-through-git");
    let out = out.to_str().unwrap();
    let off = child_active("false");
    let run = through_git(
        fx.root.path(),
        &["-c", &off, "sprig", "-C", sup, "-b", out, "-v"],
    );
    run.assert_ok();
    run.assert_says("vendor/child': not active");
    let on = child_active("true");
    let args = ["-c", &off, "sprig", "-C", sup, "-b", out, "-c", &on];
    through_git(fx.root.path(), &args).assert_ok();
    assert_eq!(
        read(&fx.path("out-through-git/vendor/child/hello.txt")),
        "child\n"
    );

    // -c applies to the superproject, like `git -c`.
    git(&clone, &["config", "--unset", "submodule.active"]);
    let out = fx.path("out-by-override");
    let run = plant(&clone, &out, &["-c", "submodule.vendor/grand.active=false"]);
    run.assert_ok();
    assert_eq!(read(&out.join("vendor/child/hello.txt")), "child\n");
    assert!(!out.join("vendor/grand/g.txt").exists());
}

/// A server that cannot be reached fails before the commit is asked for, so
/// the fallback, which starts the same way, is not attempted.
#[test]
fn an_unreachable_http_server_is_not_tried_twice() {
    let fx = Fixture::new();
    let sup = fx.superproject();
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    set_child_url(&sup, &format!("http://127.0.0.1:{port}/child.git"));

    let run = plant(&sup, &fx.path("out"), &[]);
    run.assert_failed();
    run.assert_silent_about("falling back");
    run.assert_says(&format!("127.0.0.1:{port}"));
    run.assert_no_source_locations();
}
