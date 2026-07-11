# Allele UI/UX Review: Attention Inbox + Session Workbench

**Date:** 2026-07-11
**Baseline:** Current post-refresh build through DEV-17
**Scope:** Product experience, interaction design, information architecture, accessibility, and visual hierarchy
**Method:** Independent workflow audit first; previous refresh proposal and Linear plan compared afterward

## Executive Summary

Allele's July visual refresh succeeded. The app now has a coherent palette, native-feeling shell, system-font chrome, SVG icons, restrained motion, and a clearer settings surface. The app no longer primarily feels dated because of raw styling.

The next modernization problem is structural: **Allele presents concurrent sessions, but does not yet fully orchestrate the user's attention across them.** Its strongest product capability, knowing when agents need input or have results, is spread across row ordering, status icons, footer counts, banners, notifications, and transcripts. Users still have to reconstruct what changed, what matters, and what action is safe.

The recommended direction is **Attention Inbox + Session Workbench**:

1. Keep projects and sessions as stable navigation.
2. Add a cross-project attention queue for blocked, failed, and completed work.
3. Make the center a coherent workbench rather than four implementation-mode tabs.
4. Consolidate lifecycle actions and session metadata into an inspector/action model.
5. Add a global command palette for discovery and keyboard parity.

This is not a second visual refresh. It is a product-model refresh built on the visual foundation already shipped.

## Product Axioms

Allele exists for people running five or more coding-agent sessions. At that scale, the scarce resource is not terminal space. It is human attention.

The interface therefore needs to answer five questions quickly:

1. What changed since I last looked?
2. Which session needs me now?
3. What decision is it asking me to make?
4. What has finished and needs review?
5. What is the safest next lifecycle action?

The current app answers each question somewhere, but not through one coherent model.

## Current Experience

### What Works

**The core product concept is strong.** Projects, isolated sessions, status routing, transcript rendering, changes, browser context, and lifecycle management belong together.

**Attention has real underlying state.** `AwaitingInput` and `ResponseReady` are not decorative badges; they affect sorting, summaries, banners, sounds, and notifications (`src/session/mod.rs`, `src/main.rs:367-514`, `src/sidebar/render.rs:509-525`).

**The app gives useful operation feedback.** Clone and merge operations receive loading placeholders instead of appearing frozen (`src/sidebar/render.rs:462-507`).

**Rich mode is strategically valuable.** It renders prompts, prose, tool calls, diffs, failures, permission requests, and composition without replacing the real PTY (`src/rich/rich_view.rs`).

**The changes panel has a sound state model.** Loading, stale-result protection, clean, non-repository, binary, no-diff, and truncation cases are represented (`src/changes/mod.rs`).

**The visual foundation is now viable.** The transparent shell, tokens, SF Pro chrome, iconography, cards, animation, and identity accents make iterative improvement practical.

### Layout Assessment From Current Source

The current post-refresh source defines a cleaner shell, but it also indicates several hierarchy risks that need validation in the latest running build:

- Sidebar, top strip, terminal, context panel, and changes panel occupy similar dark planes.
- Project headers and session rows differ more by borders than by role.
- Active session selection is visible, but actionable urgency is not dominant.
- Session rows spend substantial width on elapsed time and hover actions.
- The right context area can compete with the primary terminal without a clear inspector relationship.
- Top tabs look like mode switches but use internal nouns: Claude, Editor, Transcript, Browser.
- The status footer includes implementation telemetry (`fps`) alongside user-relevant state.
- The overall density is appropriate, but prioritization inside that density is weak.

## Priority Findings

### P0: Create One Attention Model

**Problem**

Attention is represented in too many places. A session can be promoted in the tree, counted in the footer, shown in a top banner, announced by notification, and represented inside Rich mode. This creates signal duplication without a durable triage flow.

**Recommendation**

Add an **Attention Inbox** above or beside the project tree. It should contain durable, cross-project items for:

- Needs approval
- Needs answer
- Failed
- Ready to review
- Completed since last viewed

