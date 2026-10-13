# Spike: a built-in git for projj

Could projj write the git index itself, for machines with no `git`, and get
the same result git would? This compares two pure-Rust libraries against real
git: [gitoxide](https://github.com/GitoxideLabs/gitoxide) (`gix` 0.85,
already in projj through jj-lib) and
[grit-lib](https://github.com/gitbutlerapp/grit) 0.5.0. It is not part of
projj's build.

```sh
cargo run --release              # every scenario
cargo run --release -- sparse    # those whose name contains "sparse"
```

## What is compared

projj writes the index three ways, and each engine implements all three
(`src/engines.rs`):

- **set**: stage exactly this content for a path, or take the path out. This
  is what `git apply --cached` of a hunk or some of its lines comes to, since
  projj knows the content the index should end up with. Git's version:
  `hash-object -w --stdin` and `update-index --cacheinfo` / `--force-remove`.
- **add**: `git add --all -- <paths>`: whole files from the worktree, through
  clean filters and line-ending conversion, with modes, symlinks, deletions
  and conflicts.
- **reset**: `git reset -- <paths>`.

Every scenario (`src/main.rs`) builds the same repository from scratch once
per engine (fixed dates, so object ids agree; no user or system git config),
runs the operation, and then only real git is asked what came of it:
`ls-files --stage`, `ls-files -v` (flags such as skip-worktree),
`ls-files --resolve-undo`, `write-tree` (the tree a commit would record) and
`status --porcelain=v2`. The index's version and extensions are read by a
small reader of its own (`src/index_file.rs`), not by either library.

A third engine, **control**, is gitoxide made wrong on purpose: it keeps the
cache-tree it read. That it is caught (21 times) shows the comparison can see
a broken index.

## Results

26 scenarios: partial and whole-file staging of modified, new, executable,
deleted, intent-to-add and symlinked paths; other entries' skip-worktree and
assume-unchanged flags; index version 4; split index; untracked cache;
fsmonitor; sparse index; `core.autocrlf`; `eol=crlf`; a clean filter;
resolving a conflict; resetting changes and new files; a held `index.lock`;
a submodule entry; awkward path names; `index.skipHash`; a SHA-256
repository.

| | Same as git | Different | Notes |
| --- | --- | --- | --- |
| grit-lib | 26 | 0 | Keeps every extension git kept: split index (`link`), untracked cache, fsmonitor, resolve-undo, sparse index, version 4. Writes extensions in another order than git (harmless; git reads them in any order). |
| gitoxide | 25 | 1 | Loses the resolve-undo record when a conflict is resolved, which `git checkout -m` needs to bring the conflict back. Drops the split index, untracked cache and fsmonitor data and turns version 4 into 2: all things git rebuilds, but it changes the repository's setup behind the user's back. Its cache-tree has to be thrown away by hand, since it writes back the one it read. SHA-256 needs its `sha256` feature. |

Both refuse to write while another process holds `index.lock`, as git does.

Neither library applies patches to the index (grit-lib's `apply` only
parses them), so projj would stage by computing the new content itself, as
the `set` operation here does.

## Conclusion

grit-lib, for a built-in fallback. It matched git everywhere tested, keeps
what gitoxide drops, is MIT, pure Rust, and cheap to add (about 40 s of
release build and a few MB). Against it: it is new (April 2026, 0.x) and its
API is still being reorganised, so it should be pinned to an exact version
and upgraded deliberately, with this comparison run again.

What the spike does not cover yet, and a built-in git would need:

- **Reads.** projj also reads through git: `status`, `diff` (both sides,
  with numstat), `ls-files` and `cat-file`. Those are lower risk, since they
  change nothing, but they are most of the work.
- **Staleness.** `git apply --cached` refuses a patch whose context no
  longer matches the index, which is how projj notices that a hunk is out of
  date. Staging by content must instead check, under the index lock, that the
  path's staged blob is still the one the hunk was computed from.
- Behaviour under concurrent git processes, beyond a held lock.
