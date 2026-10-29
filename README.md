# git-sprig

**git-sprig materializes Git repositories and their submodules as plain files.**
It can export a complete repository tree directly, or fill in the submodules of
an existing project. Every tree is written at exactly the requested or pinned
commit, recursively. No `.git` folders are left behind — you get just the source
code.

That comes in handy whenever you need a project's complete sources but not its
repositories: release tarballs, distribution packages, container images, CI
builds, or copying a dependency into another tree.

## Why not `git submodule update`?

`git submodule update --init --recursive` does fill the folders in, but it
clones each submodule as a real repository: history is downloaded, and `.git`
files are scattered all over the tree for you to clean up afterwards. git-sprig asks
the server for nothing but the one commit each submodule needs, writes the
files, and throws the downloaded data away.

## Manual page and shell completions

Installing git-sprig gives you the command and nothing more. It prints its
manual page, which `git sprig --help` opens, and its shell completions on
request.

Install the manual page for yourself:

```console
$ mkdir -p ~/.local/share/man/man1
$ git-sprig --generate-man > ~/.local/share/man/man1/git-sprig.1
```

If `man git-sprig` still finds nothing, add
`export MANPATH="$HOME/.local/share/man:"` to your shell's startup file. To
install it for all users, write it to `/usr/local/share/man/man1/git-sprig.1`
instead. Without the manual page, `git sprig -h` and `git-sprig --help` still
show the built-in help.

Shell completions work for both `git-sprig` and `git sprig`:

- **bash**:
  `git-sprig --generate-completions bash > ~/.local/share/bash-completion/completions/git-sprig`,
  then add `source ~/.local/share/bash-completion/completions/git-sprig` to
  `~/.bashrc`. Loading it from `~/.bashrc` is what makes `git sprig` complete;
  bash-completion on its own only loads the file when you type `git-sprig`.
- **zsh**: `git-sprig --generate-completions zsh > ~/.zfunc/_git-sprig`, with
  `~/.zfunc` added to `fpath` before `compinit` runs.
- **fish**:
  `git-sprig --generate-completions fish > ~/.config/fish/conf.d/git-sprig.fish`.

## Quick start

Export a complete project, including its submodules, straight from a repository:

```console
$ git-sprig -b v1.4.0 https://example.com/project.git project-1.4.0
```

The destination contains the project's own files as well as its submodules,
with no repository history. It is replaced as a whole only after the complete
tree has been prepared successfully.

To fill in only the submodules of an existing checkout, clone a project and run
git-sprig inside it:

```console
$ git clone https://example.com/project.git
$ cd project
$ git-sprig
Fetching submodule 'vendor/libfoo' from 'https://example.com/libfoo.git' (1/2)
Fetching submodule 'vendor/libbar' from 'https://example.com/libbar.git' (2/2)
Submodule path 'vendor/libbar': planted '51c0e3a8d2f47b1c9e6a0d3f5b8c2e7a14d9f063'
Fetching submodule 'vendor/libfoo/third_party/zlib' from 'https://example.com/zlib.git' (3/3)
Submodule path 'vendor/libfoo': planted '3ad07471008c9e0e4ab1d2f77f4c1b1e5a4d9c71'
Submodule path 'vendor/libfoo/third_party/zlib': planted '9f1c2e04b7d3a8e6c5f40b19a7d2e8c3f6b5a401'
```

The submodule folders now contain ordinary files. The messages follow git's own
style, like those of `git submodule update`. The total in `(3/3)` grows while
git-sprig runs, because submodules inside a submodule only become known once
that submodule has been downloaded. A submodule is put in place together with
everything inside it, which is why `vendor/libfoo` is reported last.

## Recipes

**A complete source tarball.** Use direct mode, then archive the resulting tree:

```console
$ git-sprig project project-1.4.0 --revision v1.4.0
$ tar -czf project-1.4.0.tar.gz project-1.4.0
```

**Commit a snapshot of the submodules** into the project itself, as ordinary
files, from anywhere in the project's work tree:

```console
$ git-sprig --stage
$ git commit -m "Vendor submodules"
```

`--stage` replaces each submodule in the index with its files and removes it
from `.gitmodules`; nothing is committed until you do.

**Check what would be downloaded** without changing anything:

```console
$ git-sprig --dry-run
```

**Download even faster.** git-sprig already fetches up to 8 submodules at once;
projects with many small submodules gain from more:

```console
$ git-sprig --jobs 16
```

**Skip nested submodules** and take only the first level:

```console
$ git-sprig --no-recursive
```

**Skip a submodule** you don't need, the way `git submodule` would:

```console
$ git config submodule.vendor/libbar.active false
$ git-sprig
```

Submodules that `.gitmodules` marks with `update = none` are skipped as well,
as `git submodule update` skips them; `-c submodule.<name>.update=checkout`
brings one back.

**Select top-level submodules** with Git pathspecs, relative to the current
directory as in git. Put includes after `--`, or repeat `--exclude` to skip
matching paths and globs:

```console
$ git-sprig -- vendor/libfoo
$ git-sprig --exclude 'vendor/deprecated*'
```

Direct mode accepts the same filters. They apply only to top-level submodules;
included submodules keep their normal recursive behavior:

```console
$ git-sprig project project-snapshot --revision v1.4.0 -- vendor/libfoo
```

**Relative submodule URLs** (such as `../libfoo.git`) are resolved against the
project's remote. Without one, git-sprig does what git does and resolves them
against the project's own path, with a warning. To resolve them elsewhere:

```console
$ git-sprig --remote-url https://example.com/project.git
```

**Fetch a submodule from a mirror** by configuring its URL, which takes
precedence over `.gitmodules` as it does for `git submodule`. In direct mode,
only `-c` and your user or system configuration count, as for `git clone`:

```console
$ git config submodule.vendor/libfoo.url https://mirror.example.com/libfoo.git
$ git-sprig project out -c submodule.vendor/libfoo.url=https://mirror.example.com/libfoo.git
```

**Go through a proxy**, or pass any other git setting, with `-c` before or
after `sprig`, as with any git command:

```console
$ git-sprig -c http.proxy=http://proxy.example.com:3128
$ git -c http.proxy=http://proxy.example.com:3128 sprig
```

## Options

| Option | What it does |
|---|---|
| `<REPOSITORY> <DIRECTORY>` | Materialize the complete root tree of a local or remote repository into the required destination. Cannot be combined with `--work-tree` or `--stage`. |
| `-C <DIR>` | Run as if git-sprig was started in `<DIR>`, like `git -C`. May be repeated. |
| `-b, --revision <REV>` | Which commit, branch or tag of the source to use. Default: the index, as for `git submodule update`, or `HEAD` of `<REPOSITORY>`. `--branch` names it as in `git clone`. |
| `--work-tree <DIR>` | Where to write submodules without `<REPOSITORY>`. Default: the top of the work tree, wherever in it git-sprig runs; `git --git-dir=… --work-tree=… sprig` works as in git. |
| `[-- <PATHSPEC>...]` | Include only matching top-level submodules. Without includes, every submodule is eligible. |
| `--exclude <PATHSPEC>` | Exclude matching top-level submodules, including glob patterns. May be repeated. |
| `--recurse-submodules[=<PATHSPEC>]` | Materialize submodules, as by default, like `git clone --recurse-submodules`; a pathspec limits the top-level ones, as after `--`. |
| `--no-recurse-submodules` | Export `<REPOSITORY>` alone, without its submodules. |
| `-j, --jobs <N>` | How many submodules to download at the same time; 0 picks the default. Default: `submodule.fetchJobs` as for git, or else the number of CPUs, at most 8. |
| `--no-recursive` | Don't go into submodules of submodules. |
| `--max-depth <N>` | How many levels of submodules to fetch. Default: 32; 0 means no limit. The direct root does not count as a level. |
| `--remote-url <URL>` | Where relative submodule URLs are resolved from. Default: the remote, or the project itself without one, or `<REPOSITORY>`. |
| `-c, --config <KEY=VALUE>` | A git setting for this run, like `git -c`: for the superproject, such as `submodule.<name>.active`, and for the downloads. |
| `--raw, --no-filters` | Write files exactly as stored, without line ending conversion or filters. |
| `-f, --force` | Replace destinations even if they are or contain git checkouts. |
| `--stage` | Replace the submodules in the project's index with their files, ready to commit. |
| `--tempdir <DIR>` | Where to keep downloads while working. |
| `-n, --dry-run` | Show what would be fetched and stop without changing the destination. |
| `-q, --quiet` | Print only errors. |
| `-v, --verbose` | Print more about each repository and submodule. |
| `--generate-man` | Print the manual page and exit. |
| `--generate-completions <SHELL>` | Print shell completions (bash, zsh, fish) and exit. |