Each item should show project, session, reason, age, and one primary action. Support keyboard traversal, mark reviewed, and optional snooze. Clearing an item should follow explicit state transitions, not merely selecting the session.

Keep the tree as navigation. Do not turn it into the inbox by repeatedly reordering it; stable location lowers spatial-memory cost.

**Impact:** Very high
**Confidence:** High
**Effort:** Large

### P0: Make Permission Handling a Decision Surface

**Problem**

The top attention bar can inject Enter for an approval, but the user may not receive enough context to make that decision safely. A generic `Allow` action is fast but can normalize blind approval (`src/main.rs:367-514`).

**Recommendation**

Render permission requests as compact decision cards:

- Tool and plain-language purpose
- Target file, command, or external resource
- Risk cue when destructive or external
- `Allow once`, `Open session`, and `Reject` actions where supported
- Full request detail on disclosure

The inbox item and Rich permission block should be two views of the same decision object.

**Impact:** Very high
**Confidence:** High
**Effort:** Medium

### P0: Surface Failures In Context

**Problem**

Several failures are written to logs with weak or no user-facing recovery: attachment failures, archive merges, diff loads, and missing suspended clones. The visible result can be an inert control or generic empty state.

Examples:

- Attachment errors: `src/rich/compose_bar.rs:581-601`
- Archive merge errors: `src/pending_actions.rs:489-493`
- Suspended clone failures: `src/session_ops.rs:771-788`
- Changes errors collapse toward empty output: `src/changes/mod.rs:111-114,157-162`

**Recommendation**

Use a shared in-app feedback pattern:

- Inline error beside the originating action
- Clear consequence in plain language
- Recovery action such as Retry, Locate workspace, Open log, or Copy details
- Persistent entry in the Attention Inbox for unresolved failures

Do not use an empty state to represent an error.

**Impact:** Very high
**Confidence:** High
**Effort:** Medium

### P1: Replace Modes With a Session Workbench

**Problem**

`Claude`, `Editor`, `Transcript`, and `Browser` describe underlying surfaces rather than the user's workflow. Browser can disappear based on configuration, and terminal versus transcript feels like choosing between parallel products (`src/main.rs:293-364`).

**Recommendation**

Make the active session a coherent workbench:

- **Activity:** summary, conversation, tool calls, permissions, outcomes
- **Terminal:** exact PTY, always available
- **Changes:** files and diffs
- **Preview:** browser state when configured

Activity should become the default once fidelity and permission handling are trusted. Terminal remains one click or shortcut away and should be visibly available, not demoted.

The external editor/file tree can live in a left or right workbench pane rather than competing as an equal top-level mode.

**Impact:** High
**Confidence:** Medium-high
**Effort:** Large

### P1: Add Session Summary Headers

**Problem**

After switching sessions, the user must infer context from terminal output, sidebar text, branch state, and optional comment. Context reconstruction is expensive.

**Recommendation**

Give every active session a compact header containing:

- Session name and project
- Branch
- Current state in words
- Last meaningful activity
- Changed-file count
- Agent profile
- Primary next action

Keep elapsed runtime secondary. “Waiting for approval: Bash” is more valuable than “9m 44s.”

**Impact:** High
**Confidence:** High
**Effort:** Medium

### P1: Consolidate Lifecycle Actions

**Problem**

Merge, suspend, discard, edit, pin, and archive actions appear in hover clusters, context menus, inline confirmations, and separate archive controls. Meanings such as close versus suspend versus discard require learned knowledge. Archive deletion is immediate (`src/sidebar/render.rs:990-1010`).

**Recommendation**

Use one consistent session action panel available from:

- Visible row overflow button
- Right-click menu
- Command palette
- Keyboard shortcuts where safe

Group actions by outcome:

- Continue: Open, Resume, Edit details
- Preserve: Suspend, Archive
- Integrate: Review changes, Merge and close
- Destructive: Discard workspace, Delete archive

Irreversible actions require explicit consequences and confirmation. Reversible actions should prefer undo.

**Impact:** High
**Confidence:** High
**Effort:** Medium

### P1: Fix Creation Discoverability

**Problem**

