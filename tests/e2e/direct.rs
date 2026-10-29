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

use crate::{common::*, set_child_url};

fn file_url(path: &Path) -> String {
    format!("file://{}", path.display())
}

#[test]
fn materializes_a_local_tree_at_an_older_revision() {
    let root = tempfile::tempdir().unwrap();
    let repo = root.path().join("repo");
    init_repo(&repo);
    write(&repo, "root.txt", "root\n");
    write(&repo, "nested/dir/file.txt", "nested\n");
    write(&repo, ".gitattributes", "auto.txt text=auto\n");
    write(&repo, "auto.txt", "one\ntwo\n");
    write_exec(&repo, "bin/run.sh", "#!/bin/sh\necho hi\n");
    symlink(&repo, "link", "root.txt");
    let old = commit_all(&repo, "old tree");
    write(&repo, "future.txt", "not pinned\n");
    commit_all(&repo, "new tree");

    let out = root.path().join("out");
    let run = direct(
        repo.to_str().unwrap(),
        &out,
        "HEAD~1",
        &["-c", "core.autocrlf=true"],
    );
    run.assert_ok();
    run.assert_says(&old);
    assert_eq!(read(&out.join("root.txt")), "root\r\n");
    assert_eq!(read(&out.join("nested/dir/file.txt")), "nested\r\n");
    assert_eq!(read(&out.join("auto.txt")), "one\r\ntwo\r\n");
    assert!(is_executable(&out.join("bin/run.sh")));
    assert!(!is_executable(&out.join("root.txt")));
    assert_eq!(
        std::fs::read_link(out.join("link")).unwrap(),
        Path::new("root.txt")
    );
    assert!(!out.join("future.txt").exists());
    assert!(!out.join(".git").exists());
}

#[test]
fn streams_loose_files_from_local_repositories() {
    let root = tempfile::tempdir().unwrap();
    let repo = root.path().join("repo");
    init_repo(&repo);
    let big = vec![0x5a; 1 << 20];
    std::fs::write(repo.join("big.bin"), &big).unwrap();
    write(&repo, "small.txt", "small\n");
    commit_all(&repo, "loose files");

    let worktree = root.path().join("worktree");
    git(
        &repo,
        &["worktree", "add", "-q", worktree.to_str().unwrap()],
    );

    for (name, source) in [("out", &repo), ("out-worktree", &worktree)] {
        let out = root.path().join(name);
        let run = direct(source.to_str().unwrap(), &out, "HEAD", &["-v", "--raw"]);
        run.assert_ok();
        run.assert_says("wrote 2 files, 2 streamed");
        assert_eq!(std::fs::read(out.join("big.bin")).unwrap(), big);
        assert_eq!(read(&out.join("small.txt")), "small\n");
    }
}

#[test]
fn resolves_remote_ids_branches_tags_and_head() {
    let root = tempfile::tempdir().unwrap();
    let repo = root.path().join("repo");
    init_repo(&repo);
    write(&repo, "version.txt", "old\n");
    let old = commit_all(&repo, "old tree");
    git(&repo, &["branch", "old-branch"]);
    git(&repo, &["tag", "lightweight"]);
    git(&repo, &["tag", "-a", "annotated", "-m", "release"]);
    git(&repo, &["update-ref", "refs/pull/1/head", &old]);
    write(&repo, "version.txt", "new\n");
    let tip = commit_all(&repo, "new tree");
    let url = file_url(&repo);

    let cases = [
        (old.as_str(), old.as_str(), "old\n"),
        ("old-branch", old.as_str(), "old\n"),
        ("refs/heads/old-branch", old.as_str(), "old\n"),
        ("refs/pull/1/head", old.as_str(), "old\n"),
        ("lightweight", old.as_str(), "old\n"),
        ("annotated", old.as_str(), "old\n"),
        ("HEAD", tip.as_str(), "new\n"),
    ];
    for (index, (rev, commit, contents)) in cases.into_iter().enumerate() {
        let out = root.path().join(format!("out-{index}"));
        let run = direct(&url, &out, rev, &[]);
        assert!(run.ok, "{rev}:\n{}", run.stderr);
        run.assert_says(commit);
        assert_eq!(read(&out.join("version.txt")), contents, "{rev}");
        assert!(!out.join(".git").exists(), "{rev}");
    }
}

