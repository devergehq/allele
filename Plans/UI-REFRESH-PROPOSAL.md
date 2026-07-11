# Allele UI Refresh — "Native Lab Instrument"

**Status:** Proposal (2026-07-11). No code changes yet.
**Problem:** The UI reads as technical/lo-fi/dated next to modern macOS apps (Linear, Warp, Raycast, Ghostty). This document is the audit of why, and a phased direction to fix it without losing Allele's terminal-first density or genetics identity.

---

## Part 1 — Why it feels dated (audit)

Grounded in a full code sweep + live screenshot. The short version: **the chrome is styled like terminal output.**

| # | Finding | Evidence |
|---|---------|----------|
| 1 | **White native titlebar clashes with the all-dark app** — the single loudest "unfinished" signal | `src/main.rs:1930` uses default `TitlebarOptions`; no appearance override |
| 2 | **No theme system** — ~28 Catppuccin Mocha hex values hardcoded at 500+ call sites, plus ad-hoc tints (`0x3b1f28`, `0x2a2a3c`, …) | every UI file; local palette copies in `src/rich/rich_view.rs:24-39`, `src/session/mod.rs:58-66` |
| 3 | **Monospace everywhere** — JetBrains Mono (or GPUI default) for all chrome; no system font (SF Pro) anywhere | 14× `font_family("JetBrains Mono")`, 0× system font |
| 4 | **Tiny dense type** — chrome dominated by 10–12px (62× 11px, 38× 10px, 6× 9px); no semantic type scale | `src/sidebar/render.rs:63-72`, `src/main.rs:304-318` |
| 5 | **Unicode/emoji as icons** — `▾ ● ○ ✓ ⏸ ⚠ ★ 📌 🗑 📦 📎 ⚙` instead of vector icons | `src/session/mod.rs:47-55`, `src/sidebar/render.rs:123-197` |
| 6 | **Flat, border-based depth** — 1px borders separate everything; shadows only on modals; no vibrancy/materials | `src/main.rs:2562-2564`, `src/drawer/mod.rs:390-399` |
| 7 | **Zero animation** — toggles, drawers, modals, hovers all switch instantly | e.g. instant toggle thumb at `src/settings_window.rs:951-966` |
| 8 | **Inconsistent radii** — 2–12px scattered, mostly sharp 3–4px | 55× 4px, 24× 3px, then 6/7/8/9/12 |
| 9 | **Handmade non-native settings controls** — flat pills, fixed 180px sidebar; nothing like macOS System Settings | `src/settings_window.rs:585-618, 930-1007` |
| 10 | **Icon-only micro hit-targets** — sidebar action clusters (`+ ▸ ⊙ ×`) are cryptic and cramped | `src/sidebar/render.rs:785-856` |

GPUI (pinned Zed rev `83de8a2`) already supports everything the fix needs: `WindowBackgroundAppearance::Blurred`, an `elements/animation.rs` module, `svg()` elements, `appears_transparent` titlebars with `traffic_light_position`, and full font weights (Medium/Semibold). Zed itself is the existence proof.

## Part 2 — Direction

Three directions were generated and evaluated by a three-member council (designer / GPUI engineer / trends researcher, run out-of-process):

- **A. Native Instrument** — fully macOS-native (SF Pro, vibrancy, light+dark)
- **B. Refined Lab** — stay dark+dense, professionalize (tokens, SVG icons, elevation, motion)
- **C. Living Organism** — genetics metaphor as the visual language

**Verdict (unanimous): hybrid — B as the base, A for the shell, C only as restrained accents.**
Allele should feel like a *serious macOS power tool*: native enough to belong on the platform, dense enough for running 5+ parallel sessions, distinctive enough to be memorable. C as a whole-app theme risks gimmick over legibility.

Key principles:
1. **SF Pro for chrome, JetBrains Mono for content.** Monospace is the terminal's voice, not the app's.
2. **Layered elevation, not borders.** Separate surfaces by background level (base → raised → overlay) + soft shadows; reserve 1px borders for interactive affordances.
3. **Density stays, illegibility goes.** Keep compact rows; move the chrome floor from 10px to ~12px with a real scale.
4. **Motion clarifies state, never decorates.** 120–180ms ease on toggles, drawer, modals, selection; a "breathing" running-session dot is the one identity flourish.
5. **Token-first.** Every change flows through a semantic theme module so light mode later is a palette swap, not another rewrite.

## Part 3 — Phased plan

### Phase 1 — Foundation: design tokens (enables everything else)
- New `src/theme.rs`: semantic roles (`bg_base`, `bg_raised`, `bg_overlay`, `text_primary/secondary/muted`, `accent`, `danger`, `success`, `warning`, `border_subtle`, `border_interactive`…) mapping the existing Mocha values, structured as a swappable `Theme` struct.
- Type scale: `text_xs(11) / sm(12) / base(13) / lg(15) / xl(17)` + weight conventions; chrome switches to system font (`.SystemUIFont` / "SF Pro Text").
- Spacing + radius scale: radii collapse to `4 / 6 / 10` (controls / cards / windows-popovers).
- Mechanical migration of the 500+ call sites file-by-file (sidebar → main → settings → rich → rest). No visual redesign yet; the app should look near-identical after this phase.

### Phase 2 — Native shell (biggest visible win, small code)
- `appears_transparent` titlebar + `traffic_light_position`; fold the Claude/Editor/Transcript tab strip into the titlebar row (like Warp/Arc) — kills the white-bar clash and buys vertical space.
- `WindowBackgroundAppearance::Blurred` + slightly translucent sidebar background for macOS vibrancy.
- Window/appearance set to dark so system chrome matches.

### Phase 3 — Iconography & controls
- SVG icon set (Lucide-style, bundled in `assets/icons/svg/`) via GPUI `svg()`: replace all glyph/emoji icons; status indicators become drawn dots/badges with shape+color redundancy.
- Settings rebuilt with grouped cards, real toggle/select components, roomier hit targets.
- Sidebar rows: larger touch targets, actions hidden until hover (Linear-style), status dot left-aligned, session name at 12–13px.

### Phase 4 — Depth & motion
- Elevation model applied: sidebar = base, content = raised, popovers/modals = overlay + shadow; delete most separator borders.
- Animation pass via GPUI's animation element: drawer slide, modal fade+scale, toggle thumb, hover transitions, animated breathing dot for Running sessions.

### Phase 5 — Identity accents (small, last)
- Helix-gradient accent for empty states / onboarding / About.
- Session lineage cues in the archive browser (variant-of relationships).

### Quick wins (can ship independently, this week)
1. Titlebar transparency + dark appearance (Phase 2a) — hours, biggest perceived jump.
2. Chrome font floor 10→12px + system font in sidebar/tabs.
3. Radii normalized to 6px on cards/buttons.
4. Hover-reveal for sidebar action clusters.

## Risks
- **Token migration is foundational, not cosmetic** — 500+ call sites; do it file-by-file with screenshots before/after to catch regressions.
- **Vibrancy vs. legibility** — test blur behind the sidebar with real content; back off to solid if contrast suffers.
- **Density loss** — audit every size bump against a 10-session sidebar.
- **GPUI animation perf** — animate opacity/offset only; never per-frame relayout of the terminal grid.
