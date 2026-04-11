# Contributing to cc-multiplex

Thanks for your interest. cc-multiplex is a small personal project primarily built for the maintainer and a handful of friends who run lots of Claude Code sessions. Contributions are welcome, but please read this document first so we're on the same page about expectations.

## Expectations

- **This is a side project.** Response times on issues and PRs may be slow — days to weeks, not hours. If that's not acceptable for your situation, please fork.
- **Scope is intentionally narrow.** The project aims to do one thing well: manage multiple Claude Code sessions on macOS with APFS-backed workspace isolation. Feature requests outside that scope will usually be declined.
- **macOS only.** Cross-platform support is explicitly out of scope. The APFS copy-on-write feature is a core value proposition and is Apple-specific.

## Before you start work

- **For anything more than a trivial fix**, please open an issue first to discuss the approach. This avoids wasted effort if the change conflicts with the direction of the project.
- **For trivial fixes** (typos, obvious bugs, small doc improvements), feel free to open a PR directly.

## Contributor Licence Agreement

Before your first contribution can be merged, you'll need to sign the project's Contributor Licence Agreement ([CLA.md](CLA.md)). The agreement is lightweight:

- You retain copyright to your contributions.
- You grant the maintainer a broad licence to use, distribute, and sublicence your contributions — including under different licence terms in the future.
- You confirm that you wrote the code (or have permission to contribute it) and that you're legally entitled to grant the licence.

When you open your first pull request, **CLA Assistant** will automatically post a comment asking you to sign. It's a single click and records your agreement against your GitHub identity. You only sign once.

If you object to signing a CLA on principle, that's fine — you're welcome to fork the project under the Apache 2.0 licence and maintain your own version.

## Building from source

Requirements:

- macOS 14 or later (Metal is required by GPUI)
- Rust toolchain (stable) — install via https://rustup.rs
- Xcode Command Line Tools (`xcode-select --install`)

Clone and build:

```sh
git clone https://github.com/patrickdorival/cc-multiplex.git
cd cc-multiplex
cargo build --release
./target/release/cc-multiplex
```

First build is slow because GPUI and alacritty_terminal are large. Incremental builds are fast.

## Code conventions

- **Rust 2021 edition.** Standard `rustfmt` and `clippy` — no custom rules yet.
- **One logical change per PR.** If you find yourself touching unrelated files, split the PR.
- **Clear commit messages.** Explain *why*, not just *what*. The commit message is the primary documentation for future-you and future-anyone-else.
- **No aggressive refactors in feature PRs.** If you want to refactor, do it in a separate PR with its own discussion.

## Reporting bugs

Open a GitHub issue with:

1. macOS version and architecture (`sw_vers` and `uname -m`).
2. Claude Code version (`claude --version`).
3. Steps to reproduce.
4. What you expected to happen.
5. What actually happened.
6. Any relevant terminal output or crash log.

## Licence

By contributing, you agree that your contributions will be licensed under the Apache License 2.0 (see [LICENSE](LICENSE)), with the additional sublicensing rights granted by the [CLA](CLA.md).
