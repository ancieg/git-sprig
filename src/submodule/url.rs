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

//! Resolving submodule URLs; relative ones follow the superproject's URL.

use anyhow::{Result, bail};
use gix::{
    Url,
    bstr::{BStr, BString, ByteSlice, ByteVec},
};

use crate::report;

/// Resolve a submodule URL; relative ones are resolved against the
/// superproject's URL, as `git submodule` does.
pub fn resolve_url(raw: &BStr, parent: Option<&Url>) -> Result<Url> {
    if !is_relative(raw) {
        // The caller names the url and the submodule it belongs to.
        return Ok(gix::url::parse(raw)?);
    }

    let Some(parent) = parent else {
        bail!("the superproject has no remote (pass --remote-url)");
    };

    let mut segments: Vec<&[u8]> = parent
        .path
        .trim_end_with(|c| c == '/')
        .split_str("/")
        .collect();
    let absolute = segments.first().is_some_and(|first| first.is_empty());
    if absolute {
        segments.remove(0);
    }

    for component in raw.split_str("/") {
        match component {
            b"" | b"." => {}
            b".." => {
                if segments.pop().is_none() {
                    bail!(
                        "relative url {} escapes the base url {}",
                        report::quoted(raw),
                        report::quoted(parent)
                    );
                }
            }
            other => segments.push(other),
        }
    }

    let mut path = BString::from(Vec::new());
    if absolute {
        path.push_byte(b'/');
    }
    path.extend_from_slice(segments.join(&b'/').as_slice());

    let mut url = parent.clone();
    url.path = path;
    Ok(url)
}

pub fn is_relative(url: &BStr) -> bool {
    url.starts_with(b"./") || url.starts_with(b"../") || url == "." || url == ".."
}

#[cfg(test)]
mod tests {
    use super::*;
    fn resolve(parent: &str, rel: &str) -> String {
        let parent = gix::url::parse(parent).unwrap();
        resolve_url(rel.into(), Some(&parent))
            .unwrap()
            .to_bstring()
            .to_string()
    }

    #[test]
    fn relative_urls_follow_the_superproject() {
        assert_eq!(
            resolve("https://github.com/astral-sh/ty.git", "../ruff.git"),
            "https://github.com/astral-sh/ruff.git"
        );
        assert_eq!(
            resolve("https://github.com/astral-sh/ty", "./vendor/x"),
            "https://github.com/astral-sh/ty/vendor/x"
        );
        assert_eq!(
            resolve("https://host/a/b/c.git", "../../d.git"),
            "https://host/a/d.git"
        );
        assert_eq!(
            resolve("git@github.com:astral-sh/ty.git", "../ruff.git"),
            "git@github.com:astral-sh/ruff.git"
        );
    }

    #[test]
    fn absolute_urls_pass_through() {
        let url = resolve_url("https://example.com/x.git".into(), None).unwrap();
        assert_eq!(url.to_bstring().to_string(), "https://example.com/x.git");
    }

    #[test]
    fn escaping_the_base_is_an_error() {
        let parent = gix::url::parse("https://host/a.git").unwrap();
        assert!(resolve_url("../../b.git".into(), Some(&parent)).is_err());
        assert!(resolve_url("../b.git".into(), None).is_err());
    }
}
