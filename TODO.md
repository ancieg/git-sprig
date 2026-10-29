# TODO

## Performance

- [ ] **Reuse HTTP connections.** Every submodule opens a new connection.
      The saving would be modest, because the server's response time dominates
      what a fetch costs and parallel jobs hide most of the rest. git-sprig
      cannot do it safely today either: gix's reqwest backend
      (`gix-transport/src/client/blocking_io/http/reqwest/`) keeps the base URL
      of the last redirect in private state that is never reset, and gix's HTTP
      transport rewrites its URL from it. A backend shared between repositories
      could send one repository's requests to another's redirect target. Needs
      gix to let a client be shared, or that state be reset.
- [ ] **Make the fallback cheaper.** Servers that refuse a single commit make
      git-sprig fetch every branch and tag with full history. A fetch without
      blobs (`--filter=blob:none`), followed by fetching just the needed blobs,
      would shrink that, once gix supports filters (PR #2375, see Upstream).

## Upstream: gitoxide

Many feature issues are closed and moved to "Feature Proposal" discussions
under gitoxide's contribution process. There, closed means under review, not
rejected or done.

| What git-sprig needs | Status in gitoxide | What git-sprig does meanwhile |
|---|---|---|
| Streaming a blob without loading it whole | Issue [#1595](https://github.com/GitoxideLabs/gitoxide/issues/1595), moved to discussion [#2789](https://github.com/GitoxideLabs/gitoxide/discussions/2789), where there is no activity | Inflates whole packed blobs and loose blobs straight from storage (`Objects` in [tree/objects.rs](src/tree/objects.rs)) |
| Bounded memory while receiving and indexing a pack | Issue [#2527](https://github.com/GitoxideLabs/gitoxide/issues/2527), closed; the move to a discussion failed, so none exists | Nothing; the fetch peaks at about the largest file's size |
| Partial fetches with filters such as `blob:none` | Issue [#1046](https://github.com/GitoxideLabs/gitoxide/issues/1046), moved to discussion [#2791](https://github.com/GitoxideLabs/gitoxide/discussions/2791); PR [#2375](https://github.com/GitoxideLabs/gitoxide/pull/2375) open and unmerged | The full fallback fetch |
| Submodules when archiving or cloning | Issue [#1182](https://github.com/GitoxideLabs/gitoxide/issues/1182) (gix-archive), moved to discussion [#2769](https://github.com/GitoxideLabs/gitoxide/discussions/2769); issue [#2387](https://github.com/GitoxideLabs/gitoxide/issues/2387) (clone) closed with nothing planned | git-sprig is that tool |
| Asking the filter pipeline whether a path would be converted | Nothing found | `line_endings` in [tree/convert.rs](src/tree/convert.rs) mirrors gix-filter's private `Configuration::at_path`; a test compares the result with `git clone` |
| An HTTP client shared by several fetches | Nothing found | Nothing (see Performance) |
| Interrupting a fetch blocked on a read | Nothing found; issue [#24](https://github.com/GitoxideLabs/gitoxide/issues/24), about being killed midway, is closed | After Ctrl-C, a watchdog removes temporary and staging directories and exits if work has not wound down within 3 s |
| Parallel checkout | Exists: `gix-worktree-state` `checkout` with `thread_limit` | Not used: it would bypass git-sprig's streaming and staging, so git-sprig writes in parallel itself |
| `submodule.<name>.active` | Exists: `gix_submodule::File::is_active_platform` | Not used: it reads the whole configuration, while a direct export has to leave out the source repository's own settings, as `git clone` does. git-sprig applies the same rules itself, except the last one (active only with a configured `url`), which only holds after `git submodule init` |

- [ ] Describe git-sprig's approach in discussion #2789: blobs stored whole are
      inflated from `pack::data::File::entry_slice` with `gix::zlib::Decompress`.
      It is a small, concrete first step towards streaming.
- [ ] Open proposals for the gaps with nothing upstream: a public "would this
      path be converted" query on `gix::filter::Pipeline`, a shareable HTTP
      client (or a way to reset the redirect state), and network reads that
      notice the interrupt flag.
- [ ] Follow PR #2375 and build the cheaper fallback once it is merged.
