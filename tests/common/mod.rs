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

#![allow(
    dead_code,
    reason = "compiled into both the e2e and network test binaries, each of which uses a different part of it"
)]

use std::{
    io::Write,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

pub fn git(dir: &Path, args: &[&str]) -> String {
    git_with_bytes(dir, args, b"")
}

pub fn git_with_input(dir: &Path, args: &[&str], input: &str) -> String {
    git_with_bytes(dir, args, input.as_bytes())
}

pub fn git_with_bytes(dir: &Path, args: &[&str], input: &[u8]) -> String {
    let mut child = isolated(Command::new("git"))
        .current_dir(dir)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("git must be installed");
    child.stdin.take().unwrap().write_all(input).unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "git {args:?} in {} failed:\n{}\n{}",
        dir.display(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Keeps the developer's git configuration out of the tests.
fn isolated(mut command: Command) -> Command {
    command
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .env("GIT_AUTHOR_DATE", "2020-01-01T00:00:00Z")
        .env("GIT_COMMITTER_DATE", "2020-01-01T00:00:00Z");
    command
}

/// A repository on `main` that serves fetches by object id.
pub fn init_repo(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    git(dir, &["init", "-q", "-b", "main"]);
    git(dir, &["config", "uploadpack.allowAnySHA1InWant", "true"]);
    git(dir, &["config", "protocol.file.allow", "always"]);
}

pub fn write(dir: &Path, rel: &str, contents: &str) {
    let path = dir.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, contents).unwrap();
}

pub fn write_exec(dir: &Path, rel: &str, contents: &str) {
    write(dir, rel, contents);
    let path = dir.join(rel);
    let mut perms = std::fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms).unwrap();
}

pub fn symlink(dir: &Path, rel: &str, target: &str) {
    let path = dir.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(target, path).unwrap();
}

pub fn commit_all(dir: &Path, message: &str) -> String {
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-q", "-m", message]);
    git(dir, &["rev-parse", "HEAD"])
}

pub fn add_submodule(super_dir: &Path, url: &Path, path: &str) {
    git(
        super_dir,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            "-q",
            url.to_str().unwrap(),
            path,
        ],
    );
}

pub fn hash_object(dir: &Path, contents: &str) -> String {
    git_with_input(dir, &["hash-object", "-w", "--stdin"], contents)
}

pub fn hex_to_bytes(hex: &str) -> Vec<u8> {
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
        .collect()
}

/// A superproject whose submodule `vendor/sub` pins a commit with a hand-built
/// tree; `tree` returns `git mktree` input.
pub fn crafted_superproject(root: &Path, tree: impl FnOnce(&Path) -> String) -> PathBuf {
    crafted_superproject_with_tree(root, |dir| {
        let listing = tree(dir);
        git_with_input(dir, &["mktree"], &listing)
    })
}

/// Like [`crafted_superproject`], with `tree` writing the tree object itself.
pub fn crafted_superproject_with_tree(root: &Path, tree: impl FnOnce(&Path) -> String) -> PathBuf {
    let sub = root.join("remotes/crafted");
    init_repo(&sub);
    let tree_id = tree(&sub);
    let commit = git(&sub, &["commit-tree", &tree_id, "-m", "crafted"]);
    git(&sub, &["update-ref", "refs/heads/main", &commit]);

    let sup = root.join("remotes/crafted-super");
    init_repo(&sup);
    write(&sup, "super.txt", "superproject\n");
    write(
        &sup,
        ".gitmodules",
        &format!(
            "[submodule \"sub\"]\n\tpath = vendor/sub\n\turl = {}\n",
            sub.display()
        ),
    );
    git(&sup, &["add", "-A"]);
    git(
        &sup,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{commit},vendor/sub"),
        ],
    );
    git(&sup, &["commit", "-q", "-m", "pin crafted"]);
    sup
}

pub fn git_sprig(args: &[&str]) -> Command {
    let mut command = isolated(Command::new(env!("CARGO_BIN_EXE_git-sprig")));
    command.args(args);
    command
}

pub struct Run {
    pub ok: bool,
    pub code: Option<i32>,
    pub stderr: String,
}

impl Run {
    /// Panics at the caller's line, so a failure points at the test, not here.
    #[track_caller]
    pub fn assert_ok(&self) -> &Self {
        assert!(self.ok, "git-sprig failed:\n{}", self.stderr);
        self
    }

    #[track_caller]
    pub fn assert_failed(&self) -> &Self {
        assert!(
            !self.ok,
            "git-sprig unexpectedly succeeded:\n{}",
            self.stderr
        );
        self
    }

    #[track_caller]
    pub fn assert_code(&self, code: i32) -> &Self {
        assert_eq!(self.code, Some(code), "{}", self.stderr);
        self
    }

    #[track_caller]
    pub fn assert_says(&self, needle: &str) -> &Self {
        assert!(
            self.stderr.contains(needle),
            "expected {needle:?} in:\n{}",
            self.stderr
        );
        self
    }

    #[track_caller]
    pub fn assert_silent_about(&self, needle: &str) -> &Self {
        assert!(
            !self.stderr.contains(needle),
            "unexpected {needle:?} in:\n{}",
            self.stderr
        );
        self
    }

