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

> **Invariant:** the commit a release tag points at must have its `Cargo.toml`
> `version` equal to the tag without the `v` (e.g. tag `v0.2.0` → `version = "0.2.0"`).
> The version bump and the tag therefore live on the **same commit** — never bump
> `master` to a version ahead of the latest release "in advance". Between releases,
> `master` carries the last released version. (`Cargo.toml` drives the binary's own
> `--version`; the workflow only stamps the `.app` bundle, so the source must be right
> at the tagged commit.)

1. **Confirm `master` is releasable** — all intended PRs merged and the tree coherent.
2. **Create a release-prep commit** (a tiny PR, or the last commit before tagging) that
   sets the version to `X.Y.Z` in **all three** places and updates the changelog:
   - [`Cargo.toml`](Cargo.toml) → `version = "X.Y.Z"`.
   - [`Cargo.lock`](Cargo.lock) → the `[[package]] name = "allele"` entry's
     `version = "X.Y.Z"` (edit it directly, or run `cargo build` once to let cargo sync
     the lockfile). Easy to forget — the tagged commit is inconsistent without it.
   - [`resources/Info.plist`](resources/Info.plist) → `CFBundleShortVersionString` = `X.Y.Z`.
   - [`CHANGELOG.md`](CHANGELOG.md) → move the `## [Unreleased]` items into a new
     `## [X.Y.Z] - YYYY-MM-DD` section and refresh the compare/tag links at the bottom.
3. **Merge the prep commit to `master`.**
4. **Tag that commit and push** — this is the trigger:
   ```sh
   git checkout master && git pull
   git tag vX.Y.Z
   git push origin vX.Y.Z
   ```
5. **Everything after the tag push is automatic — do not do it by hand.**
   [`release.yml`](.github/workflows/release.yml) builds the `.app`, ad-hoc signs it,
   zips it as `Allele-vX.Y.Z-macos.zip`, creates the GitHub **pre-release** with
   auto-generated notes, and prepends the first-launch instructions from
   [`.github/RELEASE_INSTALL.md`](.github/RELEASE_INSTALL.md).
6. **Verify:** the Actions → Release run is green, the release exists, and the zip is
   attached. Optionally paste curated `CHANGELOG.md` highlights above the auto PR list.

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

Release bundles are **ad-hoc signed** in the workflow (`codesign --force --deep --sign -`)
so macOS Gatekeeper accepts the signature and no longer reports the app as "damaged".
They are **not notarised**, so first launch still shows a one-time Gatekeeper prompt —
the workflow prepends bypass instructions ([`.github/RELEASE_INSTALL.md`](.github/RELEASE_INSTALL.md))
to every release's notes.

Full notarization (double-click, no prompt) is tracked in **DEV-94** for the beta
milestone: swap the ad-hoc `-` identity for a Developer ID certificate (with
`--options runtime`), add `xcrun notarytool submit --wait` + `xcrun stapler staple`, and
supply the signing cert `.p12` + an App Store Connect API key as repo secrets. This is
additive and needs no restructuring of `release.yml`.

## Windows / Linux

Out of scope. The APFS copy-on-write workspace model is Apple-specific and central to
the tool, so releases are macOS-only for the foreseeable future.
