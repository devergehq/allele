//! Named session groups — the pure decisions behind them (DEV-853).
//!
//! At ~36 live sessions a flat list per project stops answering "which of these
//! is waiting on me". A group is an **optional free-text name on a session**:
//! nothing is derived from who dispatched it, and there is no fixed list. A
//! group exists because at least one session carries its name.
//!
//! Everything here is pure. `src/sidebar/render.rs` draws what these functions
//! decide and makes no grouping decisions of its own, because a GPUI render path
//! needs a window and a listener cannot be called from a test at all — so logic
//! that lived there would be logic nothing could check.
//!
//! ## The two rules that are easy to get subtly wrong
//!
//! **Case-insensitive, first-spelling-wins.** `Review` typed after `review`
//! joins `review` and is *displayed* as `review`. The canonical spelling is
//! whichever the project saw first, held in [`GroupOrder`], and it is interned at
//! assignment time — so the common path stores one spelling and comparison never
//! has to run. Grouping still compares case-insensitively anyway
//! ([`group_blocks`]), because `state.json` can be hand-edited and a file allele
//! did not write is not bound by allele's interning.
//!
//! **A name keeps its place after its last session goes.** The order is a
//! per-project list of names, not a derived ordering of populated groups, so a
//! `review` group that empties and refills comes back where it was instead of
//! jumping to the end. That is the whole reason the order is stored separately
//! from the sessions.
//!
//! ## Slots, and why the ungrouped block is one (DEV-937)
//!
//! DEV-853 drew the ungrouped block first, unconditionally. DEV-937 has to be
//! able to put it below a group, so the order is no longer a list of *names*
//! but a list of **slots**: the named groups with one [`GroupSlot::Ungrouped`]
//! among them, held as `names` plus the index it sits at. One list means one
//! sort key for every block ([`GroupOrder::block_sort_key`]) and one move
//! operation ([`GroupOrder::move_slot`]), rather than a special case wherever
//! the ungrouped block is mentioned.
//!
//! The ungrouped block draws **no header**, so it has nothing to drag and no
//! menu to open. Its position is therefore moved *implicitly*, by moving named
//! groups past it — which reaches every arrangement, because a list of n slots
//! is fully determined by where its n-1 named ones are.

/// Longest permitted group name, in **characters**.
///
/// Characters, not bytes: `name.len() > 40` would refuse a legal 40-character
/// name the moment it contained anything outside ASCII.
///
/// This limit is part of the `group` parameter contract shared with DEV-938
/// (Locus) — it is also enforced on `allele_sessions_create`. Changing it here
/// changes that contract.
pub const GROUP_NAME_MAX_LEN: usize = 40;

/// A group name was longer than [`GROUP_NAME_MAX_LEN`].
///
/// Carries the offending length so every surface can say how far over the limit
/// the name was rather than only that it was over — the acceptance criterion
/// requires the message to name the limit, and a caller fixing a generated name
/// wants to know by how much.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GroupNameTooLong {
    pub chars: usize,
}

impl GroupNameTooLong {
    /// The refusal message, shared by both modals and the control socket so the
    /// three cannot describe the same limit differently.
    pub fn message(&self) -> String {
        format!(
            "group name is {} characters; the limit is {GROUP_NAME_MAX_LEN}",
            self.chars
        )
    }
}

/// Clean a typed or dispatched group name.
///
/// Trimmed; empty (or all whitespace) means **no group**, which is how the edit
/// modal removes a session from its group — clearing the field is the gesture,
/// so it cannot also be an error. Over-length is refused rather than truncated:
/// silently storing a different name than the caller asked for would split a
/// group the caller believed it had joined.
pub fn normalise(raw: &str) -> Result<Option<String>, GroupNameTooLong> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let chars = trimmed.chars().count();
    if chars > GROUP_NAME_MAX_LEN {
        return Err(GroupNameTooLong { chars });
    }
    Ok(Some(trimmed.to_string()))
}

/// Do two group names denote the same group?
///
/// `eq_ignore_ascii_case` would call `Review` and `review` equal but not `CAFÉ`
/// and `café`, which is an arbitrary line to draw in a free-text field.
pub fn same_group(a: &str, b: &str) -> bool {
    a.to_lowercase() == b.to_lowercase()
}

/// One slot in a project's sidebar order: a named group, or the block of
/// sessions that have no group.
///
/// The ungrouped block is a slot like any other so that ordering, and the move
/// that changes it, have one shape rather than two (DEV-937).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GroupSlot {
    /// The sessions with no group. Exactly one slot per order is this.
    Ungrouped,
    Named(String),
}

/// Which way a menu entry moves a group through the order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoveDirection {
    Up,
    Down,
}

/// Why a group rename was refused.
///
/// Refusals rather than best-effort fixes: every one of these would otherwise
/// silently produce a *different* group from the one the user asked for, and a
/// group is only identifiable by its name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RenameRefused {
    /// Over [`GROUP_NAME_MAX_LEN`] — the same limit the Group field enforces.
    TooLong(GroupNameTooLong),
    /// The new name was blank. Clearing a *session's* group is how a session
    /// leaves a group; a group with no name could not be drawn or referred to.
    Empty,
    /// Another group in this project already has that name, carrying the
    /// spelling it already uses. Accepting it would merge two groups — which is
    /// a destructive, un-undoable operation the user asked for a rename, not a
    /// merge, to get.
    Taken { by: String },
    /// The order has no such group. Reachable when a context menu outlives the
    /// group it names.
    Unknown,
}

impl RenameRefused {
    /// The refusal message shown beside the header's rename field.
    pub fn message(&self) -> String {
        match self {
            RenameRefused::TooLong(over) => over.message(),
            RenameRefused::Empty => "a group needs a name".to_string(),
            RenameRefused::Taken { by } => {
                format!("this project already has a group called {by}")
            }
            RenameRefused::Unknown => "that group is gone".to_string(),
        }
    }
}

/// A project's sidebar group state: the order its blocks are drawn in, where
/// the ungrouped block sits among them, and which groups are folded away.
///
/// Group *names* are still append-only — a name that loses its last session
/// stays in the list so the group reappears in its old position when it refills
/// (DEV-853). What DEV-937 adds is that the list can be permuted, a name can be
/// respelt in place, and the ungrouped block has a position in it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GroupOrder {
    names: Vec<String>,
    /// How many named groups are drawn *above* the ungrouped block.
    ///
    /// `0` — the default, and what every file written before DEV-937 restores
    /// to — is DEV-853's behaviour: ungrouped first. Always clamped to
    /// `names.len()` on the way in, so it can never name a slot that is not
    /// there.
    ungrouped_at: usize,
    /// Names whose rows are folded away. A free list rather than a flag per
    /// name, because it is also what gets persisted and a `Vec<String>` is what
    /// a person reading `state.json` can follow.
    collapsed: Vec<String>,
}

impl GroupOrder {
    /// An order of names alone — ungrouped first, nothing collapsed, which is
    /// DEV-853's behaviour and the shape most tests want.
    ///
    /// Test-only since DEV-937: the restore path has three things to rebuild and
    /// goes through [`Self::from_persisted`], so a second public constructor
    /// that silently defaults two of them is a way to lose a user's collapsed
    /// groups.
    #[cfg(test)]
    pub fn from_names(names: Vec<String>) -> Self {
        Self {
            names,
            ungrouped_at: 0,
            collapsed: Vec::new(),
        }
    }

