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

//! The settings `git -c` hands down to an external command such as
//! `git sprig`, read back from `GIT_CONFIG_PARAMETERS` as git writes them.

/// The settings that `git -c` hands down to `git sprig` in
/// `GIT_CONFIG_PARAMETERS`, as `-c` arguments.
pub(super) fn inherited_overrides() -> anyhow::Result<Vec<String>> {
    match std::env::var("GIT_CONFIG_PARAMETERS") {
        Ok(parameters) => config_parameters(&parameters)
            .ok_or_else(|| anyhow::anyhow!("bogus format in GIT_CONFIG_PARAMETERS")),
        Err(std::env::VarError::NotPresent) => Ok(Vec::new()),
        Err(std::env::VarError::NotUnicode(_)) => {
            anyhow::bail!("GIT_CONFIG_PARAMETERS is not valid UTF-8")
        }
    }
}

/// Parse `GIT_CONFIG_PARAMETERS` as git does: shell-quoted `'key'='value'`
/// pairs, `'key'=` for an implicit true, or `'key=value'` from older git.
fn config_parameters(mut rest: &str) -> Option<Vec<String>> {
    let mut overrides = Vec::new();
    rest = rest.trim_start();
    while !rest.is_empty() {
        let (key, after) = sq_dequote(rest)?;
        rest = after;
        match rest.strip_prefix('=') {
            Some(after) if after.starts_with('\'') => {
                let (value, after) = sq_dequote(after)?;
                overrides.push(format!("{key}={value}"));
                rest = after;
            }
            Some(after) => {
                overrides.push(key);
                rest = after;
            }
            None => overrides.push(key),
        }
        if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
            return None;
        }
        rest = rest.trim_start();
    }
    Some(overrides)
}

/// One word quoted as git's `sq_quote` writes it: within `'…'`, a quote is
/// `'\''` and an exclamation mark `'\!'`.
fn sq_dequote(input: &str) -> Option<(String, &str)> {
    let mut rest = input.strip_prefix('\'')?;
    let mut word = String::new();
    loop {
        let end = rest.find('\'')?;
        word.push_str(&rest[..end]);
        rest = &rest[end + 1..];
        match rest.as_bytes() {
            [b'\\', quoted @ (b'\'' | b'!'), b'\'', ..] => {
                word.push(char::from(*quoted));
                rest = &rest[3..];
            }
            _ => return Some((word, rest)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_parameters_are_read_as_git_writes_them() {
        let parse = |text: &str| config_parameters(text).unwrap();
        assert_eq!(parse("'a.b'='1' 'c.d'='x y'"), ["a.b=1", "c.d=x y"]);
        assert_eq!(parse("'a.b=1'"), ["a.b=1"]);
        assert_eq!(parse("'a.b'= 'c.d'"), ["a.b", "c.d"]);
        assert_eq!(parse(r"'it'\''s.x'='a'\!'b'"), ["it's.x=a!b"]);
        assert_eq!(parse("  "), Vec::<String>::new());
    }

    #[test]
    fn bogus_config_parameters_are_refused() {
        for text in ["a.b=1", "'a.b", "'a.b'x", "'a.b'='1'x", "'a.b'=1"] {
            assert!(config_parameters(text).is_none(), "{text}");
        }
    }
}
