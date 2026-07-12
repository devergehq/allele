# Releasing Allele

This document describes how Allele is versioned and how a release is cut. It is
the process reference for maintainers; end users just download from the
[Releases page](https://github.com/devergehq/allele/releases).

## Versioning

Allele follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html) on a
pre-1.0 `0.MINOR.PATCH` line while the project is in alpha:

- **`MINOR`** (e.g. `0.1.0 → 0.2.0`) — a batch of features; may include breaking
  changes to state files, config, or behaviour. This is the normal release bump today.
- **`PATCH`** (e.g. `0.2.0 → 0.2.1`) — bug-fix-only releases off a `MINOR` line.
- Tags are prefixed with `v` (e.g. `v0.2.0`).

`0.x` releases are published as **pre-releases** on GitHub (the release workflow marks
them so automatically). We move to `1.0.0` only when the project is considered
generally usable and the public API/behaviour is stable enough to promise compatibility.

## Cadence

Alpha cadence is **batch-driven, not calendar-driven**: cut a release when a
meaningful group of PRs has landed on `master` and the build is in a coherent,
runnable state. There is no fixed schedule. Prefer more frequent, smaller releases
over rare, giant ones so the auto-generated notes stay readable.

## Cutting a release (normal path)

1. **Make sure `master` is green and coherent.** All intended PRs merged.
2. **Prepare the version.** In a short release PR (or as the final change in the last
   feature PR):
   - Bump `version` in [`Cargo.toml`](Cargo.toml).
   - Bump `CFBundleShortVersionString` in [`resources/Info.plist`](resources/Info.plist)
     to match. (The workflow also stamps this at build time, but keeping the source in
     sync means local `./script/bundle-mac.sh` builds report the right version too.)
   - Move the `## [Unreleased]` items in [`CHANGELOG.md`](CHANGELOG.md) into a new
     `## [X.Y.Z] - YYYY-MM-DD` section and refresh the compare/tag links at the bottom.
3. **Merge to `master`.**
4. **Tag and push** — this is what triggers the release build:
   ```sh
   git checkout master && git pull
   git tag vX.Y.Z
   git push origin vX.Y.Z
   ```
5. The [`release.yml`](.github/workflows/release.yml) workflow builds the macOS `.app`,
   zips it as `Allele-vX.Y.Z-macos.zip`, and creates a GitHub Release for the tag with
   **auto-generated notes** (every PR since the previous release) plus the binary asset.
6. **Optionally polish the notes.** Open the release on GitHub and paste the curated
   highlights from `CHANGELOG.md` above the auto-generated PR list.

## Cutting a release from an arbitrary commit (manual path)

Because the release workflow reads its own definition from the ref it runs on, a tag
placed on a commit *older than the workflow* won't trigger it. To build any commit —
including historical ones — use the manual dispatch, which runs the workflow from
`master` but checks out the commit you name:

1. GitHub → **Actions → Release → Run workflow**.
2. Inputs:
   - **tag** — the tag to create, e.g. `v0.1.0`.
   - **ref** — the commit SHA / branch / tag to build, e.g. `8f7e2be`. Leave blank to
     build the tag itself.
   - **prerelease** — leave `true` for `0.x`.
3. The workflow builds that ref, creates the tag pointing at it, and publishes the release.

Equivalent CLI:
```sh
gh workflow run release.yml -f tag=v0.1.0 -f ref=8f7e2be -f prerelease=true
```

## Signing & notarization

Binaries are currently shipped **unsigned and un-notarised** (pre-alpha). Users clear
the Gatekeeper quarantine with `xattr -dr com.apple.quarantine Allele.app` or a
right-click → Open. When the project reaches beta, add Developer ID signing +
notarization by supplying repo secrets (signing cert `.p12` and an App Store Connect
API key) and enabling the signing step in `release.yml`; the workflow is structured so
this is additive and requires no restructuring.

## Windows / Linux

Out of scope. The APFS copy-on-write workspace model is Apple-specific and central to
the tool, so releases are macOS-only for the foreseeable future.