    /// Rebuild an order from `state.json`.
    ///
    /// `ungrouped_at` is clamped rather than trusted: the file is hand-editable
    /// and a stored index past the end would otherwise put the ungrouped block
    /// in a slot that does not exist.
    pub fn from_persisted(names: Vec<String>, ungrouped_at: usize, collapsed: Vec<String>) -> Self {
        let ungrouped_at = ungrouped_at.min(names.len());
        Self {
            names,
            ungrouped_at,
            collapsed,
        }
    }

    pub fn names(&self) -> &[String] {
        &self.names
    }

    /// How many named groups sit above the ungrouped block — the persisted
    /// half of its position.
    pub fn ungrouped_at(&self) -> usize {
        self.ungrouped_at.min(self.names.len())
    }

    /// The folded groups, for persistence.
    pub fn collapsed_names(&self) -> &[String] {
        &self.collapsed
    }

    /// The spelling this project already uses for `name`, if it has seen it.
    pub fn canonical(&self, name: &str) -> Option<&str> {
        self.names
            .iter()
            .find(|known| same_group(known, name))
            .map(String::as_str)
    }

    /// Resolve `name` to this project's spelling of it, recording it as a new
    /// group at the end of the order if it is new.
    ///
    /// Returns the spelling to store on the session. Call this on every
    /// assignment — it is what makes "the first spelling wins" true rather than
    /// aspirational.
    pub fn intern(&mut self, name: &str) -> String {
        if let Some(known) = self.canonical(name) {
            return known.to_string();
        }
        self.names.push(name.to_string());
        name.to_string()
    }

    /// Where `name` sits among the **named** groups. `None` for a name the
    /// project has never seen. See [`Self::slot_of`] for its position among the
    /// drawn blocks, which differs once the ungrouped block is not first.
    pub fn position(&self, name: &str) -> Option<usize> {
        self.names.iter().position(|known| same_group(known, name))
    }

    // ── slots (DEV-937) ────────────────────────────────────────────────

    /// The order as the sidebar draws it: every named group with the ungrouped
    /// block in its place.
    ///
    /// Exactly one [`GroupSlot::Ungrouped`], always — it is the block that
    /// exists whether or not anything is in it, because "no group" is not a
    /// name that can be forgotten.
    pub fn slots(&self) -> Vec<GroupSlot> {
        let at = self.ungrouped_at();
        let named = |n: &String| GroupSlot::Named(n.clone());
        let mut slots = Vec::with_capacity(self.names.len() + 1);
        slots.extend(self.names[..at].iter().map(named));
        slots.push(GroupSlot::Ungrouped);
        slots.extend(self.names[at..].iter().map(named));
        slots
    }

    /// Adopt a permuted slot list.
    ///
    /// Private, and the only caller is [`Self::move_slot`] with the output of
    /// [`Self::slots`], which is what guarantees the one-ungrouped-slot
    /// invariant holds on the way back in.
    fn set_slots(&mut self, slots: Vec<GroupSlot>) {
        let mut names = Vec::with_capacity(slots.len());
        let mut ungrouped_at = None;
        for slot in slots {
            match slot {
                GroupSlot::Ungrouped => ungrouped_at = Some(names.len()),
                GroupSlot::Named(name) => names.push(name),
            }
        }
        self.ungrouped_at = ungrouped_at.unwrap_or(0).min(names.len());
        self.names = names;
    }

    /// Which drawn block `name` is — `None` meaning the ungrouped block.
    ///
    /// `None` returned for a *named* group the project has never seen; the
    /// ungrouped block always has a slot. Arithmetic rather than a scan of
    /// [`Self::slots`], because this is called once per block on every frame.
    pub fn slot_of(&self, name: Option<&str>) -> Option<usize> {
        let at = self.ungrouped_at();
        match name {
            None => Some(at),
            Some(name) => self.position(name).map(|i| if i < at { i } else { i + 1 }),
        }
    }

    /// Sort key for a drawn block, ungrouped included.
    ///
    /// `usize::MAX` for a name the order has never seen, so it sorts after
    /// every placed block; a stable sort then keeps several such names in the
    /// order they appeared. Unreachable through allele's own assignment path,
    /// which interns first — reachable through a hand-edited `state.json`.
    pub fn block_sort_key(&self, name: Option<&str>) -> usize {
        self.slot_of(name).unwrap_or(usize::MAX)
    }

    /// Move the slot at `from` to `to`, shifting the rest.
    ///
    /// Remove-and-insert, deliberately the same semantics as a session
    /// drag-reorder (`SessionAction::ReorderSession`): dropping A on B puts A
    /// where B was, which is the gesture the sidebar already teaches.
    ///
    /// Returns whether anything moved, so a caller can skip persisting a no-op.
    pub fn move_slot(&mut self, from: usize, to: usize) -> bool {
        let mut slots = self.slots();
        if from == to || from >= slots.len() || to >= slots.len() {
            return false;
        }
        let slot = slots.remove(from);
        slots.insert(to, slot);
        self.set_slots(slots);
        true
    }

    /// Move `name` one slot up or down — the header menu's "Move Up" / "Move
    /// Down".
    ///
    /// One slot, not one *group*: a group above the ungrouped block that moves
    /// down lands below it, which is how the ungrouped block is positioned
    /// without a header of its own to drag.
    ///
    /// Returns `false` at either end of the list, so the menu entry can be
    /// drawn as unavailable rather than doing nothing silently.
    pub fn move_group(&mut self, name: &str, direction: MoveDirection) -> bool {
        let Some(from) = self.slot_of(Some(name)) else {
            return false;
        };
        let to = match direction {
            MoveDirection::Up => from.checked_sub(1),
            // `from` indexes slots, of which there is one more than there are
            // names, so the last slot is `names.len()`.
            MoveDirection::Down => (from < self.names.len()).then_some(from + 1),
        };
        to.is_some_and(|to| self.move_slot(from, to))
    }

    // ── collapse (DEV-937) ─────────────────────────────────────────────

    /// Are this group's rows folded away?
    ///
    /// Case-insensitive like every other name comparison here, so a collapsed
    /// `review` stays collapsed if the file spells it `Review`.
    pub fn is_collapsed(&self, name: &str) -> bool {
        self.collapsed.iter().any(|c| same_group(c, name))
    }

    /// Fold a group away, or unfold it. Returns the new state.
    ///
    /// Takes any spelling and stores the project's canonical one when it knows
    /// it, so the collapsed list and the order cannot drift apart.
    pub fn toggle_collapsed(&mut self, name: &str) -> bool {
        if let Some(at) = self.collapsed.iter().position(|c| same_group(c, name)) {
            self.collapsed.remove(at);
            return false;
        }
        let canonical = self.canonical(name).unwrap_or(name).to_string();
        self.collapsed.push(canonical);
        true
    }

    // ── rename (DEV-937) ───────────────────────────────────────────────

    /// Rename a group in place, keeping its slot.
    ///
    /// Returns the stored spelling on success — the caller then rewrites every
    /// member session's `group` to it, which is the half of the rename this type
    /// cannot see.
    ///
    /// A respelling of the *same* group (`review` → `Review`) is a rename, not a
    /// collision: it is the only way to correct the canonical spelling a project
    /// was stuck with by whoever typed the name first.
    pub fn rename(&mut self, from: &str, to: &str) -> Result<String, RenameRefused> {
        let to = match normalise(to) {
            Ok(Some(name)) => name,
            Ok(None) => return Err(RenameRefused::Empty),
            Err(over) => return Err(RenameRefused::TooLong(over)),
        };
        let Some(at) = self.position(from) else {
            return Err(RenameRefused::Unknown);
        };
        if let Some(clash) = self
            .names
            .iter()
            .enumerate()
            .find(|(i, known)| *i != at && same_group(known, &to))
        {
            return Err(RenameRefused::Taken {
                by: clash.1.clone(),
            });
        }
        self.names[at] = to.clone();
        // The collapsed list stores names, so it has to follow the rename or the
        // group would quietly unfold.
        for entry in &mut self.collapsed {
            if same_group(entry, from) {
                *entry = to.clone();
            }
        }
        Ok(to)
    }
}

