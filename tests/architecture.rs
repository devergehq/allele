//! Architecture fitness tests — executable enforcement of `ARCHITECTURE.md` §7.
//!
//! Prose rules rot. Every invariant in §7 was documented and several were still
//! being violated in live code by the time the 2026-07 architecture review ran
//! (see the "Architecture Review — 2026-07" Linear project). These tests turn
//! the documented rules into deterministic CI status checks so drift fails at
//! PR time instead of accumulating until the next manual audit.
//!
//! # The ratchet pattern
//!
//! Several rules are violated *today* by code that predates enforcement. Rather
//! than block the repo on a big-bang cleanup, those tests assert against a
//! **baseline** — the current violation count — and fail if it grows. When you
//! reduce a count, lower the baseline in the same PR; the test then locks in
//! your improvement. Baselines only ever go down.
//!
//! A baseline of `0` means the rule is fully enforced and must stay that way.
//!
//! # Why file-walking instead of a lint plugin
//!
//! These run under plain `cargo test` with no toolchain plugins, so they work
//! identically on a laptop and in CI. Structural rules (file size, layering)
//! are checked by walking the tree; syntax-sensitive rules are parsed with
//! `syn` so string literals and comments can't produce false positives — a
//! lesson learned when a naive grep flagged `println!` inside a test fixture.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use syn::visit::Visit;
use walkdir::WalkDir;

// ---------------------------------------------------------------------------
// Baselines — lower these as debt is paid off. Never raise them.
// ---------------------------------------------------------------------------

/// Largest permitted file, in lines. `main.rs` is currently 4,885 (DEV-105).
///
/// Ratchet plan: 5000 → 1500 → 800 as `main.rs` and the other oversized files
/// (DEV-111) are decomposed.
///
/// Only ~115 lines of headroom above `main.rs` today, which is deliberate: the
/// next feature that piles into it trips this guard rather than sliding through.
const MAX_FILE_LINES: usize = 5_000;

/// Files outside `src/platform/` that still contain `cfg(target_os = ...)`,
/// violating §7.4. Tracked as an explicit allowlist rather than a count so a
/// *new* leak fails even if an old one is fixed in the same PR (DEV-110).
const CFG_TARGET_OS_ALLOWLIST: &[&str] = &[
    "src/debug_capture.rs",
    "src/hooks/mod.rs",
    // Moved out of `src/main.rs` wholesale (DEV-415): the About panel's
    // Obj-C block came with it. Same single violation, different file.
    "src/mac_menu.rs",
    "src/memory_watchdog.rs",
    "src/scratch_pad/clipboard_image.rs",
    "src/sync/crypto.rs",
    "src/terminal/pty_terminal.rs",
];

/// Modules still using `anyhow`, against §7.6's typed-error rule (DEV-108).
///
/// Two things worth noting about this list:
///
/// 1. It is shorter than a grep for "anyhow" suggests — `errors.rs` only names
///    it in a doc comment, which the AST walk correctly ignores.
/// 2. The whole `src/sync/` subsystem is on it. Sync was added *after* §7.6 was
///    written and adopted `anyhow` throughout regardless. That is precisely the
///    drift this test exists to stop: a documented rule with no enforcement did
///    not survive contact with the next large feature.
const ANYHOW_ALLOWLIST: &[&str] = &[
    "src/assets.rs",
    "src/debug_capture.rs",
    "src/git/mod.rs",
    "src/pending_actions.rs",
    "src/session_ops.rs",
    "src/settings_window/mod.rs",
    "src/settings_window/sync.rs",
    "src/state/mod.rs",
    "src/sync/config.rs",
    "src/sync/connect.rs",
    "src/sync/crypto.rs",
    "src/sync/encrypting_store.rs",
    "src/sync/ledger.rs",
    "src/sync/pull.rs",
    "src/sync/push.rs",
    "src/sync/s3_store.rs",
    "src/sync/store.rs",
    "src/terminal/pty_terminal.rs",
    "src/trust/mod.rs",
];

