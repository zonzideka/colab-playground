# Releasing Colab Playground

How a version goes from commits on `main` to a GitHub release with downloads. Releases are driven by
[release-please](https://github.com/googleapis/release-please) in manifest mode; the changelog
follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) as far as release-please allows.
How users install a download is in [USAGE.md](USAGE.md#install-a-release).

The pieces:

- [`.github/workflows/release.yml`](../.github/workflows/release.yml): runs release-please on every
  push to `main`, then calls Package when a release was created.
- [`.github/workflows/package.yml`](../.github/workflows/package.yml): builds and checks the
  downloads; attaches them to a release, or keeps them as workflow artifacts (dry run).
- [`release-please-config.json`](../release-please-config.json): release type, version rules,
  changelog sections, the files that carry the version.
- [`.release-please-manifest.json`](../.release-please-manifest.json): the last released version.
  Only release-please writes it.

## How a release happens

1. Pull requests are squash-merged into `main`: each becomes one conventional commit, titled after
   the pull request.
2. On each push, the **Release** workflow runs release-please. If anything since the last release
   would appear in the changelog, it opens or updates one pull request, `chore(main): release
   X.Y.Z`, from the branch `release-please--branches--main`. The pull request changes:
   - `version` under `[workspace.package]` in `Cargo.toml` (both crates inherit it);
   - the workspace crates' entries in `Cargo.lock`, so CI's `--locked` builds pass;
   - `CHANGELOG.md` (the new entry goes on top; the file is created by the first release);
   - `.release-please-manifest.json`.

   Commits of hidden types alone (see the table below) open no pull request.
3. When the release should go out, merge that pull request with **Squash and merge**, like every
   other pull request. Nothing else needs editing.
4. That push runs Release again. release-please tags the squash commit `vX.Y.Z` and creates the
   GitHub release as a **draft**, with the changelog entry as its notes.
5. In the same run, **Package** builds the downloads on Windows, Linux and macOS, writes
   `SHA256SUMS`, attaches all four files to the draft and only then publishes it. A draft is hidden
   from visitors and is never the "latest" release, so the app's update check (which reads the
   latest release) never sees a release whose downloads are missing.

### The downloads

| File | For | Contents |
| --- | --- | --- |
| `colab-playground-X.Y.Z-x86_64-pc-windows-msvc.exe` | Windows x64 | The program itself |
| `colab-playground-X.Y.Z-x86_64-unknown-linux-gnu.tar.gz` | Linux x64 | A folder with `colab-playground`, `LICENSE`, `README.md` |
| `colab-playground-X.Y.Z-aarch64-apple-darwin.zip` | macOS, Apple silicon | A folder with `Colab Playground.app`, `LICENSE`, `README.md` |
| `SHA256SUMS` | All | The SHA-256 of the three files above |