#[test]
fn reuses_conversion_and_recursive_submodule_materialization() {
    let fx = Fixture::new();
    let sup = fx.superproject();
    let out = fx.path("out");
    let run = direct(sup.to_str().unwrap(), &out, "HEAD", &[]);
    run.assert_ok();
    assert_eq!(read(&out.join("super.txt")), "superproject\n");
    assert_eq!(read(&out.join("vendor/child/hello.txt")), "child\n");
    assert_eq!(
        read(&out.join("vendor/child/third_party/grand/g.txt")),
        "grandchild\n"
    );
    assert_eq!(read(&out.join("vendor/child/crlf.txt")), "dos\r\nlines\r\n");
    assert!(out.join(".gitmodules").is_file());
    assert!(out.join("vendor/child/.gitmodules").is_file());
    assert!(!out.join(".git").exists());
    assert!(!out.join("vendor/child/.git").exists());
    assert!(!out.join("vendor/child/third_party/grand/.git").exists());
    assert!(!out.join("vendor/child/future.txt").exists());
    assert_eq!(leftovers(&out), Vec::<String>::new());

    let raw = fx.path("raw");
    direct(sup.to_str().unwrap(), &raw, "HEAD", &["--raw"]).assert_ok();
    assert_eq!(read(&raw.join("vendor/child/crlf.txt")), "dos\nlines\n");

    for (name, extra) in [
        ("no-recursive", vec!["--no-recursive"]),
        ("max-depth", vec!["--max-depth", "1"]),
    ] {
        let out = fx.path(name);
        direct(sup.to_str().unwrap(), &out, "HEAD", &extra).assert_ok();
        assert_eq!(read(&out.join("vendor/child/hello.txt")), "child\n");
        assert!(out.join("vendor/child/third_party/grand").is_dir());
        assert!(!out.join("vendor/child/third_party/grand/g.txt").exists());
    }
}

#[test]
fn resolves_relative_root_submodule_urls() {
    let fx = Fixture::new();
    let sup = fx.superproject();
    set_child_url(&sup, "../child");
    let out = fx.path("out");

    direct(&file_url(&sup), &out, "HEAD", &[]).assert_ok();
    assert_eq!(read(&out.join("vendor/child/hello.txt")), "child\n");
    assert_eq!(
        read(&out.join("vendor/child/third_party/grand/g.txt")),
        "grandchild\n"
    );
}

#[test]
fn resolves_relative_submodule_urls_against_the_local_source() {
    let fx = Fixture::new();
    let sup = fx.superproject();
    set_child_url(&sup, "../child");

    // However the source is spelled, `../child` is the sibling of `super`.
    for (index, (cwd, source)) in [
        (sup.clone(), "."),
        (sup.join("vendor"), ".."),
        (fx.path("remotes"), "super/vendor"),
    ]
    .into_iter()
    .enumerate()
    {
        let out = fx.path(&format!("out-{index}"));
        let output = git_sprig(&[source, out.to_str().unwrap()])
            .current_dir(&cwd)
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{source}: {stderr}");
        assert_eq!(read(&out.join("super.txt")), "superproject\n");
        assert_eq!(read(&out.join("vendor/child/hello.txt")), "child\n");
    }
}

#[test]
fn replaces_a_plain_destination_only_after_complete_success() {
    let fx = Fixture::new();
    let sup = fx.superproject();
    let out = fx.path("out");
    write(&out, "stale.txt", "old\n");
    write(&out, "vendor/child/stale.txt", "old\n");
    direct(sup.to_str().unwrap(), &out, "HEAD", &[]).assert_ok();
    assert!(!out.join("stale.txt").exists());
    assert!(!out.join("vendor/child/stale.txt").exists());
    assert_eq!(read(&out.join("super.txt")), "superproject\n");

    write(&out, "marker.txt", "previous\n");
    let before = tree_listing(&out);
    std::fs::rename(fx.path("remotes/grand"), fx.path("remotes/grand-moved")).unwrap();
    direct(sup.to_str().unwrap(), &out, "HEAD", &[]).assert_failed();
    assert_eq!(tree_listing(&out), before);
    assert_eq!(leftovers(&out), Vec::<String>::new());

    let missing = file_url(&fx.path("remotes/missing"));
    direct(&missing, &out, "HEAD", &[]).assert_failed();
    assert_eq!(tree_listing(&out), before);
}

#[test]
fn protects_a_destination_checkout_unless_forced() {
    let root = tempfile::tempdir().unwrap();
    let repo = root.path().join("repo");
    init_repo(&repo);
    write(&repo, "source.txt", "source\n");
    commit_all(&repo, "source");
    let out = root.path().join("out");
    init_repo(&out);
    write(&out, "wip.txt", "uncommitted\n");

    let missing = file_url(&root.path().join("missing"));
    let run = direct(&missing, &out, "HEAD", &[]);
    run.assert_failed();
    run.assert_says("--force");
    run.assert_silent_about("cannot fetch");
    assert_eq!(read(&out.join("wip.txt")), "uncommitted\n");

    direct(repo.to_str().unwrap(), &out, "HEAD", &["--force"]).assert_ok();
    assert_eq!(read(&out.join("source.txt")), "source\n");
    assert!(!out.join("wip.txt").exists());
    assert!(!out.join(".git").exists());
}

