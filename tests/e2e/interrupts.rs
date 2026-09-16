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

use std::{
    fmt::Write as _,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use crate::common::*;

#[test]
fn an_interrupt_cleans_up_and_keeps_the_previous_snapshot() {
    let root = tempfile::tempdir().unwrap();
    // Enough files that writing them takes long enough to be interrupted.
    let sup = crafted_superproject(root.path(), |dir| {
        let blob = hash_object(dir, "x\n");
        (0..30_000).fold(String::new(), |mut listing, i| {
            writeln!(listing, "100644 blob {blob}\tfile-{i:05}").unwrap();
            listing
        })
    });
    let out = root.path().join("out");
    let tmp = root.path().join("tmp");
    write(&out, "vendor/sub/previous.txt", "previous");

    let mut child = git_sprig(&[
        "-C",
        sup.to_str().unwrap(),
        "-b",
        out.to_str().unwrap(),
        "--tempdir",
        tmp.to_str().unwrap(),
    ])
    .stderr(Stdio::piped())
    .spawn()
    .unwrap();

    let vendor = out.join("vendor");
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let writing = leftovers(&vendor).iter().any(|name| {
            std::fs::read_dir(vendor.join(name)).is_ok_and(|mut dir| dir.next().is_some())
        });
        if writing {
            break;
        }
        assert!(
            child.try_wait().unwrap().is_none(),
            "finished before it could be interrupted"
        );
        assert!(Instant::now() < deadline, "never started writing");
        std::thread::sleep(Duration::from_millis(1));
    }

    let killed = Command::new("sh")
        .args(["-c", &format!("kill -INT {}", child.id())])
        .status()
        .unwrap();
    assert!(killed.success());
    let output = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_eq!(output.status.code(), Some(130), "{stderr}");
    assert!(stderr.contains("fatal: interrupted"), "{stderr}");
    assert_eq!(read(&vendor.join("sub/previous.txt")), "previous");
    assert_eq!(leftovers(&vendor), Vec::<String>::new());
    assert_eq!(std::fs::read_dir(&tmp).unwrap().count(), 0);
}

/// A fetch blocked on a server that never answers cannot notice Ctrl-C; git-sprig
/// still has to clean up and exit.
#[test]
fn an_interrupt_during_a_stuck_fetch_still_cleans_up() {
    use std::os::unix::fs::PermissionsExt;

    let fx = Fixture::new();
    let fake = fx.path("fake-bin");
    std::fs::create_dir_all(&fake).unwrap();
    // Swallows the request and never answers; exits once git-sprig closes the pipe.
    let upload_pack = fake.join("git-upload-pack");
    // No `exec`: the shell keeps stdout open while `cat` waits for git-sprig to go away.
    std::fs::write(&upload_pack, "#!/bin/sh\ncat > /dev/null\n").unwrap();
    std::fs::set_permissions(&upload_pack, std::fs::Permissions::from_mode(0o755)).unwrap();

    let out = fx.path("out");
    let tmp = fx.path("tmp");
    write(&out, "vendor/child/previous.txt", "previous");
    let path = format!("{}:{}", fake.display(), std::env::var("PATH").unwrap());
    let mut child = git_sprig(&[
        "-C",
        fx.superproject().to_str().unwrap(),
        "-b",
        out.to_str().unwrap(),
        "--tempdir",
        tmp.to_str().unwrap(),
    ])
    .env("PATH", path)
    .stderr(Stdio::piped())
    .spawn()
    .unwrap();

    let deadline = Instant::now() + Duration::from_secs(30);
    while !std::fs::read_dir(&tmp)
        .into_iter()
        .flatten()
        .flatten()
        .any(|root| {
            std::fs::read_dir(root.path())
                .into_iter()
                .flatten()
                .flatten()
                .any(|entry| entry.file_name().to_string_lossy().starts_with("odb-"))
        })
    {
        assert!(
            child.try_wait().unwrap().is_none(),
            "git-sprig exited before it got stuck"
        );
        assert!(Instant::now() < deadline, "the fetch never started");
        std::thread::sleep(Duration::from_millis(10));
    }
    std::thread::sleep(Duration::from_millis(200));

    let interrupted = Instant::now();
    let killed = Command::new("sh")
        .args(["-c", &format!("kill -INT {}", child.id())])
        .status()
        .unwrap();
    assert!(killed.success());
    let output = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        interrupted.elapsed() < Duration::from_secs(15),
        "took {:?}",
        interrupted.elapsed()
    );
    assert_eq!(output.status.code(), Some(130), "{stderr}");
    assert_eq!(stderr.matches("fatal: interrupted").count(), 1, "{stderr}");
    assert_eq!(read(&out.join("vendor/child/previous.txt")), "previous");
    assert_eq!(leftovers(&out.join("vendor")), Vec::<String>::new());
    assert_eq!(
        std::fs::read_dir(&tmp).unwrap().count(),
        0,
        "temporary data left"
    );
}

/// A second Ctrl-C still removes the staging and temporary directories, which
/// the old signal handler left behind when it exited on the second signal.
#[test]
fn a_second_interrupt_still_cleans_up() {
    use std::os::unix::fs::PermissionsExt;

    let fx = Fixture::new();
    let fake = fx.path("fake-bin");
    std::fs::create_dir_all(&fake).unwrap();
    let upload_pack = fake.join("git-upload-pack");
    std::fs::write(&upload_pack, "#!/bin/sh\ncat > /dev/null\n").unwrap();
    std::fs::set_permissions(&upload_pack, std::fs::Permissions::from_mode(0o755)).unwrap();

    let out = fx.path("out");
    let tmp = fx.path("tmp");
    write(&out, "vendor/child/previous.txt", "previous");
    let path = format!("{}:{}", fake.display(), std::env::var("PATH").unwrap());
    let mut child = git_sprig(&[
        "-C",
        fx.superproject().to_str().unwrap(),
        "-b",
        out.to_str().unwrap(),
        "--tempdir",
        tmp.to_str().unwrap(),
    ])
    .env("PATH", path)
    .stderr(Stdio::piped())
    .spawn()
    .unwrap();

    let deadline = Instant::now() + Duration::from_secs(30);
    while !std::fs::read_dir(&tmp)
        .into_iter()
        .flatten()
        .flatten()
        .any(|root| {
            std::fs::read_dir(root.path())
                .into_iter()
                .flatten()
                .flatten()
                .any(|entry| entry.file_name().to_string_lossy().starts_with("odb-"))
        })
    {
        assert!(
            child.try_wait().unwrap().is_none(),
            "exited before it got stuck"
        );
        assert!(Instant::now() < deadline, "the fetch never started");
        std::thread::sleep(Duration::from_millis(10));
    }
    std::thread::sleep(Duration::from_millis(200));

    // Two Ctrl-C in a row: the second cuts the grace period short.
    for _ in 0..2 {
        Command::new("sh")
            .args(["-c", &format!("kill -INT {}", child.id())])
            .status()
            .unwrap();
        std::thread::sleep(Duration::from_millis(150));
    }

    let output = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(130), "{stderr}");
    assert_eq!(stderr.matches("fatal: interrupted").count(), 1, "{stderr}");
    assert_eq!(read(&out.join("vendor/child/previous.txt")), "previous");
    assert_eq!(leftovers(&out.join("vendor")), Vec::<String>::new());
    assert_eq!(
        std::fs::read_dir(&tmp).unwrap().count(),
        0,
        "temporary data left"
    );
}