There are no installers ([ROADMAP.md](ROADMAP.md#platform-and-distribution)). The binary needs no
other files: the bundled plugin and helper scripts are compiled in. Why each file looks the way it
does, and what Package checks before anything is attached:

- **Windows: the bare `.exe`.** It is built with the C runtime linked statically
  (`+crt-static`), so it starts without the Visual C++ Redistributable; the build fails if the exe
  still imports `VCRUNTIME140.dll`. It is not code-signed.
- **Linux: a `.tar.gz`.** HTTP carries no file permissions, so a binary downloaded on its own
  through a browser is saved without its executable bit; tar stores the mode, and the job fails if
  the binary in the archive is not `rwxr-xr-x`. It is built on the oldest Ubuntu runner GitHub
  offers (`ubuntu-22.04`) so it runs on as many distributions as possible; the resulting requirement
  is stated in [USAGE.md](USAGE.md#install-a-release). When that runner is retired, move to the
  oldest one left and update the requirement there.
- **macOS: a `.zip` holding the `.app`.** An `.app` is a folder and a release asset must be a single
  file. The workflow assembles the bundle: an `Info.plist` it writes (identifier
  `io.github.zonzideka.colab-playground`, version from Cargo, copyright line from `LICENSE`), the
  binary, and [`assets/icon/icon.icns`](../assets/icon/icon.icns). It is ad-hoc signed (Apple
  silicon runs only signed code) but has no Developer ID and is not notarised, so the first launch
  needs the steps in USAGE.md. The zip is made with `ditto`, and the job unzips it again and
  verifies the signature.
- Each build job reads the version from Cargo metadata for the file names. On a release it also
  checks that the tag is `v` plus that version, so a release whose `Cargo.toml` was not bumped is
  never built.
- Release builds use `[profile.release]` in [`Cargo.toml`](../Cargo.toml) and are compiled from
  scratch, without a build cache.

## Version numbers

- Tags are `vX.Y.Z`; the version lives only in `[workspace.package]` and the crates inherit it
  (`version.workspace = true`).
- Before 1.0.0: a breaking change or a `feat` raises the minor version (0.1.3 → 0.2.0); any other
  commit that shows in the changelog raises the patch version. From 1.0.0 a breaking change raises
  the major version.
- To choose a version (for example 1.0.0), end the squash commit of a `feat` or `fix` pull request
  with the footer `Release-As: 1.0.0` (see [footers](#writing-commits-for-the-changelog)). A
  commit of a hidden type that carries the footer is listed in the changelog anyway, under its
  hidden section's name (for example "Chores").
- The release type is `simple`, with the version written into `Cargo.toml` and `Cargo.lock` as
  `extra-files`. release-please's `rust` type and its `cargo-workspace` plugin cannot handle a
  version inherited from `[workspace.package]` (the `rust` updater only edits `[package].version`,
  and the plugin rejects `version.workspace = true`), so do not switch to them. The `Cargo.lock`
  path matches every package without a `source`, which is every workspace member: a new crate under
  `crates/` that inherits the version is covered without touching the config.

## Writing commits for the changelog

Each commit on `main` is a squash-merged pull request, and its subject is the pull request's title:
**write the title as a conventional commit**, because it becomes one changelog line. The rest of
the message does not appear in the changelog, with two exceptions:

- the text of a `BREAKING CHANGE:` footer (see **Breaking change** below);
- a paragraph that starts with a standard type after a blank line (`feat: …`, `fix(app): …`):
  release-please reads it as one more commit, with its own changelog line and its effect on the
  version. The commits GitHub lists in the proposed message start with `* `, so they do not count.

The scope (`app`, `core`) is printed in bold before the line, and each line links to its commit.

**Footers** (`BREAKING CHANGE:`, `Release-As:`) belong at the end of the squash commit's message.
When merging, check the message GitHub proposes: put any footer on its last lines, and make sure
no paragraph starts with a type unless you want that extra line.

| Type | Section | Use it for |
| --- | --- | --- |
| `feat` | Added | A new capability |
| `change` | Changed | Existing behaviour that now works differently |
| `perf` | Changed | The same behaviour, faster or lighter |
| `deprecate` | Deprecated | Something that still works but will be removed |
| `remove` | Removed | Something that is gone |
| `fix` | Fixed | A bug fix |
| `security` | Security | A fix for a vulnerability |
| `docs`, `refactor`, `test`, `build`, `ci`, `style`, `chore`, `revert` | (not shown) | Anything users do not notice |

- Write the subject as the user-visible effect, in the imperative ("keep the title bar usable while
  a dialog is open"), not the implementation.
- **Breaking change:** add `!` after the type or scope (`remove(app)!: drop the old settings file`)
  and, ideally, a footer `BREAKING CHANGE: <what users must do>`. The footer text (or the subject,
  without a footer) is listed under **⚠ BREAKING CHANGES** at the top of the entry, and the commit
  also appears in its own section.
- **Reverts:** undoing a change that was already released is described by its effect (`remove:` for
  a feature taken back, `fix:` for a regression). `revert:` is hidden, for undoing unreleased work.
- A wrong line cannot be fixed by rewriting the commit. Fix it in `CHANGELOG.md` with a `docs:`
  commit after the release (release-please only adds new entries above the old ones and never
  rewrites them), and edit the release notes on GitHub if needed.

### What matches Keep a Changelog and what does not

Matches: one `CHANGELOG.md`, newest version first; one entry per version with its date (ISO 8601);
the sections `Added`, `Changed`, `Deprecated`, `Removed`, `Fixed`, `Security`, in that order and
only when they have lines; Semantic Versioning; no commit noise.

Does not match (release-please's fixed format):

- **No `[Unreleased]` section.** The next entry is shown in the open release pull request instead.
  Do not add one by hand: release-please inserts each new entry above the first version heading,
  which an `## [Unreleased]` heading would be.
- **Version headings** read `## [0.2.0](compare link) (2026-01-31)` instead of
  `## [0.2.0] - 2026-01-31` with link references at the bottom; the first release has no compare
  link: `## 0.1.0 (date)`.
- **Breaking changes** get an extra `⚠ BREAKING CHANGES` section, which Keep a Changelog does not
  have.
- **Lines** carry the bold scope and a commit link; there is no `[YANKED]` marker.
- **The file starts with `# Changelog`** and no introduction. An introduction can be added once,
  between that title and the first version heading; release-please keeps everything above the
  first version heading.

## The first release

- `.release-please-manifest.json` starts as `{}` (nothing released) and the config sets
  `initial-version` to `0.1.0`. With no earlier release, release-please uses that version instead
  of computing a bump, so the first release is 0.1.0 whatever the commit types.
- `bootstrap-sha` is `61c92b2`, the commit just before `feat: initial release`. release-please reads
  only the commits after it for the first entry (the commit itself is excluded), and that one
  commit exists to give the entry its line. So the 0.1.0 entry reads "initial release" instead of
  listing every commit since the project began; the README describes what the app does, and the
  full history stays in `git log`. The `bootstrap-sha` commit must stay on `main` unrewritten.
- Once a release exists, release-please counts from it and ignores `bootstrap-sha` and
  `initial-version`; both can stay in the config.
- The first release is otherwise a normal one: do the one-time settings below, push, wait for the
  release pull request, get CI to run on it (see below), merge it, and watch the Release run publish
  v0.1.0 with its four files.

## One-time repository settings

1. **Let Actions open pull requests.** Settings → Actions → General → Workflow permissions: tick
   **Allow GitHub Actions to create and approve pull requests** and click **Save**. Without it
   release-please cannot open the release pull request. Leave the default token permission at
   "Read repository contents and packages permissions": each job requests the permissions it needs.
   The same with the CLI:

   ```sh
   gh api -X PUT repos/zonzideka/colab-playground/actions/permissions/workflow \
     -f default_workflow_permissions=read -F can_approve_pull_request_reviews=true
   ```

2. **Protect `main` with a ruleset.** Settings → Rules → Rulesets. The ruleset `main` applies to the
   default branch:
   - changes arrive by pull request (no approval needed);
   - the Linux and Windows CI jobs must pass, on a branch that is up to date with `main`;
   - force pushes and deleting the branch are blocked;
   - repository admins can bypass all of it.

   Only those two CI jobs are required. The macOS CI job often waits for a runner that is never
   assigned, and Release and Package do not run on every pull request, so requiring any of them
   would block every merge. The macOS job still runs when a runner is assigned. The required
   names are the job names in [`ci.yml`](../.github/workflows/ci.yml): change one and the ruleset
   must follow.

3. **Merge by squashing only.** Settings → General → Pull Requests: tick only **Allow squash
   merging**, with the default commit message set to "Default to pull request title and commit
   details", and tick **Automatically delete head branches**. Each pull request then lands as one
   commit titled after it, so fix-up commits (review fixes and the like) never reach the changelog;
   this is also the merge method release-please recommends. The ruleset still lists all three merge
   methods; the repository setting is the one that limits them. The same with the CLI:

   ```sh
   gh api -X PATCH repos/zonzideka/colab-playground -F allow_squash_merge=true \
     -F allow_rebase_merge=false -F allow_merge_commit=false -F delete_branch_on_merge=true \
     -f squash_merge_commit_title=PR_TITLE -f squash_merge_commit_message=COMMIT_MESSAGES
   ```

4. Optionally, the token described in the next section.

Only `GITHUB_TOKEN` is used otherwise. Permissions per job: release-please gets `contents`,
`issues` (labels) and `pull-requests` write; the build and checksum jobs get `contents: read`; only
the job that attaches files and publishes the release gets `contents: write`, and it runs only for
a release. If immutable releases are turned on for the repository, the flow still works, because
the files are attached while the release is a draft; only replacing the files of an already
published release then becomes impossible.

## CI on the release pull request

The release pull request is opened and updated with `GITHUB_TOKEN`, as `github-actions[bot]`. For
the pull request events that token causes, GitHub creates the CI and Package runs but holds them
until someone with write access approves them: they show as **Action required**. The repository's
setting for approving workflows from outside contributors does not change this. `main` requires
the CI jobs, so the release pull request cannot be merged until they have run on it:

- **Simplest:** just before merging, open one of the waiting runs (from the pull request's checks or
  the Actions tab) and click **Approve and run**. CI and the Package dry run (the pull request
  changes `.release-please-manifest.json`) then run on the exact release commit. Each later update
  of the pull request by release-please waits for approval again. Closing and reopening the pull
  request works too: the reopen is your event, so its runs start without approval.
- **Every time, automatically:** create a fine-grained personal access token limited to this
  repository with **Contents**, **Pull requests** and **Issues** set to read and write, and save it
  as the Actions secret `RELEASE_PLEASE_TOKEN`. release-please then acts as you, so every update of
  the release pull request (each time its changelog grows) runs CI and the Package dry run without
  approval, three release builds each time. The job's `permissions` do not limit this token. When
  it expires the Release job fails: renew it, or delete the secret and the workflow uses
  `GITHUB_TOKEN` again.
- **Without the checks:** a repository admin can tick **Merge without waiting for requirements to be
  met** on the pull request. The release commit then reaches `main` unchecked; CI still runs on
  `main` after the merge, at the same time as the release is built.

## Dry-running the packaging

- **On pull requests** that change `release.yml`, `package.yml`, `release-please-config.json`,
  `.release-please-manifest.json` or anything under `assets/icon/`, Package runs as a dry run.
- **By hand:** Actions → Package → Run workflow, choose the branch, leave **tag** empty. Do this
  after changing `[profile.release]`, which the path filter does not cover.

A dry run runs the same build, archive and check steps as a release; only the last job (attach and
publish) is skipped. Its artifacts are the four files exactly as a release would get them, named
with the version in `Cargo.toml`; download them from the run's summary page.

## When a build fails after the release exists

The tag exists, `main` already has the new version and changelog entry, and the release is still a
draft with some or none of its files, invisible to users and to the update check.

- **A transient failure** (runner, network): open the failed Release run and use **Re-run failed
  jobs**. Do not use **Re-run all jobs**: release-please would run again, find nothing to release
  and skip the build.
- **A problem in the workflow or the packaging:** fix it on `main` (as `ci:` or `build:`, which the
  changelog does not show), then Actions → Package → Run workflow on `main` with **tag** set to
  `vX.Y.Z`. It builds the tagged code with the fixed workflow, replaces any files already attached
  and publishes the draft.
- **A problem in the code at the tag:** land the fix as `fix:` and merge the next release pull
  request (vX.Y.Z+1). Delete the broken draft on GitHub but keep its tag: release-please finds the
  previous release through it. That version number is then skipped.

Never move or delete a release tag.
