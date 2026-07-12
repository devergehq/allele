# Allele — agent & contributor guide

Allele is a **native macOS session manager for Claude Code** with APFS copy-on-write
workspace isolation. Rust + [GPUI](https://github.com/zed-industries/zed) (renders via
Metal) + `alacritty_terminal`. Targets **macOS 14+ on Apple silicon**.

This file orients any agent or new contributor. Deeper detail lives in the docs below —
prefer linking to them over duplicating.

## Key docs
- **Architecture:** [ARCHITECTURE.md](ARCHITECTURE.md) — how the app is structured.
- **Contributing & conventions:** [CONTRIBUTING.md](CONTRIBUTING.md) — expectations, CLA, code style.
- **Cutting a release:** [RELEASING.md](RELEASING.md) — the authoritative release runbook.
- **Changelog:** [CHANGELOG.md](CHANGELOG.md).

## Common commands
```sh
cargo build --release              # build (first build ~5-10 min: GPUI + alacritty_terminal)
cargo test                         # run tests
cargo fmt && cargo clippy          # format + lint
./script/bundle-mac.sh --release   # build + assemble the ad-hoc-signed Allele.app bundle
./target/release/Allele            # run the built binary
```

## Releasing

**To cut a release, follow [RELEASING.md](RELEASING.md).** In short: make a release-prep
commit that bumps the version in **`Cargo.toml`, `Cargo.lock`, and `resources/Info.plist`**
(all to `X.Y.Z`) and moves `CHANGELOG.md`'s `[Unreleased]` into a dated `[X.Y.Z]` section;
merge it to `master`; then tag and push:

```sh
git tag vX.Y.Z && git push origin vX.Y.Z
```

Pushing a `v*` tag triggers [`.github/workflows/release.yml`](.github/workflows/release.yml),
which builds, **ad-hoc signs**, zips (`Allele-vX.Y.Z-macos.zip`), and publishes the GitHub
pre-release with auto-generated notes plus the first-launch instructions. **Do not perform
the build/sign/zip/publish steps by hand** — the tag push is the only manual trigger. To
release a commit older than the workflow, use the manual `workflow_dispatch` (`ref` input)
path documented in RELEASING.md.

Versioning is SemVer on a pre-1.0 `0.MINOR.PATCH` alpha line. **Invariant:** the tagged
commit's `Cargo.toml` version must equal the tag minus its `v` — bump and tag on the same
commit; never bump `master` ahead of the latest release.

## Conventions
Rust 2021, standard `rustfmt` + `clippy`, one logical change per PR, PRs target `master`.
See [CONTRIBUTING.md](CONTRIBUTING.md) for the full guide (including the CLA).