#[test]
fn protects_a_destination_holding_the_local_source_unless_forced() {
    let root = tempfile::tempdir().unwrap();
    let work = root.path().join("work");
    init_repo(&work);
    write(&work, "source.txt", "source\n");
    commit_all(&work, "source");
    // A bare repository has no .git entry that the checkout search could find.
    let out = root.path().join("out");
    let bare = out.join("src.git");
    git(
        &work,
        &["clone", "-q", "--bare", ".", bare.to_str().unwrap()],
    );
    // Only the worktree is outside; its repository is inside the destination.
    let worktree = root.path().join("worktree");
    git(
        &bare,
        &["worktree", "add", "-q", worktree.to_str().unwrap()],
    );
    write(&worktree, "wip.txt", "uncommitted\n");

    for source in [&bare, &worktree] {
        let run = direct(source.to_str().unwrap(), &out, "HEAD", &[]);
        run.assert_failed();
        run.assert_says("contains the source repository");
        run.assert_says("--force");
        assert!(bare.join("HEAD").is_file());
        assert_eq!(read(&worktree.join("wip.txt")), "uncommitted\n");
        assert_eq!(leftovers(root.path()), Vec::<String>::new());
    }

    direct(bare.to_str().unwrap(), &out, "HEAD", &["--force"]).assert_ok();
    assert_eq!(read(&out.join("source.txt")), "source\n");
    assert!(!out.join("src.git").exists());
}

#[test]
fn protects_checkouts_inside_the_destination_unless_forced() {
    let root = tempfile::tempdir().unwrap();
    let repo = root.path().join("repo");
    init_repo(&repo);
    write(&repo, "source.txt", "source\n");
    commit_all(&repo, "source");
    let out = root.path().join("out");
    let nested = out.join("projects/other");
    init_repo(&nested);
    write(&nested, "wip.txt", "uncommitted\n");

    let run = direct(repo.to_str().unwrap(), &out, "HEAD", &[]);
    run.assert_failed();
    run.assert_says("holds the git checkout");
    run.assert_says("projects/other");
    run.assert_says("--force");
    assert_eq!(read(&nested.join("wip.txt")), "uncommitted\n");
    assert_eq!(leftovers(root.path()), Vec::<String>::new());

    // Replacing a symlink leaves the checkouts below its target alone.
    let link = root.path().join("link");
    std::os::unix::fs::symlink(&out, &link).unwrap();
    direct(repo.to_str().unwrap(), &link, "HEAD", &[]).assert_ok();
    assert_eq!(read(&link.join("source.txt")), "source\n");
    assert_eq!(read(&nested.join("wip.txt")), "uncommitted\n");

    direct(repo.to_str().unwrap(), &out, "HEAD", &["--force"]).assert_ok();
    assert_eq!(read(&out.join("source.txt")), "source\n");
    assert!(!out.join("projects").exists());
}

#[test]
fn dry_run_reports_the_root_without_touching_the_destination() {
    let fx = Fixture::new();
    let sup = fx.superproject();
    let commit = git(&sup, &["rev-parse", "HEAD"]);
    for (name, source) in [
        ("local", sup.to_str().unwrap().to_owned()),
        ("remote", file_url(&sup)),
    ] {
        let out = fx.path(name);
        let tmp = fx.path(&format!("tmp-{name}"));
        let run = direct(
            &source,
            &out,
            "HEAD",
            &["--dry-run", "--tempdir", tmp.to_str().unwrap()],
        );
        run.assert_ok();
        run.assert_says(&source);
        run.assert_says("HEAD");
        run.assert_says(&commit);
        run.assert_says(out.to_str().unwrap());
        run.assert_says("Would fetch submodule 'vendor/child'");
        assert!(!out.exists());
        assert!(match std::fs::read_dir(&tmp) {
            Ok(entries) => entries.count() == 0,
            Err(err) => err.kind() == std::io::ErrorKind::NotFound,
        });
    }
}