/// How many group names the Group field offers at once.
///
/// Enough to cover a working set of groups without the list pushing the rest of
/// the modal's fields off screen.
pub const GROUP_SUGGESTION_LIMIT: usize = 6;

/// The project's group names worth offering for what has been typed so far.
///
/// The field is free text — a new group is created by typing one — so this is a
/// convenience, not a constraint. Its job is the accident the issue names: that
/// `review` and `reviews` become two groups because nobody could see the first
/// one existed.
///
/// An exact match (ignoring case) offers nothing: the name is already typed, so
/// a row restating it is a row that does nothing. Everything else is a
/// case-insensitive substring match, which catches both the prefix a person
/// types and the `9586` they remember from the middle of a name.
pub fn suggestions<'a>(existing: &'a [String], typed: &str) -> Vec<&'a str> {
    let needle = typed.trim().to_lowercase();
    existing
        .iter()
        .map(String::as_str)
        .filter(|name| {
            if needle.is_empty() {
                return true;
            }
            let lower = name.to_lowercase();
            lower != needle && lower.contains(&needle)
        })
        .take(GROUP_SUGGESTION_LIMIT)
        .collect()
}

/// How many of a group's visible sessions are in each state worth surfacing.
///
/// Three counts rather than one total: "4 sessions" does not say whether to look
/// now, and the whole point of the group header is to answer that from the
/// collapsed view.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GroupCounts {
    /// Actively producing output.
    pub running: usize,
    /// Finished a turn and waiting to be read.
    pub finished: usize,
    /// Genuinely stuck until a human acts — see [`SessionAttention::Blocked`].
    pub blocked: usize,
}

impl GroupCounts {
    fn add(&mut self, attention: SessionAttention) {
        match attention {
            SessionAttention::Running => self.running += 1,
            SessionAttention::Finished => self.finished += 1,
            SessionAttention::Blocked => self.blocked += 1,
            SessionAttention::Other => {}
        }
    }

    /// The header's count text, e.g. `2 running · 1 finished · 1 blocked`.
    ///
    /// Zero counts are omitted rather than shown as `0 blocked`: a group with
    /// nothing blocked should not spend a word on the word "blocked". `None`
    /// when no session is in any of the three states — a group of suspended
    /// rows gets a bare header.
    pub fn summary(&self) -> Option<String> {
        let parts: Vec<String> = [
            (self.running, "running"),
            (self.finished, "finished"),
            (self.blocked, "blocked"),
        ]
        .iter()
        .filter(|(n, _)| *n > 0)
        .map(|(n, label)| format!("{n} {label}"))
        .collect();
        (!parts.is_empty()).then(|| parts.join(" · "))
    }
}

/// Which of the header's three counts a session contributes to.
///
/// Deliberately a separate vocabulary from [`crate::session::SessionStatus`].
/// The header asks "should I look at this group", and the answer collapses six
/// statuses into four buckets: `Suspended`, `Idle` and `Done` are `Other` and
/// are counted nowhere, because a header that said "32 finished" for a project
/// of suspended rows would be describing history rather than work.
///
/// `Blocked` follows DEV-788: an `AwaitingInput` session waiting on a permission
/// or a question is blocked; one that simply has nothing to do is not, and
/// counting it as blocked is what made "18 sessions waiting for your input"
/// impossible to triage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionAttention {
    Running,
    Finished,
    Blocked,
    Other,
}

/// One run of consecutive session rows the sidebar draws together.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupBlock {
    /// The group's display name, in the project's first-seen spelling. `None`
    /// for the ungrouped block, which is drawn **without a header** — the issue
    /// is explicit that an ungrouped session looks exactly as every session did
    /// before this change.
    pub name: Option<String>,
    /// Indices into the project's `sessions`, in the order to draw them.
    ///
    /// Original indices, never display positions: selection, drag-and-drop and
    /// the discard confirmation all key on a session's index in
    /// `project.sessions`, so grouping must partition that list and never
    /// renumber it.
    pub sessions: Vec<usize>,
    /// Counts over the group's visible rows.
    ///
    /// Note "visible", not `sessions`: the two are the same until
    /// [`collapse_blocks`] folds the rows away, and the counts deliberately
    /// keep describing the whole visible group after it does — a collapsed
    /// header exists to say what is inside it.
    pub counts: GroupCounts,
    /// Are this group's rows folded away? Set by [`collapse_blocks`]; always
    /// `false` for the ungrouped block, which has no header to unfold from.
    pub collapsed: bool,
}

/// A group header's content: its name, the counts beside it, and whether it is
/// folded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupHeader {
    pub name: String,
    pub counts: GroupCounts,
    /// Drives the disclosure chevron's direction, and nothing else — the rows
    /// are already gone by the time a header is built.
    pub collapsed: bool,
}

/// Which of the header's three counts a session in this state contributes to.
///
/// Takes the two values the decision actually rests on rather than a `Session`,
/// so it can be checked without standing up a GPUI window.
///
/// `Blocked` follows DEV-788: an `AwaitingInput` with no attention context is
/// treated as blocking, which is the same safer default the sidebar's sort key
/// already uses — a session that might be stuck is better counted as stuck than
/// quietly dropped from the number a person triages by.
pub fn attention_of(
    status: crate::session::SessionStatus,
    kind: Option<crate::session::AttentionKind>,
) -> SessionAttention {
    use crate::session::SessionStatus;
    match status {
        SessionStatus::Running => SessionAttention::Running,
        SessionStatus::ResponseReady => SessionAttention::Finished,
        SessionStatus::AwaitingInput => match kind {
            Some(kind) if kind.is_blocking() => SessionAttention::Blocked,
            None => SessionAttention::Blocked,
            // `AttentionKind::Idle` — nothing to do, so nothing is blocked.
            Some(_) => SessionAttention::Other,
        },
        SessionStatus::Idle | SessionStatus::Suspended | SessionStatus::Done => {
            SessionAttention::Other
        }
    }
}

/// Flatten blocks into the sequence the sidebar draws: one entry per row,
/// carrying the header that must be emitted immediately before it.
///
/// Flattened rather than handed over as nested blocks so the existing row loop
/// in `render.rs` keeps its shape — it stays one loop over session indices, and
/// the only new lines in it emit a header and skip a header-only row. A nested
/// loop would have re-indented four hundred lines of unrelated rendering, which
/// is the "aggressive refactor in a feature PR" CONTRIBUTING.md asks people not
/// to do.
///
/// The session index is `None` for a **header-only** row. A collapsed group can
/// fold every row away, and its header is the whole point of having collapsed
/// it, so the header cannot be something that only exists to be hung on a row
/// (DEV-937).
pub fn draw_order(blocks: Vec<GroupBlock>) -> Vec<(Option<GroupHeader>, Option<usize>)> {
    let mut rows = Vec::new();
    for block in blocks {
        let collapsed = block.collapsed;
        let counts = block.counts;
        let mut header = block.name.map(|name| GroupHeader {
            name,
            counts,
            collapsed,
        });
        if block.sessions.is_empty() {
            // Only reachable after `collapse_blocks` — `group_blocks` never
            // emits an empty block, so a group with nothing visible still draws
            // nothing at all.
            rows.extend(header.take().map(|h| (Some(h), None)));
            continue;
        }
        for idx in block.sessions {
            // `take` so the header lands on the group's first row only.
            rows.push((header.take(), Some(idx)));
        }
    }
    rows
}