/// §7.2 — argument-less `save()` calls inside `impl AppState` that are *not*
/// the coordinator. Each one persists something the render tick can't coalesce
/// and no test fixture can intercept, because it goes to the data type's
/// inherent `save()` rather than through `AppState.repos`.
///
/// DEV-520 emptied this list of its seven `handle_settings_action` entries. The
/// sync ledger is what's left: it has no repository yet (§8's "extend
/// repositories" item). Give it one and this list goes to zero.
const DIRECT_SAVE_ALLOWLIST: &[&str] = &["src/session_ops.rs::apply_pulled_session"];

/// Panic-inducing call sites (`unwrap`/`expect`) in non-test code (DEV-109).
///
/// A grep for `.unwrap()` reports a far larger number, but the overwhelming
/// majority are inside `#[cfg(test)]` modules where panicking is the point. The
/// real production panic surface is **14** across ~8 files — notably it grew by
/// only one while the crate grew by 18k lines. Ratchet down as the audit lands.
const MAX_PANIC_SITES: usize = 14;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn repo_root() -> PathBuf {
    // CARGO_MANIFEST_DIR is the crate root regardless of where cargo is invoked.
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Every `.rs` file under `src/`, as repo-relative paths with `/` separators.
fn source_files() -> Vec<(String, PathBuf)> {
    let root = repo_root();
    let mut files: Vec<(String, PathBuf)> = WalkDir::new(root.join("src"))
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "rs"))
        .map(|e| {
            let rel = e
                .path()
                .strip_prefix(&root)
                .expect("walked path is under repo root")
                .to_string_lossy()
                .replace('\\', "/");
            (rel, e.path().to_path_buf())
        })
        .collect();
    files.sort();
    assert!(!files.is_empty(), "found no source files under src/");
    files
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
}

fn parse(path: &Path, src: &str) -> syn::File {
    syn::parse_file(src).unwrap_or_else(|e| panic!("parsing {}: {e}", path.display()))
}

/// Compares an observed violation set against an allowlist and reports both
/// directions: newly-introduced violations (hard failure) and fixed ones
/// (failure too — so the allowlist gets tightened in the same PR).
fn assert_matches_allowlist(observed: &BTreeSet<String>, allowlist: &[&str], rule: &str) {
    let expected: BTreeSet<String> = allowlist.iter().map(|s| (*s).to_string()).collect();

    let new: Vec<&String> = observed.difference(&expected).collect();
    assert!(
        new.is_empty(),
        "\n{rule}\n\nNew violations introduced in:\n{}\n\nFix them, or — if genuinely \
         unavoidable — add them to the allowlist in tests/architecture.rs with a \
         justification in the PR description.\n",
        new.iter()
            .map(|f| format!("  - {f}"))
            .collect::<Vec<_>>()
            .join("\n"),
    );

    let fixed: Vec<&String> = expected.difference(observed).collect();
    assert!(
        fixed.is_empty(),
        "\n{rule}\n\nThese are now clean:\n{}\n\nRemove them from the allowlist in \
         tests/architecture.rs to lock the improvement in.\n",
        fixed
            .iter()
            .map(|f| format!("  - {f}"))
            .collect::<Vec<_>>()
            .join("\n"),
    );
}

// ---------------------------------------------------------------------------
// §7.7 / DEV-105 / DEV-111 — no god-files
// ---------------------------------------------------------------------------

#[test]
fn no_file_exceeds_the_size_ratchet() {
    let mut oversized: Vec<(String, usize)> = source_files()
        .into_iter()
        .map(|(rel, path)| (rel, read(&path).lines().count()))
        .filter(|(_, lines)| *lines > MAX_FILE_LINES)
        .collect();
    oversized.sort_by_key(|(_, lines)| std::cmp::Reverse(*lines));

    assert!(
        oversized.is_empty(),
        "\nARCHITECTURE.md §7.7 — files exceed the {MAX_FILE_LINES}-line ratchet:\n{}\n\n\
         Split the file, or raise MAX_FILE_LINES only with an explicit decision to do so.\n",
        oversized
            .iter()
            .map(|(f, n)| format!("  - {f}: {n} lines"))
            .collect::<Vec<_>>()
            .join("\n"),
    );
}

