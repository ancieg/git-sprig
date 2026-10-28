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

//! The manual page and shell completions, generated from the command line
//! definition so they always match the installed program.

use anyhow::Result;
use clap::{Arg, Command, ValueEnum};
use clap_complete::Shell;

/// The statuses `main` and `cancel` exit with, which are git's.
const EXIT_STATUS: &str = r#".SH "EXIT STATUS"
.TP
0
Success.
.TP
128
A fatal error, reported after \fBfatal:\fR.
.TP
129
A wrong command line.
.TP
130
Interrupted, for example by Ctrl\-C.
"#;

pub fn man(command: Command) -> Result<Vec<u8>> {
    let mut page = Vec::new();
    clap_mangen::Man::new(command).render(&mut page)?;
    let mut page = String::from_utf8(page)?;
    let at = page.find(".SH VERSION").unwrap_or(page.len());
    page.insert_str(at, EXIT_STATUS);
    Ok(page.into_bytes())
}

/// The shells git-sprig writes completions for. `clap_complete` knows two more
/// that git-sprig does not offer: PowerShell, which is not a Unix shell, and
/// Elvish, which too few people use to be worth carrying.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum CompletionShell {
    Bash,
    Fish,
    Zsh,
}

impl From<CompletionShell> for Shell {
    fn from(shell: CompletionShell) -> Self {
        match shell {
            CompletionShell::Bash => Self::Bash,
            CompletionShell::Fish => Self::Fish,
            CompletionShell::Zsh => Self::Zsh,
        }
    }
}

/// Completions for `git-sprig`, plus hooks so that `git sprig` completes through
/// the shell's git completion, where that completion offers a way in.
pub fn completions(shell: CompletionShell, mut command: Command) -> Vec<u8> {
    let name = command.get_name().to_owned();
    let mut script = Vec::new();
    clap_complete::generate(Shell::from(shell), &mut command, name, &mut script);
    match shell {
        CompletionShell::Bash => script.extend_from_slice(bash_git_hook(&command).as_bytes()),
        CompletionShell::Fish => script.extend_from_slice(fish_git_hook(&command).as_bytes()),
        // zsh's git completion calls `_git-sprig`, the function clap already generates.
        CompletionShell::Zsh => {}
    }
    script
}

fn options(command: &Command) -> impl Iterator<Item = &Arg> {
    command.get_arguments().filter(|arg| {
        arg.get_long().is_some()
            && !arg.is_hide_set()
            && !matches!(arg.get_id().as_str(), "help" | "version")
    })
}

fn shell_names() -> String {
    CompletionShell::value_variants()
        .iter()
        .filter_map(ValueEnum::to_possible_value)
        .map(|value| value.get_name().to_owned())
        .collect::<Vec<_>>()
        .join(" ")
}

/// git's bash completion calls `_git_<command>` for commands it runs from PATH.
fn bash_git_hook(command: &Command) -> String {
    let words: Vec<String> = options(command)
        .map(|arg| {
            let long = arg.get_long().unwrap_or_default();
            if arg.get_action().takes_values() {
                format!("--{long}=")
            } else {
                format!("--{long}")
            }
        })
        .collect();
    let shells = shell_names();
    format!(
        r#"
# `git sprig`: git's completion calls _git_<command> for commands in PATH.
_git_sprig ()
{{
	case "$prev" in
	--revision)
		__git_complete_refs
		return
		;;
	--generate-completions)
		__gitcomp "{shells}"
		return
		;;
	esac
	case "$cur" in
	--revision=*)
		__git_complete_refs --cur="${{cur##--revision=}}"
		return
		;;
	--generate-completions=*)
		__gitcomp "{shells}" "" "${{cur##--generate-completions=}}"
		return
		;;
	--*=*)
		return
		;;
	-*)
		__gitcomp "{words}"
		return
		;;
	esac
}}
"#,
        words = words.join(" ")
    )
}

/// fish's git completion offers each subcommand's options behind a
/// `__fish_git_using_command` condition.
fn fish_git_hook(command: &Command) -> String {
    let mut hook = String::from("\n# `git sprig`\n");
    for arg in options(command) {
        let mut line = vec![
            "complete -c git -n '__fish_git_using_command sprig'".to_owned(),
            format!("-l {}", arg.get_long().unwrap_or_default()),
        ];
        if let Some(short) = arg.get_short() {
            line.push(format!("-s {short}"));
        }
        let values: Vec<String> = arg
            .get_possible_values()
            .iter()
            .map(|value| value.get_name().to_owned())
            .collect();
        if !values.is_empty() {
            line.push(format!("-x -a '{}'", values.join(" ")));
        } else if arg.get_action().takes_values() {
            line.push("-r".to_owned());
        }
        if let Some(help) = arg.get_help() {
            let help = help.to_string().replace('\\', "\\\\").replace('\'', "\\'");
            line.push(format!("-d '{help}'"));
        }
        hook.push_str(&line.join(" "));
        hook.push('\n');
    }
    hook
}
