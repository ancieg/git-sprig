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

//! `--stage`: turning submodules into ordinary files of the superproject,
//! staged for a commit.
//!
//! This runs git itself rather than writing the index with gix: `git add`
//! applies exactly the attributes, modes and index extensions git will commit.

use std::{
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result, bail};
use gix::{Repository, bstr::BString};

use crate::{
    name::TreePath,
    report::{self, Reporter},
};

/// A submodule whose snapshot is staged: its name in `.gitmodules` and its path
/// in the work tree.
pub struct Submodule {
    pub name: BString,
    pub path: TreePath,
}

/// The superproject's work tree, which `basedir` has to be for `--stage`.
pub fn work_tree(repo: &Repository, basedir: &Path) -> Result<PathBuf> {
    let Some(work_tree) = repo.workdir() else {
        bail!(
            "--stage needs a work tree, but {} is a bare repository",
            report::quoted(repo.git_dir().display())
        );
    };
    let work_tree = work_tree.canonicalize().with_context(|| {
        format!(
            "cannot find the work tree {}",
            report::quoted(work_tree.display())
        )
    })?;
    if basedir.canonicalize().ok().as_ref() != Some(&work_tree) {
        bail!(
            "--stage changes the superproject's index, so the submodules have to be written \
             into its work tree: run from {} or pass --work-tree {}",
            report::quoted(work_tree.display()),
            report::quoted(work_tree.display())
        );
    }
    Ok(work_tree)
}

/// Replace each submodule in the index with the files now at its path, and
/// remove it from `.gitmodules`.
pub fn stage(work_tree: &Path, submodules: &[Submodule], reporter: &Reporter) -> Result<()> {
    let git = Git::in_tree(work_tree);
    for submodule in submodules {
        let mut pathspec = OsString::from(":(literal)");
        pathspec.push(submodule.path.as_path());
        git.run([
            OsStr::new("rm"),
            "-r".as_ref(),
            "--cached".as_ref(),
            "-q".as_ref(),
            "--ignore-unmatch".as_ref(),
            "--".as_ref(),
            pathspec.as_os_str(),
        ])?;
        // Forced: the snapshot is a committed tree, and ignore rules of the
        // superproject or of the snapshot itself must not drop files from it.
        git.run([
            OsStr::new("add"),
            "--force".as_ref(),
            "--".as_ref(),
            pathspec.as_os_str(),
        ])?;
        report::status!(
            reporter,
            "Submodule path {}: staged as plain files",
            report::quoted(submodule.path.display())
        );
    }
    forget_in_gitmodules(&git, submodules)
}

fn forget_in_gitmodules(git: &Git<'_>, submodules: &[Submodule]) -> Result<()> {
    let file = git.work_tree.join(".gitmodules");
    if !file.exists() {
        return Ok(());
    }

    // Read the configured keys once; removing sections below only ever drops
    // the keys of our own submodules, so what is left can be worked out here.
    let keys = git.run(["config", "--file", ".gitmodules", "--name-only", "--list"])?;
    // A key is `<section>.<variable>`. Variable names hold no dots but submodule
    // names may, so `submodule.foo.bar.path` belongs to `foo.bar`, not to `foo`:
    // compare whole sections, never prefixes.
    let sections: Vec<&str> = keys
        .lines()
        .map(|key| key.rsplit_once('.').map_or(key, |(section, _)| section))
        .collect();
    let mut removed = Vec::new();
    for submodule in submodules {
        let section = format!("submodule.{}", submodule.name);
        if sections.contains(&section.as_str()) {
            git.run([
                "config",
                "--file",
                ".gitmodules",
                "--remove-section",
                &section,
            ])?;
            removed.push(section);
        }
    }

    let empty = sections
        .iter()
        .all(|section| removed.iter().any(|gone| gone == section));
    if empty {
        git.run([
            "rm",
            "-q",
            "--force",
            "--ignore-unmatch",
            "--",
            ".gitmodules",
        ])?;
        if file.exists() {
            std::fs::remove_file(&file)
                .with_context(|| format!("cannot remove {}", report::quoted(file.display())))?;
        }
    } else {
        git.run(["add", "--", ".gitmodules"])?;
    }
    Ok(())
}

/// Runs `git -C <work_tree> …`, the CLI `--stage` drives instead of gix so the
/// index ends up exactly as `git add` would leave it.
struct Git<'a> {
    work_tree: &'a Path,
}

impl<'a> Git<'a> {
    fn in_tree(work_tree: &'a Path) -> Self {
        Self { work_tree }
    }

    fn run(&self, args: impl IntoIterator<Item = impl AsRef<OsStr>>) -> Result<String> {
        let args: Vec<OsString> = args
            .into_iter()
            .map(|arg| arg.as_ref().to_owned())
            .collect();
        let output = Command::new("git")
            .arg("-C")
            .arg(self.work_tree)
            .args(&args)
            .output()
            .context("cannot run git, which --stage needs")?;
        if !output.status.success() {
            let shown: Vec<_> = args.iter().map(|arg| arg.to_string_lossy()).collect();
            bail!(
                "git {} failed: {}",
                shown.join(" "),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }
}