#[test]
fn rejects_invalid_direct_arguments_before_fetching() {
    let root = tempfile::tempdir().unwrap();
    let out = root.path().join("out");
    let missing = file_url(&root.path().join("missing"));
    let cases: [&[&str]; 5] = [
        &[&missing],
        &[&missing, out.to_str().unwrap(), "--stage"],
        &[&missing, out.to_str().unwrap(), "-C", "."],
        &[
            &missing,
            out.to_str().unwrap(),
            "--work-tree",
            out.to_str().unwrap(),
        ],
        &[
            &missing,
            out.to_str().unwrap(),
            "--basedir",
            out.to_str().unwrap(),
        ],
    ];
    for args in cases {
        let run = run(args);
        run.assert_code(129);
        assert!(run.stderr.starts_with("error: "), "{}", run.stderr);
        run.assert_silent_about("cannot fetch");
        assert!(!out.exists());
    }
}

#[test]
fn destinations_must_end_in_a_name() {
    let root = tempfile::tempdir().unwrap();
    let repo = root.path().join("repo");
    init_repo(&repo);
    write(&repo, "source.txt", "source\n");
    commit_all(&repo, "source");
    let work = root.path().join("work");
    std::fs::create_dir(&work).unwrap();

    for out in [".", "..", "out/.."] {
        let output = git_sprig(&[repo.to_str().unwrap(), out])
            .current_dir(&work)
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(129), "{out}: {stderr}");
        assert!(stderr.contains("must end in a name"), "{out}: {stderr}");
    }
    assert_eq!(std::fs::read_dir(&work).unwrap().count(), 0);

    let out = root.path().join("out/.");
    direct(repo.to_str().unwrap(), &out, "HEAD", &[]).assert_ok();
    assert_eq!(read(&root.path().join("out/source.txt")), "source\n");
    assert_eq!(leftovers(root.path()), Vec::<String>::new());
}

#[test]
fn hostile_root_trees_leave_the_previous_destination_intact() {
    let root = tempfile::tempdir().unwrap();
    let repo = root.path().join("repo");
    init_repo(&repo);
    let config = hash_object(&repo, "[core]\n");
    let inner = git_with_input(
        &repo,
        &["mktree"],
        &format!("100644 blob {config}\tconfig\n"),
    );
    let ok = hash_object(&repo, "ok\n");
    let tree = git_with_input(
        &repo,
        &["mktree"],
        &format!("100644 blob {ok}\tok.txt\n040000 tree {inner}\t.git\n"),
    );
    let commit = git(&repo, &["commit-tree", &tree, "-m", "hostile"]);
    git(&repo, &["update-ref", "refs/heads/main", &commit]);
    let out = root.path().join("out");
    write(&out, "previous.txt", "previous\n");

    direct(repo.to_str().unwrap(), &out, "HEAD", &[]).assert_failed();
    assert_eq!(read(&out.join("previous.txt")), "previous\n");
    assert!(!out.join("ok.txt").exists());
    assert_eq!(leftovers(root.path()), Vec::<String>::new());
}

#[test]
fn interruption_keeps_the_previous_root_and_cleans_up() {
    use std::{
        fmt::Write as _,
        process::{Command, Stdio},
        time::{Duration, Instant},
    };

    let root = tempfile::tempdir().unwrap();
    let repo = root.path().join("repo");
    init_repo(&repo);
    let blob = hash_object(&repo, "x\n");
    let listing = (0..30_000).fold(String::new(), |mut listing, i| {
        writeln!(listing, "100644 blob {blob}\tfile-{i:05}").unwrap();
        listing
    });
    let tree = git_with_input(&repo, &["mktree"], &listing);
    let commit = git(&repo, &["commit-tree", &tree, "-m", "large"]);
    git(&repo, &["update-ref", "refs/heads/main", &commit]);
    let out = root.path().join("out");
    let tmp = root.path().join("tmp");
    write(&out, "previous.txt", "previous\n");

    let mut child = git_sprig(&[
        repo.to_str().unwrap(),
        out.to_str().unwrap(),
        "--tempdir",
        tmp.to_str().unwrap(),
    ])
    .stderr(Stdio::piped())
    .spawn()
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let writing = leftovers(root.path()).iter().any(|name| {
            std::fs::read_dir(root.path().join(name)).is_ok_and(|mut dir| dir.next().is_some())
        });
        if writing {
            break;
        }
        assert!(child.try_wait().unwrap().is_none());
        assert!(Instant::now() < deadline, "never started writing");
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(
        Command::new("sh")
            .args(["-c", &format!("kill -INT {}", child.id())])
            .status()
            .unwrap()
            .success()
    );
    let output = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(130), "{stderr}");
    assert_eq!(read(&out.join("previous.txt")), "previous\n");
    assert_eq!(leftovers(root.path()), Vec::<String>::new());
    assert_eq!(std::fs::read_dir(&tmp).unwrap().count(), 0);
}
