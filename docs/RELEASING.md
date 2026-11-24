# Releasing Colab Playground

How a version goes from commits on `main` to a GitHub release with downloads. This fork releases by
hand — the changelog follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the
version, the entry and the tag are made by the steps below. There is no release bot and no CI on
`main`: both were upstream's and were removed.
How users install a download is in [USAGE.md](USAGE.md#install-a-release).

The pieces:

- [`.github/workflows/package.yml`](../.github/workflows/package.yml): the one workflow. A `vX.Y.Z`
  tag — or a manual run naming one — builds the downloads, writes and signs `SHA256SUMS` and
  attaches everything to that tag's release; a manual run with **dry run** ticked does all of the
  build and signing without touching a release.
- The three files a release changes, in one commit: `version` under `[workspace.package]` in
  [`Cargo.toml`](../Cargo.toml) (both crates inherit it), the workspace crates' entries in
  `Cargo.lock`, and `CHANGELOG.md`.

## How a release happens

1. Pick the version from the commits since the last tag ([Version numbers](#version-numbers)), and
   land the release as one `chore: release X.Y.Z` commit on `main`: the version in `Cargo.toml`, the
   same version for both workspace crates in `Cargo.lock` (`cargo metadata --locked` fails while the
   lock file is stale), and the new `CHANGELOG.md` entry on top.
2. Push that commit, then push the tag `vX.Y.Z` on it.
3. The tag starts **Package**: Windows and Linux build, `SHA256SUMS` is written and signed with the
   `UPDATE_SIGNING_KEY` secret, the signature is checked against `crates/core/update-keys.txt`, the
   GitHub release is created as a **draft**, every file is attached to it, and only then is it
   published. A draft is hidden from visitors and is never the "latest" release, so the app's update
   check (which reads the latest release) never sees a release whose downloads are missing.
4. To try a packaging change without publishing anything: Actions → Package → Run workflow on
   `main`, with **tag** `vX.Y.Z` (a version `Cargo.toml` already carries) and **dry run** ticked. It
   runs the whole build and the signing, and skips only the job that attaches and publishes.

A release is always signed: an unsigned one could not be installed by the app, so a run without the
`UPDATE_SIGNING_KEY` secret fails instead of publishing.

### The downloads

| File | For | Contents |
| --- | --- | --- |
| `colab-playground-x86_64-pc-windows-msvc.exe` | Windows x64 | The program itself |
| `colab-playground-x86_64-unknown-linux-gnu.tar.gz` | Linux x64 | A folder `colab-playground-x86_64-unknown-linux-gnu` with `colab-playground`, `LICENSE`, `README.md` |
| `colab-playground-aarch64-apple-darwin.zip` | macOS, Apple silicon | A folder `colab-playground-aarch64-apple-darwin` with `Colab Playground.app`, `LICENSE`, `README.md` |
| `SHA256SUMS` | All | The SHA-256 of the three files above |
| `SHA256SUMS.sig` | All | The signature of `SHA256SUMS` ([The update signature](#the-update-signature)) |

There are no installers ([ROADMAP.md](ROADMAP.md#platform-and-distribution)). The binary needs no
other files: the bundled plugin and helper scripts are compiled in. Why each file looks the way it
does, and what Package checks before anything is attached:

- **No version in the names.** The release tag says which version the files are. The Windows exe
  is the app itself and keeps its name when the app updates in place, and the folder the Linux
  archive unpacks stays where the user put it. Installed copies fetch the files by these names
  from `releases/download/<tag>/`: renaming a download, or the folder inside an archive, breaks
  updating for every installed copy.

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
  `io.github.wynxing.colab-playground`, version from Cargo, copyright line from `LICENSE`), the
  binary, and [`assets/icon/icon.icns`](../assets/icon/icon.icns). It is ad-hoc signed (Apple
  silicon runs only signed code) but has no Developer ID and is not notarised, so the first launch
  needs the steps in USAGE.md. The zip is made with `ditto`, and the job unzips it again and
  verifies the signature.
- Each build job reads the version from Cargo metadata, for the macOS bundle. On a release it
  also checks that the tag is `v` plus that version, so a release whose `Cargo.toml` was not
  bumped is never built.
- The build sets `COLAB_PLAYGROUND_RELEASE_BUILD`, which marks the binary as a release download:
  only such a binary replaces itself with a newer release. Any other build (`cargo run`, a local
  release build, a distribution's package) offers the download page instead.
- Release builds use `[profile.release]` in [`Cargo.toml`](../Cargo.toml) and are compiled from
  scratch, without a build cache.

### The update signature

The app installs an update only after checking that it is what the project published. Package
signs, with the Ed25519 private key in the Actions secret `UPDATE_SIGNING_KEY`, the line
`colab-playground <tag>` followed by the bytes of `SHA256SUMS`, and attaches the 64-byte signature
as `SHA256SUMS.sig`. The app rebuilds that message from the tag it downloads from, accepts the
signature only from one of the public keys in
[`crates/core/update-keys.txt`](../crates/core/update-keys.txt), which are compiled into it, and
then compares the download with the SHA-256 listed for it. The tag is part of the message because
every release lists the same file names: without it an older release's signed files would pass for
a newer one.

- Package checks the new signature against `update-keys.txt` at the tag before anything is
  attached, so a release signed with a key the app does not trust is never published.
- A release fails when the secret is missing, so no unsigned release is published. A dry run never
  signs and says so in a notice; the key is used only for a release.
- **What it protects against:** release files that were changed or replaced after the build, on
  GitHub or on the way, and an older release's files passed off as a newer release.
- **What it does not protect against:** someone who takes over the repository and its secrets, or
  who can change the workflows on `main`: Package signs whatever it builds. Nor is it OS code
  signing: Windows SmartScreen and macOS Gatekeeper still treat a download by hand as unsigned.
- **Rotating the key:** add the new public key to `update-keys.txt` and release; once that release
  is out, replace the secret with the new private key, and remove the old public key in a later
  release. A copy older than the release that added the new key can no longer verify updates: its
  users update by hand once.
- **Losing the key** (the secret is deleted, or the private key leaks): create a new key pair as in
  [One-time repository settings](#one-time-repository-settings), replace the public key in
  `update-keys.txt` (remove a leaked one) and release. Installed copies trust only the old key, so
  they report that release as not verified and do not install it: their users download it by hand
  once ([USAGE.md](USAGE.md#install-a-release)), and from then on updates install in place again.

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
- `Cargo.lock` lists the version of every package without a `source`, which is every workspace
  member: a new crate under `crates/` that inherits the version is one more entry to bump.

## Writing commits for the changelog

**Write every commit subject as a conventional commit**: the subject becomes one changelog line, in
the section its type picks (table below), and a hidden type keeps its commit out of the entry. The
changelog is written by hand in the shape upstream's bot used: the scope (`app`, `core`) in bold
before the line, and the line linking to its commit. The rest of the message does not appear.

A **breaking change** carries `!` after the type or scope and, ideally, a `BREAKING CHANGE:` footer
as the commit's last lines; the footer text is also listed under **⚠ BREAKING CHANGES** at the top
of the entry.

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
  commit after the release (the changelog only ever gains entries above the old ones; it never
  rewrites them), and edit the release notes on GitHub if needed.

### What matches Keep a Changelog and what does not

Matches: one `CHANGELOG.md`, newest version first; one entry per version with its date (ISO 8601);
the sections `Added`, `Changed`, `Deprecated`, `Removed`, `Fixed`, `Security`, in that order and
only when they have lines; Semantic Versioning; no commit noise.

Does not match the shape the bot used:

- **No `[Unreleased]` section.** An entry is written when its release is made, so there is nothing
  to park: it goes above the first version heading.
- **Version headings** read `## [0.2.0](compare link) (2026-01-31)` instead of
  `## [0.2.0] - 2026-01-31` with link references at the bottom; the first release has no compare
  link: `## 0.1.0 (date)`.
- **Breaking changes** get an extra `⚠ BREAKING CHANGES` section, which Keep a Changelog does not
  have.
- **Lines** carry the bold scope and a commit link; there is no `[YANKED]` marker.
- **The file starts with `# Changelog`** and no introduction. An introduction can sit between that
  title and the first version heading.

## The first release

Versions 0.1.0 to 0.5.0 were made by upstream's release bot, which is no longer in this repository:
its manifest, config and workflows are gone, so the version is now chosen by hand as described
above. The one thing it left behind that still matters is the tag history — a version number is
never reused.

## One-time repository settings

1. **Token permissions.** Settings → Actions → General → Workflow permissions: leave the default
   token permission at "Read repository contents and packages permissions": each job requests the
   permissions it needs. Nothing else is needed here — this fork has no bot that opens pull
   requests. The same with the CLI:

   ```sh
   gh api -X PUT repos/wynxing/colab-playground/actions/permissions/workflow \
     -f default_workflow_permissions=read
   ```

2. **Protect `main` with a ruleset.** Settings → Rules → Rulesets. The ruleset `main` applies to the
   default branch:
   - changes arrive by pull request (no approval needed);
   - force pushes and deleting the branch are blocked;
   - repository admins can bypass all of it.

   No status check is required: this fork has no CI workflows, so there is none to require. A
   required check that never runs would block every merge, and a release commit is pushed straight
   to `main` anyway.

3. **Merge by squashing only.** Settings → General → Pull Requests: tick only **Allow squash
   merging**, with the default commit message set to "Default to pull request title and commit
   details", and tick **Automatically delete head branches**. Each pull request then lands as one
   commit titled after it, so fix-up commits (review fixes and the like) never reach the changelog;
   and it keeps fix-up commits out of the changelog. The ruleset still lists all three merge
   methods; the repository setting is the one that limits them. The same with the CLI:

   ```sh
   gh api -X PATCH repos/wynxing/colab-playground -F allow_squash_merge=true \
     -F allow_rebase_merge=false -F allow_merge_commit=false -F delete_branch_on_merge=true \
     -f squash_merge_commit_title=PR_TITLE -f squash_merge_commit_message=COMMIT_MESSAGES
   ```

4. **Create the update signing key** ([The update signature](#the-update-signature)). On your own
   computer, with OpenSSL 3 (on Windows, Git Bash has it), make a key pair, save the private key
   as the Actions secret `UPDATE_SIGNING_KEY`, print the public key and delete the file, so the
   private key lives only in the secret:

   ```sh
   (umask 077 && openssl genpkey -algorithm ed25519 -out update-signing-key.pem)
   gh secret set UPDATE_SIGNING_KEY --repo wynxing/colab-playground < update-signing-key.pem
   openssl pkey -in update-signing-key.pem -pubout -outform DER | tail -c 32 | base64
   rm update-signing-key.pem
   ```

   The third command prints the public key, 44 characters ending in `=`. It is not secret: add it
   as a line of [`crates/core/update-keys.txt`](../crates/core/update-keys.txt), so the app trusts
   it. A release fails until the secret is set. (This repository already has its own pair: the
   public key is the one in that file, and the secret is set.)

`UPDATE_SIGNING_KEY` and `GITHUB_TOKEN` are the only secrets used. Permissions per job: the build
and checksum jobs get `contents: read`; only the job that attaches the files and publishes the
release gets `contents: write`, and it runs only for a release. If immutable releases are turned on for the repository, the flow still works, because
the files are attached while the release is a draft; only replacing the files of an already
published release then becomes impossible.

## Checks before a release

This fork has no CI workflows, and nothing gates a release commit automatically. Run the checks
yourself before tagging ([TESTING.md](TESTING.md)): `cargo fmt --check`, `cargo clippy --workspace
--all-targets --locked -- -D warnings` and `cargo test --workspace --locked`. The tag's Package run
builds on GitHub's runners and fails the release if a download is missing, so an incomplete build
is caught there.

## Dry-running the packaging

Actions → Package → **Run workflow** on `main` with **tag** `vX.Y.Z` (a version `Cargo.toml`
already carries) and **dry run** ticked. It builds, writes and signs `SHA256SUMS`, checks the
signature against `update-keys.txt`, and skips only the job that attaches the files and publishes.

Use it after any change to `package.yml`, `[profile.release]` or the release steps, and before a
first release with a new signing key: it is the only way to run those steps without publishing. The
artifacts of a dry run are the downloads and `SHA256SUMS`, from the run's summary page.

## When a build fails after the release exists

The tag exists and the release is still a draft with some or none of its files — invisible to users
and to the update check.

- **A transient failure** (runner, network): open the failed run and use **Re-run failed jobs**.
- **Signing failed** (the secret is not set, or is not a key in `update-keys.txt` at the tag): set
  the right secret ([One-time repository settings](#one-time-repository-settings)) and re-run. A key
  missing from `update-keys.txt` at the tag is a problem in the code there.
- **A problem in the workflow or the packaging:** fix it on `main` (as `ci:` or `build:`, which the
  changelog does not show), then Actions → Package → Run workflow on `main` with **tag** `vX.Y.Z`.
  It builds the tagged code with the workflow from `main`, replaces any files already attached and
  publishes the draft.
- **A problem in the code at the tag:** land the fix as `fix:` and release the next version. Delete
  the broken draft on GitHub but keep its tag: the version number is spent.

Never move or delete a release tag.