Opening a project is represented by a small `+`. New session and detailed new session appear only on project-header hover; a chevron distinguishes the detailed path (`src/main.rs:2614-2630`, `src/sidebar/render.rs:123-162`). The first-run message points at the weak control.

**Recommendation**

- Use an explicit `Open Project` button in first-run state.
- Give every project a visible or consistently reachable `New Session` action.
- Make quick creation the primary click.
- Put advanced creation in the adjacent menu, not a second unexplained icon.
- Replace click-to-cycle agent selection with a real menu listing choices.

**Impact:** High
**Confidence:** High
**Effort:** Small-medium

### P1: Add a Global Command Palette

**Problem**

Allele has valuable shortcuts, but discovery is fragmented across tooltips, menus, and documentation. Sidebar actions have no complete keyboard model, and the default keymap exposes only a small global set (`assets/default-keymap.json`).

**Recommendation**

Add `Cmd+P` or `Cmd+Shift+P` command search covering:

- Switch to project or session
- Show attention items
- Create session
- Open workbench surfaces
- Lifecycle actions
- Toggle panels
- Open settings
- Search available commands and shortcuts

Commands should be contextual, grouped, and explicit about destructive effects.

**Impact:** High
**Confidence:** High
**Effort:** Medium-large

### P2: Stabilize Navigation

**Problem**

Attention promotion changes row positions, while stable spatial location is valuable when managing many sessions. Conditional tabs can also disappear.

**Recommendation**

- Keep the project tree stable by default.
- Put urgent items in the inbox rather than moving their canonical rows.
- Preserve unavailable workbench tabs in disabled form with setup guidance when useful.
- Add explicit unread or changed-since-view markers.
- Keep pinned sessions stable within their project.

**Impact:** Medium-high
**Confidence:** Medium-high
**Effort:** Medium

### P2: Unify Configuration

**Problem**

Configuration spans inline project settings, the separate settings window, `settings.json`, and `allele.json`. Branch and remote values are read-only in one surface with instructions to edit a file (`src/sidebar/render.rs:429-457`).

**Recommendation**

- Keep global settings in Settings.
- Put complete project settings in one project inspector or Settings pane.
- Show configuration source and precedence explicitly.
- Offer `Open allele.json` only for advanced overrides.
- Remove the partial inline project settings panel after equivalent controls exist.

**Impact:** Medium-high
**Confidence:** High
**Effort:** Medium

### P2: Establish Accessibility Floors

**Problem**

The refresh replaced glyphs and added shapes, but the app still uses 20px and smaller controls, hover-only actions, incomplete keyboard traversal, no visible reduced-motion handling, and many custom controls.

**Recommendation**

- Use approximately 24pt as the dense-control interaction floor.
- Never make an important action available only on hover.
- Add visible keyboard focus to custom controls.
- Implement keyboard navigation for trees, lists, tabs, menus, and dialogs.
- Respect macOS Reduce Motion; stop the breathing status animation when enabled.
- Test Increase Contrast and Reduce Transparency.
- Expose status changes through native accessibility announcements.

**Impact:** Medium-high
**Confidence:** High
**Effort:** Large, incremental

### P2: Simplify Operational Telemetry

**Problem**

The sidebar footer mixes project/session counts, running count, attention counts, and FPS. FPS is engineering telemetry in a primary user surface (`src/main.rs:2659-2707`).

**Recommendation**

Keep only user-relevant global status in the footer. Move FPS and diagnostics behind a developer diagnostics toggle or About/Debug surface.

**Impact:** Medium
**Confidence:** High
**Effort:** Small

## Surface Review

### Sidebar

Keep it, but clarify its job: navigation and stable state, not triage. Reduce repeated icon clusters, strengthen project/session typography, use readable state labels for exceptions, and make overflow actions keyboard-accessible.

### Attention Bar

Promote it into the entry point for the Attention Inbox. A banner is appropriate for one urgent item; it becomes noisy and vertically expensive for several.

### Activity / Transcript

This is the highest-upside surface. Improve grouping by turn, add a sticky “current state” indicator, preserve scroll position when reviewing history, expose permission decisions, and support a concise summary mode. Tool cards should state outcome before payload.