/// Guards the ratchet itself: if every file shrinks below the limit, the
/// constant must come down too, or the rule silently stops applying pressure.
#[test]
fn size_ratchet_is_still_tight() {
    let largest = source_files()
        .into_iter()
        .map(|(rel, path)| (rel, read(&path).lines().count()))
        .max_by_key(|(_, lines)| *lines)
        .expect("at least one source file");

    assert!(
        largest.1 * 2 > MAX_FILE_LINES,
        "\nThe largest file ({}, {} lines) is far below MAX_FILE_LINES ({MAX_FILE_LINES}).\n\
         Lower MAX_FILE_LINES in tests/architecture.rs to lock in the progress.\n",
        largest.0,
        largest.1,
    );
}

// ---------------------------------------------------------------------------
// §7.4 / DEV-110 — platform variance stays behind the adapter traits
// ---------------------------------------------------------------------------

#[test]
fn cfg_target_os_stays_in_platform_module() {
    let observed: BTreeSet<String> = source_files()
        .into_iter()
        .filter(|(rel, _)| !rel.starts_with("src/platform/"))
        .filter(|(_, path)| read(path).contains("cfg(target_os"))
        .map(|(rel, _)| rel)
        .collect();

    assert_matches_allowlist(
        &observed,
        CFG_TARGET_OS_ALLOWLIST,
        "ARCHITECTURE.md §7.4 — don't sprinkle #[cfg(target_os)] in business logic; \
         put platform variance behind the CloneBackend / BrowserIntegration / SystemShell traits.",
    );
}

// ---------------------------------------------------------------------------
// §7.6 / DEV-108 — typed errors at API boundaries
// ---------------------------------------------------------------------------

#[test]
fn anyhow_usage_does_not_spread() {
    let observed: BTreeSet<String> = source_files()
        .into_iter()
        .filter(|(_, path)| {
            let src = read(path);
            let file = parse(path, &src);
            let mut finder = AnyhowFinder { found: false };
            finder.visit_file(&file);
            finder.found
        })
        .map(|(rel, _)| rel)
        .collect();

    assert_matches_allowlist(
        &observed,
        ANYHOW_ALLOWLIST,
        "ARCHITECTURE.md §7.6 — use crate::errors::Result (typed AlleleError) for public \
         functions. anyhow is for internal helpers only.",
    );
}

/// Detects `anyhow` in paths (`anyhow::Result`), `use` statements, and the
/// `anyhow!`/`bail!`/`ensure!` macros — without matching the word in a comment.
struct AnyhowFinder {
    found: bool,
}

impl<'ast> Visit<'ast> for AnyhowFinder {
    fn visit_path(&mut self, path: &'ast syn::Path) {
        if path.segments.iter().any(|s| s.ident == "anyhow") {
            self.found = true;
        }
        syn::visit::visit_path(self, path);
    }

    fn visit_item_use(&mut self, item: &'ast syn::ItemUse) {
        // `use anyhow::...` — the tree isn't a Path, so check it directly.
        if let syn::UseTree::Path(p) = &item.tree {
            if p.ident == "anyhow" {
                self.found = true;
            }
        }
        syn::visit::visit_item_use(self, item);
    }
}

// ---------------------------------------------------------------------------
// §7.3 — platform::global() is a leaf-only escape hatch
// ---------------------------------------------------------------------------

