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

use std::fmt::Write as _;

use crate::common::*;

#[test]
fn line_endings_follow_gitattributes_unless_raw() {
    let fx = Fixture::new();
    let out = fx.path("out");
    let run = plant(&fx.superproject(), &out, &[]);
    run.assert_ok();
    assert_eq!(read(&out.join("vendor/child/crlf.txt")), "dos\r\nlines\r\n");
    assert_eq!(read(&out.join("vendor/child/hello.txt")), "child\n");

    let raw = fx.path("raw");
    let run = plant(&fx.superproject(), &raw, &["--raw"]);
    run.assert_ok();
    assert_eq!(read(&raw.join("vendor/child/crlf.txt")), "dos\nlines\n");
}

#[test]
fn files_stored_whole_are_streamed_unless_they_need_converting() {
    let root = tempfile::tempdir().unwrap();
    let sub = root.path().join("remotes/sub");
    init_repo(&sub);
    let mut state = 0x2545_f491_4f6c_dd1d_u64;
    let big: Vec<u8> = (0..4 << 20)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state.to_le_bytes()[0]
        })
        .collect();
    std::fs::write(sub.join("big.bin"), &big).unwrap();
    write(
        &sub,
        ".gitattributes",
        "crlf.txt text eol=crlf\n*.dat binary\nauto.txt text=auto eol=lf\n",
    );
    write(&sub, "crlf.txt", "a\nb\n");
    write(&sub, "data.dat", "x\ny\n");
    write(&sub, "auto.txt", "c\nd\n");
    commit_all(&sub, "files");

    let sup = root.path().join("remotes/super");
    init_repo(&sup);
    write(&sup, "super.txt", "superproject\n");
    commit_all(&sup, "super");
    add_submodule(&sup, &sub, "vendor/sub");
    commit_all(&sup, "submodule");

    let out = root.path().join("out");
    let run = plant(&sup, &out, &["-v"]);
    run.assert_ok();
    // Everything but crlf.txt, which needs its line endings converted.
    assert!(
        run.stderr
            .contains("Submodule path 'vendor/sub': wrote 5 files, 4 streamed"),
        "{}",
        run.stderr
    );
    assert!(
        std::fs::read(out.join("vendor/sub/big.bin")).unwrap() == big,
        "big.bin differs"
    );
    assert_eq!(read(&out.join("vendor/sub/crlf.txt")), "a\r\nb\r\n");
    assert_eq!(read(&out.join("vendor/sub/data.dat")), "x\ny\n");

    let raw = root.path().join("raw");
    let run = plant(&sup, &raw, &["-v", "--raw"]);
    run.assert_ok();
    assert!(
        run.stderr
            .contains("Submodule path 'vendor/sub': wrote 5 files, 5 streamed"),
        "{}",
        run.stderr
    );
    assert_eq!(read(&raw.join("vendor/sub/crlf.txt")), "a\nb\n");
}

/// Whether a file streams or goes through the filter pipeline, the bytes must be
/// what git itself checks out, for every combination of attributes and settings.
#[test]
fn line_endings_match_git_for_every_setting() {
    let root = tempfile::tempdir().unwrap();
    let sub = root.path().join("remotes/sub");
    init_repo(&sub);
    let files = [
        ("plain.txt", ""),
        ("text.txt", "text"),
        ("no-text.txt", "-text"),
        ("auto.txt", "text=auto"),
        ("auto-lf.txt", "text=auto eol=lf"),
        ("auto-crlf.txt", "text=auto eol=crlf"),
        ("eol-lf.txt", "eol=lf"),
        ("eol-crlf.txt", "eol=crlf"),
        ("crlf.txt", "crlf"),
        ("no-crlf.txt", "-crlf"),
        ("crlf-input.txt", "crlf=input"),
        ("text-no-crlf.txt", "text -crlf"),
        ("binary.dat", "binary"),
    ];
    let mut attributes = String::new();
    for (name, attrs) in files {
        write(&sub, name, "one\ntwo\n");
        if !attrs.is_empty() {
            writeln!(attributes, "{name} {attrs}").unwrap();
        }
    }
    write(&sub, ".gitattributes", &attributes);
    commit_all(&sub, "files");

    let sup = root.path().join("remotes/super");
    init_repo(&sup);
    write(&sup, "super.txt", "superproject\n");
    commit_all(&sup, "super");
    add_submodule(&sup, &sub, "vendor/sub");
    commit_all(&sup, "submodule");

    for config in [
        "core.autocrlf=false",
        "core.autocrlf=true",
        "core.autocrlf=input",
        "core.eol=crlf",
    ] {
        let ours = root.path().join(format!("git-sprig-{config}"));
        let run = plant(&sup, &ours, &["-c", config]);
        assert!(run.ok, "{config}:\n{}", run.stderr);

        let theirs = root.path().join(format!("git-{config}"));
        git(
            root.path(),
            &[
                "-c",
                config,
                "clone",
                "-q",
                sub.to_str().unwrap(),
                theirs.to_str().unwrap(),
            ],
        );
        assert_eq!(
            tree_listing(&ours.join("vendor/sub")),
            tree_listing(&theirs),
            "differs from git with {config}"
        );
    }
}