### Terminal

Preserve exact fidelity. Avoid styling work that competes with terminal content. The terminal should become a deliberate workbench surface, not the only place where truth can be understood.

### Changes

Integrate changes more tightly with session review. Add file-type icons, directory compression, explicit load failures, keyboard traversal, and an “open in editor” path. Avoid showing file list and diff as two cramped vertical halves at narrow widths.

### Drawer

The drawer is useful operational context, but named terminal tabs need clearer relationship to the active session and project setup. Consider presenting it as a collapsible “Processes” region with health/status where known.

### Scratch Pad and Compose

These overlap conceptually. Converge on one reusable composer that can appear inline in Activity and as a global overlay. Preserve history and attachments. Show attachment errors inline. Ensure attachment-only submissions have a clearly enabled send state.

### New Session

Use progressive disclosure: project and prompt first; branch and agent defaults summarized; advanced details expandable. Replace cycle-on-click controls with menus. Preview the resulting branch and workspace behavior in plain language.

### Settings

The grouped-card refresh is a good base. Add search, complete keyboard navigation, consistent control components, validation, save feedback, and source-precedence explanations. The eight-section flat list will need grouping as options grow.

### Empty, Loading, Error States

Empty states should contain the relevant primary action. Loading states should state the operation and allow safe cancellation where possible. Error states need cause, consequence, and recovery; they must never collapse into “No data.”

## Direction Options

### A. Polished Workspace Tree

Continue improving the current tree-plus-terminal structure.

**Strengths:** Lowest change risk, preserves density, fastest delivery.
**Weaknesses:** Does not solve cross-project attention or context reconstruction.
**Verdict:** Necessary incremental work, insufficient as the product direction.

### B. Mission Control Dashboard

Make the default view a dashboard of running sessions, states, progress, and metrics.

**Strengths:** Excellent overview and visual differentiation.
**Weaknesses:** Risks becoming a monitoring screen that users leave immediately to do real work; card layouts scale poorly with many sessions.
**Verdict:** Use selectively for overview, not as the primary shell.

### C. Attention Inbox + Session Workbench

Separate triage from navigation, then give each selected session a coherent workbench.

**Strengths:** Directly addresses the scarce resource, scales across projects, preserves terminal fidelity, supports durable triage.
**Weaknesses:** Requires new state semantics such as unread/reviewed/snoozed and careful migration from existing signals.
**Verdict:** Recommended.

## Recommended Information Architecture

```text
Sidebar
  Attention
    Needs input
    Failed
    Ready to review
  Projects
    Project
      Sessions

Session Workbench
  Header: project / session / branch / state / next action
  Activity | Terminal | Changes | Preview

Inspector
  Session details
  Lifecycle actions
  Agent and workspace metadata

Bottom Region
  Processes / project terminals
```

On smaller windows, the inspector and processes region collapse. Attention remains reachable from a badge or command.

## Delivery Roadmap

### Phase 1: Safety and Feedback

- Add user-facing errors for silent failure paths.
- Confirm archive deletion.
- Replace ambiguous creation controls.
- Remove FPS from default footer.
- Correct attachment-only send state.
- Add visible overflow actions and keyboard/menu equivalents.

### Phase 2: Attention Inbox

- Define attention item states and transitions.
- Build cross-project inbox list.
- Route permission, failure, and ready events into it.
- Add reviewed and optional snooze behavior.
- Stop reordering canonical tree rows by default.

### Phase 3: Session Workbench

- Add session summary header.
- Rename and reorganize workbench surfaces.
- Promote Activity toward the default experience.
- Integrate Changes and Preview consistently.
- Add session inspector.

### Phase 4: Command and Accessibility Layer

- Add command palette.
- Add complete keyboard navigation.
- Add focus styles and accessibility labels.
- Respect Reduce Motion, Increase Contrast, and Reduce Transparency.
- Raise interaction target floors.

### Phase 5: Configuration Consolidation

- Unify project settings.
- Expose configuration precedence.
- Remove partial inline settings duplication.
- Add settings search and validation feedback.