/// Fold the rows of every collapsed group away, keeping its header.
///
/// A separate pass over [`group_blocks`]'s output rather than a parameter to it,
/// for one reason worth stating: the counts have to be taken over everything
/// visible in the group *before* its rows are folded. Computing them after would
/// make a collapsed header read `0 running` — a count disagreeing with what the
/// group holds, which is exactly DEV-789's defect in a new place.
///
/// `active` is the index of the selected session, if it is in this project. A
/// collapsed group still draws that row: the sidebar is how you know where you
/// are, and folding the session you are looking at out of view would leave the
/// app with no visible cursor.
pub fn collapse_blocks(
    blocks: Vec<GroupBlock>,
    group_order: &GroupOrder,
    active: Option<usize>,
) -> Vec<GroupBlock> {
    blocks
        .into_iter()
        .map(|mut block| {
            // The ungrouped block draws no header, so it has no affordance to
            // collapse and nothing to unfold it from.
            let Some(name) = block.name.as_deref() else {
                return block;
            };
            if !group_order.is_collapsed(name) {
                return block;
            }
            block.collapsed = true;
            block.sessions.retain(|idx| Some(*idx) == active);
            block
        })
        .collect()
}

/// Partition an already-sorted, already-filtered session order into the blocks
/// the sidebar draws, in draw order.
///
/// `order` is the output of the existing attention/pinned/manual sort. Taking it
/// pre-sorted is what makes "attention and pinning order sessions inside their
/// group, not across the project" fall out for free: a stable partition of a
/// sorted list leaves each block sorted by the same key, and no session can be
/// hoisted out of its group by being blocked. It also means this function never
/// needs to know what the sort key is.
///
/// `order` must already exclude rows hidden by the active-only toggle or the
/// text filter. Blocks are built from what is left, so `counts` describes the
/// visible rows and a group whose every row was filtered out produces no block
/// at all — hence no header. Counting a group's *whole* membership here would
/// re-create DEV-789, where a count and the rows beside it disagreed.
///
/// A session whose group the project's order has never seen sorts after every
/// known group, in first-appearance order. That is unreachable through allele's
/// own assignment path, which interns before storing; it is reachable through a
/// hand-edited `state.json`, and the honest answer there is to show the group
/// rather than to drop the session.
///
/// Every block is placed by [`GroupOrder::block_sort_key`], the ungrouped one
/// included — it is a slot in the order, not a special case pinned above the
/// groups (DEV-937). With a default order that key puts it first, which is
/// DEV-853's behaviour unchanged.
///
/// Collapsing is **not** applied here; see [`collapse_blocks`] for why it is a
/// later pass.
pub fn group_blocks(
    order: &[usize],
    group_of: impl Fn(usize) -> Option<String>,
    attention_of: impl Fn(usize) -> SessionAttention,
    group_order: &GroupOrder,
) -> Vec<GroupBlock> {
    let mut ungrouped = GroupBlock {
        name: None,
        sessions: Vec::new(),
        counts: GroupCounts::default(),
        collapsed: false,
    };
    // Keyed by the *canonical* name so a hand-edited file's `Review` and
    // `review` still land in one block.
    let mut blocks: Vec<GroupBlock> = Vec::new();

    for &idx in order {
        let attention = attention_of(idx);
        let Some(raw) = group_of(idx) else {
            ungrouped.sessions.push(idx);
            ungrouped.counts.add(attention);
            continue;
        };
        let display = group_order
            .canonical(&raw)
            .map(str::to_string)
            .unwrap_or(raw);
        let block = match blocks
            .iter_mut()
            .position(|b| b.name.as_deref().is_some_and(|n| same_group(n, &display)))
        {
            Some(at) => &mut blocks[at],
            None => {
                blocks.push(GroupBlock {
                    name: Some(display),
                    sessions: Vec::new(),
                    counts: GroupCounts::default(),
                    collapsed: false,
                });
                blocks.last_mut().expect("just pushed")
            }
        };
        block.sessions.push(idx);
        block.counts.add(attention);
    }

    // The ungrouped block joins the same sort rather than being prepended, so
    // its slot in the order is honoured (DEV-937). Empty means the project has
    // no ungrouped session to show: no block, so no gap where one would be.
    if !ungrouped.sessions.is_empty() {
        blocks.push(ungrouped);
    }
    // A stable sort, which is what keeps several never-interned names — all
    // keyed `usize::MAX` — in the order they appeared.
    blocks.sort_by_key(|b| group_order.block_sort_key(b.name.as_deref()));
    blocks
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── normalise ──────────────────────────────────────────────────────

    #[test]
    fn a_group_name_is_trimmed() {
        assert_eq!(normalise("  review  "), Ok(Some("review".to_string())));
    }

    #[test]
    fn a_whitespace_only_name_is_no_group() {
        // Clearing the edit modal's field is how a session leaves its group, so
        // this has to be "no group", never an error.
        assert_eq!(normalise("   "), Ok(None));
        assert_eq!(normalise(""), Ok(None));
        assert_eq!(normalise("\t\n"), Ok(None));
    }

    #[test]
    fn a_name_at_the_limit_is_accepted() {
        let at_limit = "r".repeat(GROUP_NAME_MAX_LEN);
        assert_eq!(normalise(&at_limit), Ok(Some(at_limit.clone())));
    }

    #[test]
    fn a_name_over_the_limit_is_refused_with_its_length() {
        let over = "r".repeat(GROUP_NAME_MAX_LEN + 1);
        assert_eq!(normalise(&over), Err(GroupNameTooLong { chars: 41 }));
    }

    /// The limit is in characters. A byte check would refuse this 40-character
    /// name because each `é` is two bytes — a legal name rejected for being
    /// written in the wrong alphabet.
    #[test]
    fn the_limit_counts_characters_not_bytes() {
        let accented = "é".repeat(GROUP_NAME_MAX_LEN);
        assert!(accented.len() > GROUP_NAME_MAX_LEN, "it is over in bytes");
        assert_eq!(normalise(&accented), Ok(Some(accented.clone())));
    }

    /// Trimming happens before measuring, so padding cannot push a legal name
    /// over the limit.
    #[test]
    fn surrounding_space_does_not_count_against_the_limit() {
        let padded = format!("  {}  ", "r".repeat(GROUP_NAME_MAX_LEN));
        assert!(normalise(&padded).is_ok());
    }

    #[test]
    fn the_refusal_message_names_the_limit() {
        let message = GroupNameTooLong { chars: 57 }.message();
        assert!(message.contains("40"), "{message}");
        assert!(message.contains("57"), "{message}");
    }

    // ── case-insensitive matching ──────────────────────────────────────

    #[test]
    fn names_differing_only_in_case_are_one_group() {
        assert!(same_group("review", "Review"));
        assert!(same_group("REVIEW", "review"));
        assert!(!same_group("review", "reviews"));
    }

    /// `eq_ignore_ascii_case` would say these differ. A free-text field has no
    /// reason to treat non-ASCII case as significant when it ignores ASCII case.
    #[test]
    fn case_folding_is_not_ascii_only() {
        assert!(same_group("CAFÉ", "café"));
    }

    // ── GroupOrder ─────────────────────────────────────────────────────

    #[test]
    fn interning_a_new_name_appends_it() {
        let mut order = GroupOrder::default();
        assert_eq!(order.intern("dispatcher"), "dispatcher");
        assert_eq!(order.intern("review"), "review");
        assert_eq!(order.names(), ["dispatcher", "review"]);
    }

    /// The acceptance criterion: `review`, `Review` and `review` are one group,
    /// shown in the spelling first used.
    #[test]
    fn interning_returns_the_first_spelling_the_project_saw() {
        let mut order = GroupOrder::default();
        assert_eq!(order.intern("review"), "review");
        assert_eq!(order.intern("Review"), "review");
        assert_eq!(order.intern("REVIEW"), "review");
        assert_eq!(order.names(), ["review"], "one group, not three");
    }

    #[test]
    fn position_is_case_insensitive() {
        let order = GroupOrder::from_names(vec!["dispatcher".into(), "review".into()]);
        assert_eq!(order.position("REVIEW"), Some(1));
        assert_eq!(order.position("linear"), None);
    }

    // ── slots, and the ungrouped block's place among them (DEV-937) ────

    /// Names and slots differ as soon as the ungrouped block is not first, and
    /// mixing them up is the bug this helper exists to make visible.
    fn slot_names(order: &GroupOrder) -> Vec<String> {
        order
            .slots()
            .into_iter()
            .map(|slot| match slot {
                GroupSlot::Ungrouped => "·".to_string(),
                GroupSlot::Named(name) => name,
            })
            .collect()
    }

    /// The default is DEV-853's behaviour: ungrouped first.
    #[test]
    fn the_ungrouped_block_starts_above_every_group() {
        let order = GroupOrder::from_names(vec!["review".into(), "linear".into()]);
        assert_eq!(slot_names(&order), ["·", "review", "linear"]);
        assert_eq!(order.slot_of(None), Some(0));
        assert_eq!(order.slot_of(Some("review")), Some(1));
    }

    /// A project with no groups at all is one slot: the ungrouped block.
    #[test]
    fn an_order_with_no_groups_is_one_slot() {
        assert_eq!(slot_names(&GroupOrder::default()), ["·"]);
    }

    #[test]
    fn a_slot_moves_to_where_it_was_dropped() {
        let mut order = GroupOrder::from_names(vec!["a".into(), "b".into(), "c".into()]);
        // Drop `c` (slot 3) on `a` (slot 1).
        assert!(order.move_slot(3, 1));
        assert_eq!(slot_names(&order), ["·", "c", "a", "b"]);
    }

    /// The acceptance criterion: the ungrouped block can sit above, between or
    /// below the groups. It has no header to drag, so it is placed by moving
    /// groups past it — and that reaches every arrangement.
    #[test]
    fn the_ungrouped_block_can_be_placed_anywhere_by_moving_groups() {
        let mut order = GroupOrder::from_names(vec!["a".into(), "b".into()]);

        // Between: move `a` above the ungrouped block.
        assert!(order.move_group("a", MoveDirection::Up));
        assert_eq!(slot_names(&order), ["a", "·", "b"]);
        assert_eq!(order.ungrouped_at(), 1);

        // Below everything: move `b` up past it too.
        assert!(order.move_group("b", MoveDirection::Up));
        assert_eq!(slot_names(&order), ["a", "b", "·"]);
        assert_eq!(order.ungrouped_at(), 2);
    }

    #[test]
    fn moving_a_group_down_moves_it_one_slot() {
        let mut order = GroupOrder::from_names(vec!["a".into(), "b".into()]);
        assert!(order.move_group("a", MoveDirection::Down));
        assert_eq!(slot_names(&order), ["·", "b", "a"]);
    }

    /// At either end there is nothing to swap with, and the caller is told so
    /// rather than left to persist a write that changed nothing.
    #[test]
    fn moving_past_either_end_of_the_order_does_nothing() {
        let mut order = GroupOrder::from_names(vec!["a".into()]);
        // `·`, `a` — the ungrouped block is above, so `a` can go up but not down.
        assert!(!order.move_group("a", MoveDirection::Down));
        assert!(order.move_group("a", MoveDirection::Up));
        assert_eq!(slot_names(&order), ["a", "·"]);
        assert!(!order.move_group("a", MoveDirection::Up));
    }

    #[test]
    fn moving_a_name_the_order_does_not_have_does_nothing() {
        let mut order = GroupOrder::from_names(vec!["a".into()]);
        assert!(!order.move_group("mystery", MoveDirection::Up));
        assert_eq!(slot_names(&order), ["·", "a"]);
    }

    /// The acceptance criterion's persistence half, in memory: a group given a
    /// position holds it when it empties and refills. Reordering does not make
    /// the order derived, so nothing is recomputed from the live sessions.
    #[test]
    fn a_reordered_group_keeps_its_slot_when_it_empties() {
        let mut order = GroupOrder::from_names(vec!["review".into(), "linear".into()]);
        assert!(order.move_group("linear", MoveDirection::Up));
        assert_eq!(slot_names(&order), ["·", "linear", "review"]);

        // `linear` empties — no session carries it — and interning it again on
        // a later session must not move it to the end.
        assert_eq!(order.intern("linear"), "linear");
        assert_eq!(slot_names(&order), ["·", "linear", "review"]);
    }

    /// Interning a genuinely new name appends it, which after a reorder means
    /// after the last *named* group — not after the ungrouped block.
    #[test]
    fn a_new_name_appends_after_the_reordered_groups() {
        let mut order = GroupOrder::from_names(vec!["a".into()]);
        assert!(order.move_group("a", MoveDirection::Up));
        order.intern("b");
        assert_eq!(slot_names(&order), ["a", "·", "b"]);
    }

    /// `state.json` is hand-editable, so a stored slot can be nonsense. Clamped
    /// rather than trusted: an index past the end would otherwise put the
    /// ungrouped block in a slot that does not exist.
    #[test]
    fn a_stored_slot_past_the_end_is_clamped() {
        let order = GroupOrder::from_persisted(vec!["a".into()], 99, Vec::new());
        assert_eq!(order.ungrouped_at(), 1);
        assert_eq!(slot_names(&order), ["a", "·"]);
    }

    // ── collapse (DEV-937) ─────────────────────────────────────────────

    #[test]
    fn a_group_starts_expanded_and_toggles() {
        let mut order = GroupOrder::from_names(vec!["review".into()]);
        assert!(!order.is_collapsed("review"));
        assert!(order.toggle_collapsed("review"));
        assert!(order.is_collapsed("review"));
        assert!(!order.toggle_collapsed("review"));
        assert!(!order.is_collapsed("review"));
    }

    /// Case-insensitive like every other name comparison here, and the stored
    /// entry is the project's own spelling — so a collapsed `review` stays
    /// collapsed however the caller spells it.
    #[test]
    fn collapsing_is_case_insensitive_and_stores_the_canonical_name() {
        let mut order = GroupOrder::from_names(vec!["review".into()]);
        order.toggle_collapsed("REVIEW");
        assert_eq!(order.collapsed_names(), ["review"]);
        assert!(order.is_collapsed("Review"));
    }

    /// The acceptance criterion: a collapsed group keeps its header and its
    /// counts, and shows no rows.
    #[test]
    fn a_collapsed_group_keeps_its_counts_and_loses_its_rows() {
        let mut order = GroupOrder::from_names(vec!["review".into()]);
        order.toggle_collapsed("review");
        let counts = GroupCounts {
            running: 2,
            finished: 1,
            blocked: 0,
        };
        let blocks = collapse_blocks(
            vec![block(Some("review"), vec![0, 1, 2], counts)],
            &order,
            None,
        );

        assert!(blocks[0].collapsed);
        assert!(blocks[0].sessions.is_empty(), "no rows");
        assert_eq!(
            blocks[0].counts, counts,
            "and the counts still describe what is inside it"
        );
    }

    /// The acceptance criterion: the active session's row survives its group
    /// being folded. The sidebar is how you know where you are, and folding the
    /// session you are looking at out of view leaves no visible cursor.
    #[test]
    fn a_collapsed_group_still_shows_the_active_session() {
        let mut order = GroupOrder::from_names(vec!["review".into()]);
        order.toggle_collapsed("review");
        let blocks = collapse_blocks(
            vec![block(Some("review"), vec![0, 1, 2], GroupCounts::default())],
            &order,
            Some(1),
        );

        assert_eq!(blocks[0].sessions, [1], "that one row, and no other");
        assert!(blocks[0].collapsed, "and the header still reads as folded");
    }

    /// An active session in a *different* group does not keep a row open in
    /// this one.
    #[test]
    fn a_collapsed_group_ignores_an_active_session_elsewhere() {
        let mut order = GroupOrder::from_names(vec!["review".into()]);
        order.toggle_collapsed("review");
        let blocks = collapse_blocks(
            vec![block(Some("review"), vec![0, 1], GroupCounts::default())],
            &order,
            Some(7),
        );
        assert!(blocks[0].sessions.is_empty());
    }

    /// The ungrouped block draws no header, so there would be nothing left to
    /// unfold it from. It is never collapsed.
    #[test]
    fn the_ungrouped_block_is_never_collapsed() {
        let order = GroupOrder::from_persisted(Vec::new(), 0, vec!["review".into()]);
        let blocks = collapse_blocks(
            vec![block(None, vec![0, 1], GroupCounts::default())],
            &order,
            None,
        );
        assert_eq!(blocks[0].sessions, [0, 1]);
        assert!(!blocks[0].collapsed);
    }

    #[test]
    fn an_expanded_group_is_left_alone() {
        let order = GroupOrder::from_names(vec!["review".into()]);
        let blocks = collapse_blocks(
            vec![block(Some("review"), vec![0, 1], GroupCounts::default())],
            &order,
            None,
        );
        assert_eq!(blocks[0].sessions, [0, 1]);
        assert!(!blocks[0].collapsed);
    }

    // ── rename (DEV-937) ───────────────────────────────────────────────

    #[test]
    fn a_rename_keeps_the_groups_slot() {
        let mut order = GroupOrder::from_names(vec!["a".into(), "review".into(), "c".into()]);
        assert_eq!(
            order.rename("review", "code review"),
            Ok("code review".into())
        );
        assert_eq!(slot_names(&order), ["·", "a", "code review", "c"]);
    }

    #[test]
    fn a_rename_is_trimmed() {
        let mut order = GroupOrder::from_names(vec!["review".into()]);
        assert_eq!(order.rename("review", "  linear  "), Ok("linear".into()));
        assert_eq!(order.names(), ["linear"]);
    }

    /// Accepting this would merge two groups — which a rename is not, and which
    /// nothing here could undo.
    #[test]
    fn a_rename_onto_another_group_is_refused() {
        let mut order = GroupOrder::from_names(vec!["review".into(), "linear".into()]);
        assert_eq!(
            order.rename("review", "LINEAR"),
            Err(RenameRefused::Taken {
                by: "linear".into()
            })
        );
        assert_eq!(order.names(), ["review", "linear"], "unchanged");
    }

    /// Respelling the *same* group is a rename, not a collision: it is the only
    /// way to correct the canonical spelling a project was stuck with by
    /// whoever typed the name first.
    #[test]
    fn a_rename_may_change_only_the_case() {
        let mut order = GroupOrder::from_names(vec!["review".into()]);
        assert_eq!(order.rename("review", "Review"), Ok("Review".into()));
        assert_eq!(order.names(), ["Review"]);
    }

    #[test]
    fn a_rename_to_an_over_length_name_is_refused() {
        let mut order = GroupOrder::from_names(vec!["review".into()]);
        let over = "r".repeat(GROUP_NAME_MAX_LEN + 1);
        assert_eq!(
            order.rename("review", &over),
            Err(RenameRefused::TooLong(GroupNameTooLong { chars: 41 }))
        );
        assert!(order
            .rename("review", &over)
            .is_err_and(|e| e.message().contains("40")));
    }

    /// Clearing a *session's* group is how a session leaves a group. A group
    /// with no name could not be drawn or referred to.
    #[test]
    fn a_rename_to_nothing_is_refused() {
        let mut order = GroupOrder::from_names(vec!["review".into()]);
        assert_eq!(order.rename("review", "   "), Err(RenameRefused::Empty));
    }

    #[test]
    fn renaming_a_group_the_order_does_not_have_is_refused() {
        let mut order = GroupOrder::from_names(vec!["review".into()]);
        assert_eq!(order.rename("gone", "linear"), Err(RenameRefused::Unknown));
    }

    /// The collapsed list stores names, so it has to follow a rename — or the
    /// group would quietly unfold the moment it was renamed.
    #[test]
    fn a_rename_carries_the_collapsed_flag_with_it() {
        let mut order = GroupOrder::from_names(vec!["review".into()]);
        order.toggle_collapsed("review");
        assert_eq!(
            order.rename("review", "code review"),
            Ok("code review".into())
        );
        assert!(order.is_collapsed("code review"));
        assert_eq!(order.collapsed_names(), ["code review"]);
    }

    // ── suggestions ────────────────────────────────────────────────────

    #[test]
    fn an_empty_field_offers_every_group() {
        let existing = vec!["dispatcher".to_string(), "review".to_string()];
        assert_eq!(suggestions(&existing, ""), ["dispatcher", "review"]);
        assert_eq!(suggestions(&existing, "   "), ["dispatcher", "review"]);
    }

    /// The accident the field exists to prevent: typing `review` should surface
    /// the existing `Review` so a second group is not created by accident.
    #[test]
    fn typing_offers_a_differently_cased_existing_group() {
        let existing = vec!["Review".to_string()];
        assert_eq!(suggestions(&existing, "rev"), ["Review"]);
    }

    #[test]
    fn a_substring_anywhere_in_the_name_matches() {
        let existing = vec!["review 9586".to_string(), "linear".to_string()];
        assert_eq!(suggestions(&existing, "9586"), ["review 9586"]);
    }

    /// Having typed the name exactly, there is nothing left to suggest — a row
    /// restating what is in the field is a row that does nothing.
    #[test]
    fn an_exact_match_offers_nothing() {
        let existing = vec!["review".to_string()];
        assert!(suggestions(&existing, "review").is_empty());
        assert!(suggestions(&existing, "REVIEW").is_empty());
    }

    #[test]
    fn a_name_the_project_does_not_have_offers_nothing() {
        let existing = vec!["review".to_string()];
        assert!(suggestions(&existing, "linear").is_empty());
    }

    #[test]
    fn the_suggestion_list_is_capped() {
        let existing: Vec<String> = (0..20).map(|n| format!("group-{n}")).collect();
        assert_eq!(suggestions(&existing, "").len(), GROUP_SUGGESTION_LIMIT);
    }

    // ── counts ─────────────────────────────────────────────────────────

    #[test]
    fn the_header_summary_lists_the_three_states() {
        let counts = GroupCounts {
            running: 2,
            finished: 1,
            blocked: 1,
        };
        assert_eq!(
            counts.summary().as_deref(),
            Some("2 running · 1 finished · 1 blocked")
        );
    }

    #[test]
    fn zero_counts_are_omitted_from_the_summary() {
        let counts = GroupCounts {
            running: 3,
            ..Default::default()
        };
        assert_eq!(counts.summary().as_deref(), Some("3 running"));
    }

    /// A group of suspended rows gets a bare header rather than `0 running`.
    #[test]
    fn a_group_with_nothing_notable_has_no_summary() {
        assert_eq!(GroupCounts::default().summary(), None);
    }

    // ── group_blocks ───────────────────────────────────────────────────

    /// An expanded block, for the `draw_order` and `collapse_blocks` tests.
    fn block(name: Option<&str>, sessions: Vec<usize>, counts: GroupCounts) -> GroupBlock {
        GroupBlock {
            name: name.map(str::to_string),
            sessions,
            counts,
            collapsed: false,
        }
    }

    /// Session fixtures as `(group, attention)`, indexed by position.
    fn blocks_of(
        order: &[usize],
        sessions: &[(Option<&str>, SessionAttention)],
        group_order: &GroupOrder,
    ) -> Vec<GroupBlock> {
        group_blocks(
            order,
            |i| sessions[i].0.map(str::to_string),
            |i| sessions[i].1,
            group_order,
        )
    }

    #[test]
    fn ungrouped_sessions_come_first_and_have_no_header() {
        let sessions = [
            (Some("review"), SessionAttention::Running),
            (None, SessionAttention::Running),
            (Some("review"), SessionAttention::Running),
            (None, SessionAttention::Running),
        ];
        let order = GroupOrder::from_names(vec!["review".into()]);
        let blocks = blocks_of(&[0, 1, 2, 3], &sessions, &order);

        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].name, None, "the ungrouped block has no header");
        assert_eq!(blocks[0].sessions, [1, 3]);
        assert_eq!(blocks[1].name.as_deref(), Some("review"));
        assert_eq!(blocks[1].sessions, [0, 2]);
    }

    #[test]
    fn groups_follow_the_projects_recorded_order() {
        // Deliberately the reverse of the order the sessions appear in, so a
        // pass that grouped by first appearance would fail here.
        let sessions = [
            (Some("linear"), SessionAttention::Running),
            (Some("review"), SessionAttention::Running),
            (Some("dispatcher"), SessionAttention::Running),
        ];
        let order =
            GroupOrder::from_names(vec!["dispatcher".into(), "review".into(), "linear".into()]);
        let blocks = blocks_of(&[0, 1, 2], &sessions, &order);

        let names: Vec<&str> = blocks.iter().filter_map(|b| b.name.as_deref()).collect();
        assert_eq!(names, ["dispatcher", "review", "linear"]);
    }

    /// The acceptance criterion, in the draw pipeline: the ungrouped block sits
    /// where the order puts it rather than always first.
    #[test]
    fn the_ungrouped_block_is_drawn_in_its_slot() {
        let sessions = [
            (None, SessionAttention::Running),
            (Some("review"), SessionAttention::Running),
            (Some("linear"), SessionAttention::Running),
        ];
        let mut order = GroupOrder::from_names(vec!["review".into(), "linear".into()]);
        // `review`, `·`, `linear`.
        assert!(order.move_group("review", MoveDirection::Up));

        let blocks = blocks_of(&[0, 1, 2], &sessions, &order);
        let names: Vec<Option<&str>> = blocks.iter().map(|b| b.name.as_deref()).collect();
        assert_eq!(names, [Some("review"), None, Some("linear")]);
    }

    /// And below every group, when that is where it was put.
    #[test]
    fn the_ungrouped_block_can_be_drawn_last() {
        let sessions = [
            (None, SessionAttention::Running),
            (Some("review"), SessionAttention::Running),
        ];
        let order = GroupOrder::from_persisted(vec!["review".into()], 1, Vec::new());

        let blocks = blocks_of(&[0, 1], &sessions, &order);
        let names: Vec<Option<&str>> = blocks.iter().map(|b| b.name.as_deref()).collect();
        assert_eq!(names, [Some("review"), None]);
    }

    /// A group whose rows were all filtered out produces no block, so the
    /// sidebar has no header to draw.
    #[test]
    fn a_group_with_no_visible_session_produces_no_block() {
        let sessions = [
            (Some("review"), SessionAttention::Running),
            (Some("linear"), SessionAttention::Running),
        ];
        let order = GroupOrder::from_names(vec!["review".into(), "linear".into()]);
        // `linear`'s only session is absent from the order — filtered out.
        let blocks = blocks_of(&[0], &sessions, &order);

        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].name.as_deref(), Some("review"));
    }

    /// An empty project draws nothing at all — not an empty ungrouped block.
    #[test]
    fn no_visible_session_produces_no_blocks() {
        let order = GroupOrder::from_names(vec!["review".into()]);
        assert!(blocks_of(&[], &[], &order).is_empty());
    }

    /// A project where nobody has used a group shows one header-less block, so
    /// the sidebar looks exactly as it did before DEV-853.
    #[test]
    fn a_project_with_no_groups_has_one_headerless_block() {
        let sessions = [
            (None, SessionAttention::Running),
            (None, SessionAttention::Running),
        ];
        let blocks = blocks_of(&[0, 1], &sessions, &GroupOrder::default());

        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].name, None);
        assert_eq!(blocks[0].sessions, [0, 1]);
    }

    /// The order handed in is already sorted by attention/pinned/manual, so a
    /// blocked session arrives at the front and must stay at the front **of its
    /// own block** — not be hoisted above the groups above it.
    #[test]
    fn a_blocked_session_rises_within_its_group_and_does_not_leave_it() {
        let sessions = [
            (Some("dispatcher"), SessionAttention::Running), // 0
            (Some("review"), SessionAttention::Blocked),     // 1
            (Some("review"), SessionAttention::Running),     // 2
        ];
        let order = GroupOrder::from_names(vec!["dispatcher".into(), "review".into()]);
        // Blocked-first, as the sidebar's sort key would deliver it.
        let blocks = blocks_of(&[1, 0, 2], &sessions, &order);

        assert_eq!(blocks[0].name.as_deref(), Some("dispatcher"));
        assert_eq!(blocks[1].name.as_deref(), Some("review"));
        assert_eq!(
            blocks[1].sessions,
            [1, 2],
            "blocked first inside review, and still inside review"
        );
    }

    /// Same contract for pinning: it orders within the group, never across the
    /// project.
    #[test]
    fn a_pinned_session_stays_inside_its_group() {
        let sessions = [
            (None, SessionAttention::Other),           // 0
            (Some("review"), SessionAttention::Other), // 1, pinned
            (Some("review"), SessionAttention::Other), // 2
        ];
        let order = GroupOrder::from_names(vec!["review".into()]);
        // The pinned session sorts to the top of the project-wide order…
        let blocks = blocks_of(&[1, 0, 2], &sessions, &order);

        // …and still lands in its group, at the top of it.
        assert_eq!(blocks[0].name, None);
        assert_eq!(blocks[0].sessions, [0]);
        assert_eq!(blocks[1].sessions, [1, 2]);
    }

    #[test]
    fn counts_describe_only_the_visible_rows() {
        let sessions = [
            (Some("review"), SessionAttention::Running),
            (Some("review"), SessionAttention::Blocked),
            (Some("review"), SessionAttention::Finished),
            (Some("review"), SessionAttention::Other),
            // Hidden by the filter, and so must not be counted.
            (Some("review"), SessionAttention::Running),
        ];
        let order = GroupOrder::from_names(vec!["review".into()]);
        let blocks = blocks_of(&[0, 1, 2, 3], &sessions, &order);

        assert_eq!(
            blocks[0].counts,
            GroupCounts {
                running: 1,
                finished: 1,
                blocked: 1,
            },
            "the fourth row is in none of the three states, the fifth is hidden"
        );
    }

    /// `state.json` is hand-editable and sync does not carry groups, so allele
    /// can be handed spellings it never interned. One group, displayed in the
    /// project's spelling.
    #[test]
    fn mixed_case_groups_from_an_unmanaged_file_still_merge() {
        let sessions = [
            (Some("review"), SessionAttention::Running),
            (Some("Review"), SessionAttention::Running),
            (Some("REVIEW"), SessionAttention::Running),
        ];
        let order = GroupOrder::from_names(vec!["review".into()]);
        let blocks = blocks_of(&[0, 1, 2], &sessions, &order);

        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].name.as_deref(), Some("review"));
        assert_eq!(blocks[0].sessions, [0, 1, 2]);
    }

    /// A group the order has never seen is shown rather than dropped, after
    /// every group it has.
    #[test]
    fn an_unrecorded_group_sorts_last_rather_than_vanishing() {
        let sessions = [
            (Some("mystery"), SessionAttention::Running),
            (Some("review"), SessionAttention::Running),
        ];
        let order = GroupOrder::from_names(vec!["review".into()]);
        let blocks = blocks_of(&[0, 1], &sessions, &order);

        let names: Vec<&str> = blocks.iter().filter_map(|b| b.name.as_deref()).collect();
        assert_eq!(names, ["review", "mystery"]);
    }

    // ── attention_of ───────────────────────────────────────────────────

    #[test]
    fn statuses_map_to_the_header_counts() {
        use crate::session::{AttentionKind, SessionStatus};
        assert_eq!(
            attention_of(SessionStatus::Running, None),
            SessionAttention::Running
        );
        assert_eq!(
            attention_of(SessionStatus::ResponseReady, None),
            SessionAttention::Finished
        );
        assert_eq!(
            attention_of(
                SessionStatus::AwaitingInput,
                Some(AttentionKind::Permission)
            ),
            SessionAttention::Blocked
        );
        assert_eq!(
            attention_of(SessionStatus::AwaitingInput, Some(AttentionKind::Question)),
            SessionAttention::Blocked
        );
    }

    /// DEV-788's distinction, carried into the header: a session with nothing to
    /// do is not blocked. Counting it as blocked is what made a headline reading
    /// "18 sessions waiting for your input" impossible to triage.
    #[test]
    fn an_idle_awaiting_session_is_not_blocked() {
        use crate::session::{AttentionKind, SessionStatus};
        assert_eq!(
            attention_of(SessionStatus::AwaitingInput, Some(AttentionKind::Idle)),
            SessionAttention::Other
        );
    }

    /// No context attached: treated as blocking, matching the sort key's own
    /// safer default.
    #[test]
    fn an_awaiting_session_with_no_context_counts_as_blocked() {
        use crate::session::SessionStatus;
        assert_eq!(
            attention_of(SessionStatus::AwaitingInput, None),
            SessionAttention::Blocked
        );
    }

    /// Suspended, idle and finished-with-the-process-gone are counted nowhere —
    /// a header saying "32 finished" for a project of restored rows would be
    /// describing history rather than work.
    #[test]
    fn inert_statuses_are_counted_nowhere() {
        use crate::session::SessionStatus;
        for status in [
            SessionStatus::Idle,
            SessionStatus::Suspended,
            SessionStatus::Done,
        ] {
            assert_eq!(attention_of(status, None), SessionAttention::Other);
        }
    }

    // ── draw_order ─────────────────────────────────────────────────────

    /// A header is emitted once, on its group's first row.
    #[test]
    fn a_header_lands_only_on_its_groups_first_row() {
        let blocks = vec![
            block(None, vec![3, 4], GroupCounts::default()),
            block(
                Some("review"),
                vec![0, 1, 2],
                GroupCounts {
                    running: 3,
                    ..Default::default()
                },
            ),
        ];
        let rows = draw_order(blocks);

        let headers: Vec<Option<&str>> = rows
            .iter()
            .map(|(h, _)| h.as_ref().map(|h| h.name.as_str()))
            .collect();
        assert_eq!(headers, [None, None, Some("review"), None, None]);
        let indices: Vec<Option<usize>> = rows.iter().map(|(_, i)| *i).collect();
        assert_eq!(
            indices,
            [Some(3), Some(4), Some(0), Some(1), Some(2)],
            "every row carries a session — nothing here is collapsed"
        );
    }

    #[test]
    fn the_header_carries_its_groups_counts() {
        let blocks = vec![block(
            Some("review"),
            vec![0],
            GroupCounts {
                running: 2,
                finished: 1,
                blocked: 1,
            },
        )];
        let rows = draw_order(blocks);
        let header = rows[0].0.as_ref().expect("first row carries the header");
        assert_eq!(
            header.counts.summary().as_deref(),
            Some("2 running · 1 finished · 1 blocked")
        );
    }

    /// The acceptance criterion: a collapsed group shows its header and no
    /// rows. `draw_order` hangs a header on its group's first row, so a group
    /// with no rows left has to be able to emit a header on its own — otherwise
    /// collapsing a group would delete it from the sidebar.
    #[test]
    fn a_fully_collapsed_group_emits_a_header_with_no_row() {
        let mut collapsed = block(
            Some("review"),
            vec![],
            GroupCounts {
                running: 2,
                ..Default::default()
            },
        );
        collapsed.collapsed = true;
        let rows = draw_order(vec![
            block(None, vec![9], GroupCounts::default()),
            collapsed,
        ]);

        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0], (None, Some(9)), "the ungrouped row, as before");
        let (header, idx) = &rows[1];
        assert_eq!(*idx, None, "a header-only row");
        let header = header
            .as_ref()
            .expect("the collapsed group kept its header");
        assert_eq!(header.name, "review");
        assert!(header.collapsed, "and it knows to draw a folded chevron");
        assert_eq!(header.counts.summary().as_deref(), Some("2 running"));
    }

    /// A collapsed group holding the active session draws that one row under
    /// its header, so the two passes compose.
    #[test]
    fn a_collapsed_group_holding_the_active_session_draws_one_row() {
        let mut order = GroupOrder::from_names(vec!["review".into()]);
        order.toggle_collapsed("review");
        let blocks = collapse_blocks(
            vec![block(Some("review"), vec![4, 5, 6], GroupCounts::default())],
            &order,
            Some(5),
        );
        let rows = draw_order(blocks);

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].1, Some(5));
        assert!(rows[0].0.as_ref().is_some_and(|h| h.collapsed));
    }

    /// The order is append-only, so a name that lost its last session keeps its
    /// index — and the group reappears above a group added after it rather than
    /// at the end. This is the in-memory half of "a group that empties and
    /// refills reappears in its earlier position"; the persisted half is tested
    /// in `crate::state`.
    #[test]
    fn an_emptied_group_reappears_in_its_old_position() {
        let mut order = GroupOrder::default();
        order.intern("review");
        order.intern("linear");

        // `review` empties: no session carries it, so it produces no block…
        let only_linear = [(Some("linear"), SessionAttention::Running)];
        let blocks = blocks_of(&[0], &only_linear, &order);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].name.as_deref(), Some("linear"));

        // …and when it refills it is above `linear` again, not after it.
        let refilled = [
            (Some("linear"), SessionAttention::Running),
            (Some("review"), SessionAttention::Running),
        ];
        let blocks = blocks_of(&[0, 1], &refilled, &order);
        let names: Vec<&str> = blocks.iter().filter_map(|b| b.name.as_deref()).collect();
        assert_eq!(names, ["review", "linear"]);
    }
}