/// §7.3: "Don't reach for `platform::global()` from `AppState` methods — use
/// `self.platform`." The global exists only for deeply-nested GPUI leaf
/// entities that can't accept injected dependencies.
///
/// This is a rule the compiler can't express, and it is exactly the kind of
/// thing that erodes silently — hence a test.
#[test]
fn platform_global_is_not_called_from_app_state_methods() {
    let mut violations = Vec::new();

    for (rel, path) in source_files() {
        let src = read(&path);
        if !src.contains("platform::global") {
            continue;
        }
        let file = parse(&path, &src);

        for item in &file.items {
            let syn::Item::Impl(item_impl) = item else {
                continue;
            };
            // Only `impl AppState` / `impl Render for AppState` blocks.
            let syn::Type::Path(ty) = &*item_impl.self_ty else {
                continue;
            };
            if ty
                .path
                .segments
                .last()
                .is_none_or(|s| s.ident != "AppState")
            {
                continue;
            }

            let mut finder = GlobalCallFinder { found: false };
            finder.visit_item_impl(item_impl);
            if finder.found {
                violations.push(rel.clone());
            }
        }
    }

    violations.sort();
    violations.dedup();
    assert!(
        violations.is_empty(),
        "\nARCHITECTURE.md §7.3 — platform::global() called from inside an impl AppState \
         block in:\n{}\n\nUse self.platform instead; the global is only for leaf GPUI \
         entities that cannot receive an injected dependency.\n",
        violations
            .iter()
            .map(|f| format!("  - {f}"))
            .collect::<Vec<_>>()
            .join("\n"),
    );
}

struct GlobalCallFinder {
    found: bool,
}

impl<'ast> Visit<'ast> for GlobalCallFinder {
    fn visit_expr_call(&mut self, call: &'ast syn::ExprCall) {
        if let syn::Expr::Path(p) = &*call.func {
            let segs: Vec<String> = p
                .path
                .segments
                .iter()
                .map(|s| s.ident.to_string())
                .collect();
            if segs.len() >= 2
                && segs[segs.len() - 2] == "platform"
                && segs[segs.len() - 1] == "global"
            {
                self.found = true;
            }
        }
        syn::visit::visit_expr_call(self, call);
    }
}

// ---------------------------------------------------------------------------
// §7.1 — structured logging only
// ---------------------------------------------------------------------------

/// §7.1: use `tracing::{info, warn, error}`, not `println!`/`eprintln!`, so the
/// `ALLELE_LOG` env filter sees everything.
///
/// Parsed with `syn` rather than grepped: the naive grep that motivated this
/// test matched `println!` inside a markdown test fixture *string literal* and
/// inside the legitimate `--capture-ui` CLI path. Only real macro invocations
/// in non-test, non-CLI code count.
#[test]
fn no_print_macros_outside_cli_entry_point() {
    let mut violations = Vec::new();

    for (rel, path) in source_files() {
        // main() owns the process's stdout/stderr for CLI subcommands
        // (`--capture-ui` prints a path for the caller to consume). That is a
        // real interface, not logging.
        if rel == "src/main.rs" {
            continue;
        }

        let src = read(&path);
        let file = parse(&path, &src);
        let mut finder = PrintMacroFinder {
            hits: Vec::new(),
            in_test: false,
        };
        finder.visit_file(&file);
        for macro_name in finder.hits {
            violations.push(format!("  - {rel}: {macro_name}!"));
        }
    }

    assert!(
        violations.is_empty(),
        "\nARCHITECTURE.md §7.1 — use tracing::{{info, warn, error}} instead of print macros:\n{}\n",
        violations.join("\n"),
    );
}

struct PrintMacroFinder {
    hits: Vec<String>,
    in_test: bool,
}

fn quote_path(path: &syn::Path) -> String {
    path.segments
        .iter()
        .map(|s| s.ident.to_string())
        .collect::<Vec<_>>()
        .join("::")
}

/// True for `#[test]`, `#[gpui::test]`, and `#[cfg(test)]` — the attributes that
/// mark code where `unwrap`/`expect`/print macros are acceptable.
fn is_test_gated(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|attr| {
        let path = quote_path(attr.path());
        if path == "test" || path.ends_with("::test") {
            return true;
        }
        if path != "cfg" {
            return false;
        }
        match &attr.meta {
            syn::Meta::List(list) => list.tokens.to_string().contains("test"),
            _ => false,
        }
    })
}

