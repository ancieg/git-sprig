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

use std::{path::Path, process::Command};

use crate::common::*;

fn generated(args: &[&str]) -> String {
    // Outside any repository: generating needs none.
    let dir = tempfile::tempdir().unwrap();
    let output = git_sprig(args).current_dir(dir.path()).output().unwrap();
    assert!(
        output.status.success(),
        "{args:?}:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

#[test]
fn generates_the_manual_page() {
    let page = generated(&["--generate-man"]);
    assert!(page.contains(".TH "), "{page}");
    assert!(page.contains("\\-\\-stage"), "{page}");
    assert!(page.contains("\\-\\-revision"), "{page}");
    assert!(page.contains("\\-\\-exclude"), "{page}");
    assert!(page.contains("pathspec"), "{page}");
    assert!(page.contains("\\-\\-generate\\-completions"), "{page}");
    assert!(page.contains(".SH \"EXIT STATUS\"\n"), "{page}");
    assert!(
        page.find("EXIT STATUS") < page.find(".SH VERSION"),
        "{page}"
    );
}

#[test]
fn generates_completions_for_every_shell() {
    for (shell, expected) in [
        ("bash", "_git_sprig ()"),
        ("zsh", "#compdef git-sprig"),
        ("fish", "__fish_git_using_command sprig"),
    ] {
        let script = generated(&["--generate-completions", shell]);
        assert!(script.contains(expected), "{shell}:\n{script}");
        assert!(script.contains("stage"), "{shell}:\n{script}");
        assert!(script.contains("revision"), "{shell}:\n{script}");
        assert!(script.contains("exclude"), "{shell}:\n{script}");
    }
}

/// Completing `git sprig` through git's own bash completion, where it is installed.
#[test]
fn bash_completes_git_sprig_through_git() {
    let git_completion = Path::new("/usr/share/bash-completion/completions/git");
    if !git_completion.exists() {
        eprintln!("skipped: {} is not installed", git_completion.display());
        return;
    }
    let fx = Fixture::new();
    let script =
        std::env::temp_dir().join(format!("git-sprig-completion-{}.bash", std::process::id()));
    std::fs::write(&script, generated(&["--generate-completions", "bash"])).unwrap();

    let complete = |line: &str| {
        let program = format!(
            r#"
source {git}
source {script}
COMP_LINE={line:?}
COMP_POINT=${{#COMP_LINE}}
read -ra COMP_WORDS <<< "$COMP_LINE"
[ "${{COMP_LINE: -1}}" = " " ] && COMP_WORDS+=("")
COMP_CWORD=$(( ${{#COMP_WORDS[@]}} - 1 ))
__git_wrap__git_main
printf '%s\n' "${{COMPREPLY[@]}}"
"#,
            git = git_completion.display(),
            script = script.display(),
        );
        let output = Command::new("bash")
            .arg("-c")
            .arg(program)
            .current_dir(fx.superproject())
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .unwrap();
        String::from_utf8_lossy(&output.stdout).into_owned()
    };

    let options = complete("git sprig --st");
    let revisions = complete("git sprig --revision=ma");
    let shells = complete("git sprig --generate-completions ");
    std::fs::remove_file(&script).ok();

    assert!(
        options.lines().any(|word| word.trim() == "--stage"),
        "{options}"
    );
    assert!(
        revisions.lines().any(|word| word.trim() == "main"),
        "{revisions}"
    );
    assert!(shells.lines().any(|word| word.trim() == "fish"), "{shells}");
}