## Good to know

- **Destinations are replaced completely.** In direct mode, the whole positional
  `<DIRECTORY>` is replaced, so files from an older revision cannot linger.
  Without a positional repository, only submodule folders are replaced; the
  project's own files are never touched, except the index and `.gitmodules`
  when you pass `--stage`.
- **Existing checkouts are protected.** If a submodule folder is a real git
  checkout with its own `.git`, or a root destination is one, has one anywhere
  inside it or holds the local source repository, git-sprig stops before
  changing anything, so you can't lose work by accident. Pass `--force` to
  replace it anyway.
- **Nothing is left half-done.** A destination is swapped in only after its
  complete tree has been prepared. In direct mode this includes all requested
  submodules. If a download fails or you press Ctrl-C, the old contents stay as
  they were, even when a server has stopped answering.
- **Files look exactly like a git checkout**, with the same line endings and
  permissions.
- **Older servers work too, just slower.** Some servers refuse to hand out a
  single commit. git-sprig then downloads the whole repository instead and tells
  you so.
- **Untrusted repositories can't escape.** A repository crafted to write files
  outside the target folder or into `.git` is refused, and one whose tree nests
  deeper than git's own `core.maxTreeDepth` allows is refused rather than
  followed.
- **Inactive submodules are skipped.** Like `git submodule update --init`,
  git-sprig honours `submodule.<name>.active` and `submodule.active` from your git
  configuration, and fetches every other submodule.
- **Big files are unpacked straight to disk** wherever git would write them
  unchanged, so writing them takes almost no memory.
- All messages go to stderr. The exit status is the one git would use: 0 on
  success, 128 after a `fatal:` message, 129 for a wrong command line and 130
  when interrupted.

## Limitations

- Unix only.
- Downloading a submodule briefly needs about as much memory as its largest
  file, because that is how gitoxide receives data.
- git-sprig has no login options of its own. Private repositories work as far as
  [gitoxide](https://github.com/GitoxideLabs/gitoxide), which git-sprig is built on,
  handles your SSH keys and git credential settings.

## How it works

For each remote repository, git-sprig creates a temporary repository and asks
the server for only the requested or pinned commit, with no history. It writes
files into a hidden `.git-sprig-*` folder next to the destination, does the same
for the submodules inside, and finally moves the folder into place. In direct
mode, the root and all of its submodules share one staging folder, so the root
destination is replaced only once. Local direct sources are read in place.
Temporary repositories are deleted as soon as their files are written, and
submodules that pin the same commit share one download.

## Development

```console
$ cargo test                  # offline tests
$ cargo test -- --ignored     # also compare with git on github.com
$ cargo clippy --all-targets  # pedantic lints are enabled in Cargo.toml
```

GitHub Actions runs the offline tests on both Rust 1.99, the oldest version
git-sprig supports, and stable, along with `cargo fmt --check` and clippy.

## License

git-sprig is free software: you can redistribute it and/or modify it under the terms
of the GNU General Public License as published by the Free Software Foundation,
either version 3 of the License, or (at your option) any later version. See
[LICENSE](LICENSE) for the full text.