impl<'ast> Visit<'ast> for PrintMacroFinder {
    fn visit_item_mod(&mut self, item: &'ast syn::ItemMod) {
        let was_in_test = self.in_test;
        if is_test_gated(&item.attrs) {
            self.in_test = true;
        }
        syn::visit::visit_item_mod(self, item);
        self.in_test = was_in_test;
    }

    fn visit_item_fn(&mut self, item: &'ast syn::ItemFn) {
        let was_in_test = self.in_test;
        if is_test_gated(&item.attrs) {
            self.in_test = true;
        }
        syn::visit::visit_item_fn(self, item);
        self.in_test = was_in_test;
    }

    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        if self.in_test {
            return;
        }
        let name = quote_path(&mac.path);
        if matches!(name.as_str(), "println" | "eprintln" | "print" | "eprint") {
            self.hits.push(name);
        }
        syn::visit::visit_macro(self, mac);
    }
}

// ---------------------------------------------------------------------------
// DEV-109 — panic surface ratchet
// ---------------------------------------------------------------------------

/// A native desktop app that panics loses every running session, not just the
/// failing operation. This counts `unwrap`/`expect` in non-test code and fails
/// if the total grows. Lower `MAX_PANIC_SITES` as the audit lands.
#[test]
fn panic_sites_do_not_increase() {
    let mut total = 0usize;
    let mut per_file: Vec<(String, usize)> = Vec::new();

    for (rel, path) in source_files() {
        let src = read(&path);
        let file = parse(&path, &src);
        let mut finder = PanicSiteFinder {
            count: 0,
            in_test: false,
        };
        finder.visit_file(&file);
        if finder.count > 0 {
            total += finder.count;
            per_file.push((rel, finder.count));
        }
    }

    per_file.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
    let worst: Vec<String> = per_file
        .iter()
        .take(5)
        .map(|(f, n)| format!("  - {f}: {n}"))
        .collect();

    assert!(
        total <= MAX_PANIC_SITES,
        "\nDEV-109 — panic sites (unwrap/expect in non-test code) rose to {total}, \
         above the {MAX_PANIC_SITES} baseline.\n\nWorst offenders:\n{}\n\n\
         Prefer returning AlleleError, or log-and-continue for recoverable failures.\n",
        worst.join("\n"),
    );

    assert!(
        total >= MAX_PANIC_SITES.saturating_sub(3),
        "\nPanic sites dropped to {total}, under the {MAX_PANIC_SITES} baseline.\n\
         Lower MAX_PANIC_SITES in tests/architecture.rs to lock the improvement in.\n",
    );
}

struct PanicSiteFinder {
    count: usize,
    in_test: bool,
}

impl<'ast> Visit<'ast> for PanicSiteFinder {
    fn visit_item_mod(&mut self, item: &'ast syn::ItemMod) {
        let was = self.in_test;
        if is_test_gated(&item.attrs) {
            self.in_test = true;
        }
        syn::visit::visit_item_mod(self, item);
        self.in_test = was;
    }

    fn visit_item_fn(&mut self, item: &'ast syn::ItemFn) {
        let was = self.in_test;
        if is_test_gated(&item.attrs) {
            self.in_test = true;
        }
        syn::visit::visit_item_fn(self, item);
        self.in_test = was;
    }

    fn visit_expr_method_call(&mut self, call: &'ast syn::ExprMethodCall) {
        if !self.in_test && (call.method == "unwrap" || call.method == "expect") {
            self.count += 1;
        }
        syn::visit::visit_expr_method_call(self, call);
    }
}

// ---------------------------------------------------------------------------
// §7.2 — handlers mark dirty; the coordinator writes
// ---------------------------------------------------------------------------