## Suggested Linear Changes

The original `UI Refresh` project should be considered substantially complete after DEV-1 through DEV-6 and DEV-17. Do not extend it indefinitely with product-architecture work.

Create a new project: **UX Modernization: Attention + Workbench**.

Suggested issues:

| Priority | Issue | Scope |
|---|---|---|
| Urgent | Surface silent operation failures | Attachments, archive merge, resume, changes |
| Urgent | Make archive deletion safe | Confirmation or undo plus explicit consequence |
| High | Redesign project and session creation affordances | First-run CTA, one primary create path, real agent menu |
| High | Define attention item state model | Needs input, failed, ready, reviewed, snoozed |
| High | Build cross-project Attention Inbox | Durable queue, keyboard triage, primary actions |
| High | Redesign permission decision cards | Context, risk, allow/open/reject |
| High | Add session workbench header | Context, state, changes, next action |
| High | Consolidate session lifecycle actions | Inspector, overflow, command equivalents, safety |
| High | Add global command palette | Discovery, navigation, contextual actions |
| Medium | Reorganize center surfaces as workbench | Activity, Terminal, Changes, Preview |
| Medium | Unify composer surfaces | Activity compose, Scratch Pad, attachments, history |
| Medium | Add keyboard and focus navigation | Trees, tabs, lists, menus, dialogs |
| Medium | Respect macOS accessibility preferences | Motion, contrast, transparency, announcements |
| Medium | Consolidate project configuration | Settings, source precedence, validation |
| Low | Add optional overview dashboard | Aggregate state only after inbox semantics exist |

Existing `Core Polish & QoL` issues should remain separate unless they directly implement one of these journeys. Drag reorder, split views, caching, box drawing, templates, and generic text-input work do not replace the attention/workbench roadmap.

## Comparison With Previous Proposal

### Where It Was Right

- The visual baseline was dated.
- The native shell, semantic tokens, SF Pro/mono split, SVG icons, restrained motion, and identity accents were appropriate.
- Density needed refinement rather than removal.
- The genetics metaphor works best as a quiet accent.

### What Has Since Shipped

- DEV-1: titlebar, font floor, radii, hover reveal
- DEV-2: semantic theme foundation
- DEV-3: native shell and vibrancy
- DEV-4: SVG icon system and status shapes
- DEV-5: depth and motion
- DEV-6: identity accents
- DEV-17: grouped settings cards and animated toggles

### What The Previous Proposal Did Not Address

- One durable attention and triage model
- Permission decisions as a first-class workflow
- Cross-project context reconstruction
- Stable navigation versus attention-driven reordering
- Session workbench information architecture
- Failure recovery and silent error paths
- Command discovery and keyboard completeness
- Accessibility preferences and focus navigation
- Configuration-source coherence

The earlier proposal should therefore remain as the visual-foundation record. This review should drive the next product-design phase.

## Success Measures

Instrument these before and after the modernization:

- Median time from attention event to user action
- Percentage of attention events acted on without manual session scanning
- Time to resume context after switching sessions
- Rate of blind permission approvals versus opened-detail decisions
- Lifecycle cancellation or reversal rate
- Session creation completion time
- Error recovery success without opening logs
- Command-palette usage and shortcut discovery
- Keyboard-only completion of primary journeys

Qualitative validation should include users running at least five sessions across at least two projects. Single-session screenshots will not expose the main UX problem.

## Evidence Boundaries

- The current app was built successfully from the post-refresh source.
- A screenshot attempt captured an already-running older app, not a verified latest-build instance; it was excluded from evidence.
- Current visual hierarchy findings are source-derived and require latest-build validation.
- Source review covered the main shell, sidebar, session modals, Rich mode, composer, Scratch Pad, Changes, Browser, and Settings.
- Benchmark research used current official documentation from Linear, Raycast, Warp, Zed, Apple, and WCAG.
- Direct Linear API tooling was not exposed in this session. Current planning evidence came from persisted Linear setup records and GitHub-linked merged work; issue statuses beyond those records were not invented.
