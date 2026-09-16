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

/// Entries git would never write must be refused before the destination is touched.
#[test]
fn hostile_tree_entries_are_refused() {
    type Craft = fn(&Path) -> String;
    let cases: [(&str, Craft, &str); 5] = [
        (
            ".git directory",
            |dir| {
                let config = hash_object(dir, "[core]\n");
                let inner =
                    git_with_input(dir, &["mktree"], &format!("100644 blob {config}\tconfig\n"));
                format!("040000 tree {inner}\t.git\n")
            },
            "would write into a .git directory",
        ),
        (
            "traversal",
            |dir| format!("100644 blob {}\t..\n", hash_object(dir, "x\n")),
            "is a directory traversal",
        ),
        (
            "8.3 short name",
            |dir| format!("100644 blob {}\tGIT~1\n", hash_object(dir, "x\n")),
            "would write into a .git directory",
        ),
        (
            "trailing space",
            |dir| format!("100644 blob {}\t.git \n", hash_object(dir, "x\n")),
            "would write into a .git directory",
        ),
        (
            "symlinked .gitmodules",
            |dir| {
                format!(
                    "120000 blob {}\t.gitmodules\n",
                    hash_object(dir, "/etc/passwd")
                )
            },
            "which git never allows for .gitmodules",
        ),
    ];

    for (label, craft, expected) in cases {
        for mode in [None, Some("--raw")] {
            let root = tempfile::tempdir().unwrap();
            let sup = crafted_superproject(root.path(), |dir| {
                format!(
                    "100644 blob {}\tok.txt\n{}",
                    hash_object(dir, "ok\n"),
                    craft(dir)
                )
            });
            let out = root.path().join("out");
            write(&out, "vendor/sub/previous.txt", "previous");

            let extra: Vec<&str> = mode.into_iter().collect();
            let run = plant(&sup, &out, &extra);

            let context = format!("{label} with {mode:?}:\n{}", run.stderr);
            assert!(!run.ok, "accepted {context}");
            assert!(run.stderr.contains(expected), "wrong message for {context}");
            assert_eq!(
                read(&out.join("vendor/sub/previous.txt")),
                "previous",
                "{context}"
            );
            assert!(!out.join("vendor/sub/ok.txt").exists(), "{context}");
            assert_eq!(
                leftovers(&out.join("vendor")),
                Vec::<String>::new(),
                "{context}"
            );
        }
    }
}

/// A directory `a` holding a submodule, then a symlink also named `a`: without
/// the duplicate check the submodule would be written through the symlink.
#[test]
fn a_duplicate_name_cannot_redirect_a_nested_submodule() {
    for mode in [None, Some("--raw")] {
        let root = tempfile::tempdir().unwrap();
        let victim = root.path().join("victim");
        std::fs::create_dir_all(&victim).unwrap();

        let nested = root.path().join("remotes/nested");
        init_repo(&nested);
        write(&nested, "dir/f.txt", "escaped\n");
        let nested_commit = commit_all(&nested, "nested");

        let sup = crafted_superproject_with_tree(root.path(), |dir| {
            let gitmodules = hash_object(
                dir,
                &format!(
                    "[submodule \"b\"]\n\tpath = a/b\n\turl = {}\n",
                    nested.display()
                ),
            );
            let inner = git_with_input(
                dir,
                &["mktree", "--missing"],
                &format!("160000 commit {nested_commit}\tb\n"),
            );
            let link = hash_object(dir, victim.to_str().unwrap());

            let mut tree = Vec::new();
            for (mode, name, id) in [
                ("100644", ".gitmodules", &gitmodules),
                ("40000", "a", &inner),
                ("120000", "a", &link),
            ] {
                tree.extend_from_slice(format!("{mode} {name}\0").as_bytes());
                tree.extend(hex_to_bytes(id));
            }
            git_with_bytes(
                dir,
                &["hash-object", "-t", "tree", "-w", "--literally", "--stdin"],
                &tree,
            )
        });

        let out = root.path().join("out");
        let extra: Vec<&str> = mode.into_iter().collect();
        let run = plant(&sup, &out, &extra);

        let context = format!("with {mode:?}:\n{}", run.stderr);
        assert!(!run.ok, "accepted a duplicate name {context}");
        assert!(run.stderr.contains("twice"), "wrong message {context}");
        assert_eq!(
            std::fs::read_dir(&victim).unwrap().count(),
            0,
            "escaped {context}"
        );
        assert_eq!(
            leftovers(&out.join("vendor")),
            Vec::<String>::new(),
            "{context}"
        );
    }
}