/// §7.2: "Don't call `save_state()` / `save_settings()` from handlers." Until
/// DEV-106 makes those methods module-private the compiler cannot enforce it,
/// and `clippy.toml` says as much — its `disallowed-methods` entry "can only be
/// advisory until DEV-106". This test is the enforcement in the meantime.
///
/// It flags any zero-argument `.save()` inside an `impl AppState` block. The
/// arity is the discriminator, and it is exact: every bypassing route takes no
/// arguments (`self.save_settings()`, `snapshot.save()`,
/// `PersistedState::save()`), while the sanctioned one takes the value
/// (`self.repos.settings.save(&settings)`). `checkpoint_persistence` is the one
/// method allowed to write, because it *is* the coordinator.
///
/// Why this rule needs a test rather than review: the failure is silent. Seven
/// `handle_settings_action` arms bypassed the coordinator for months (DEV-520)
/// and nothing looked broken — the value still persisted, just immediately,
/// un-coalesced, and into the developer's real `~/.config/allele/settings.json`
/// even under `cargo test`.
#[test]
fn only_the_coordinator_saves_directly() {
    let mut observed = BTreeSet::new();

    for (rel, path) in source_files() {
        let src = read(&path);
        let file = parse(&path, &src);

        for item in &file.items {
            let syn::Item::Impl(item_impl) = item else {
                continue;
            };
            let syn::Type::Path(ty) = &*item_impl.self_ty else {
                continue;
            };
            if ty
                .path
                .segments
                .last()
                .is_none_or(|s| s.ident != "AppState")
            {
                continue;
            }

            for impl_item in &item_impl.items {
                let syn::ImplItem::Fn(method) = impl_item else {
                    continue;
                };
                // The coordinator is the sanctioned writer; test code may do
                // whatever it likes, since the fakes catch it.
                if method.sig.ident == "checkpoint_persistence" || is_test_gated(&method.attrs) {
                    continue;
                }

                let mut finder = DirectSaveFinder { found: false };
                finder.visit_block(&method.block);
                if finder.found {
                    observed.insert(format!("{rel}::{}", method.sig.ident));
                }
            }
        }
    }

    assert_matches_allowlist(
        &observed,
        DIRECT_SAVE_ALLOWLIST,
        "ARCHITECTURE.md §7.2 — an AppState method persisted directly instead of \
         calling mark_state_dirty() / mark_settings_dirty() and letting \
         checkpoint_persistence() coalesce the write",
    );
}

/// Any `.save()` taking no arguments — see `only_the_coordinator_saves_directly`
/// for why arity is the right discriminator.
struct DirectSaveFinder {
    found: bool,
}

impl<'ast> Visit<'ast> for DirectSaveFinder {
    fn visit_expr_method_call(&mut self, call: &'ast syn::ExprMethodCall) {
        if call.method == "save" && call.args.is_empty() {
            self.found = true;
        }
        syn::visit::visit_expr_method_call(self, call);
    }
}

// ---------------------------------------------------------------------------
// Meta — the docs and the tests must agree
// ---------------------------------------------------------------------------

/// The allowlists above are only meaningful if `ARCHITECTURE.md` still contains
/// the sections they cite. If someone renumbers or deletes §7, these tests are
/// enforcing rules nobody can look up.
#[test]
fn architecture_doc_still_documents_the_enforced_rules() {
    let doc = read(&repo_root().join("ARCHITECTURE.md"));
    for anchor in [
        "### 7.1 Don't call `eprintln!`",
        "### 7.2 Don't call `save_state()` / `save_settings()` from handlers",
        "### 7.3 Don't reach for `platform::global()` from `AppState` methods",
        "### 7.4 Don't sprinkle `#[cfg(target_os = \"macos\")]` in business logic",
        "### 7.6 Don't use `anyhow::Result<T>` for new public functions",
        "### 7.7 Don't introduce a new god-object",
    ] {
        assert!(
            doc.contains(anchor),
            "ARCHITECTURE.md no longer contains {anchor:?} — tests/architecture.rs enforces \
             it. Update both together.",
        );
    }
}