    /// gix appends the place in its sources where an error was raised unless asked not to.
    #[track_caller]
    pub fn assert_no_source_locations(&self) -> &Self {
        self.assert_silent_about(".rs:")
    }
}

/// Run `git <args>` in `dir` with the git-sprig under test first on `PATH`,
/// so `git sprig` reaches it the way users invoke it.
pub fn through_git(dir: &Path, args: &[&str]) -> Run {
    let bin = Path::new(env!("CARGO_BIN_EXE_git-sprig")).parent().unwrap();
    let mut paths = vec![bin.to_owned()];
    paths.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    let out = isolated(Command::new("git"))
        .current_dir(dir)
        .env("PATH", std::env::join_paths(paths).unwrap())
        .args(args)
        .output()
        .expect("git must be installed");
    Run {
        ok: out.status.success(),
        code: out.status.code(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

pub fn run(args: &[&str]) -> Run {
    run_in(Path::new("."), args)
}

/// Run git-sprig with `dir` as the current directory.
pub fn run_in(dir: &Path, args: &[&str]) -> Run {
    let out = git_sprig(args)
        .current_dir(dir)
        .output()
        .expect("binary must be built");
    Run {
        ok: out.status.success(),
        code: out.status.code(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

/// Run git-sprig on the superproject `repo`, writing to `out`.
pub fn plant(repo: &Path, out: &Path, extra: &[&str]) -> Run {
    let mut args = vec![
        "-C",
        repo.to_str().unwrap(),
        "--work-tree",
        out.to_str().unwrap(),
    ];
    args.extend(extra);
    run(&args)
}

/// Materialize the complete tree of `source` at `rev` into `out`.
pub fn direct(source: &str, out: &Path, rev: &str, extra: &[&str]) -> Run {
    let mut args = vec![source, out.to_str().unwrap(), "--revision", rev];
    args.extend(extra);
    run(&args)
}

pub fn read(path: &Path) -> String {
    std::fs::read_to_string(path)
        .unwrap_or_else(|err| panic!("cannot read {}: {err}", path.display()))
}

pub fn is_executable(path: &Path) -> bool {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o111 != 0
}

/// Staging directories git-sprig left behind in `dir`.
pub fn leftovers(dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with(".git-sprig-"))
        .collect()
}

/// Path, kind, mode and content hash of every entry below `dir`, sorted.
pub fn tree_listing(dir: &Path) -> Vec<String> {
    fn walk(dir: &Path, prefix: &Path, out: &mut Vec<String>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let name = entry.file_name();
            if name == ".git" {
                continue;
            }
            let path = entry.path();
            let rel = prefix.join(&name);
            let meta = std::fs::symlink_metadata(&path).unwrap();
            let mode = meta.permissions().mode() & 0o7777;

            if meta.file_type().is_symlink() {
                let target = std::fs::read_link(&path).unwrap();
                out.push(format!("{} symlink {}", rel.display(), target.display()));
            } else if meta.is_dir() {
                out.push(format!("{} dir {mode:o}", rel.display()));
                walk(&path, &rel, out);
            } else {
                let data = std::fs::read(&path).unwrap();
                out.push(format!(
                    "{} file {mode:o} {} {:x}",
                    rel.display(),
                    data.len(),
                    fnv1a(&data)
                ));
            }
        }
    }

    let mut out = Vec::new();
    walk(dir, Path::new(""), &mut out);
    out.sort();
    out
}

fn fnv1a(data: &[u8]) -> u64 {
    data.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

/// `super` pins `vendor/child`, which pins `third_party/grand`. Both pins lag
/// behind their branch tips.
pub struct Fixture {
    pub root: tempfile::TempDir,
}

impl Fixture {
    pub fn path(&self, rel: &str) -> PathBuf {
        self.root.path().join(rel)
    }

    pub fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let r = root.path();

        let grand = r.join("remotes/grand");
        init_repo(&grand);
        write(&grand, "g.txt", "grandchild\n");
        commit_all(&grand, "grand");

        let child = r.join("remotes/child");
        init_repo(&child);
        write(&child, "hello.txt", "child\n");
        write(&child, "deep/nested/file.txt", "nested\n");
        write_exec(&child, "bin/run.sh", "#!/bin/sh\necho hi\n");
        symlink(&child, "link", "hello.txt");
        write(&child, ".gitattributes", "crlf.txt text eol=crlf\n");
        write(&child, "crlf.txt", "dos\nlines\n");
        commit_all(&child, "child");
        add_submodule(&child, &grand, "third_party/grand");
        commit_all(&child, "child with submodule");

        write(&grand, "future.txt", "not pinned\n");
        commit_all(&grand, "grand moves on");

        let sup = r.join("remotes/super");
        init_repo(&sup);
        write(&sup, "super.txt", "superproject\n");
        commit_all(&sup, "super");
        add_submodule(&sup, &child, "vendor/child");
        commit_all(&sup, "super with submodule");

        write(&child, "future.txt", "not pinned\n");
        commit_all(&child, "child moves on");

        Fixture { root }
    }

    pub fn superproject(&self) -> PathBuf {
        self.path("remotes/super")
    }
}
