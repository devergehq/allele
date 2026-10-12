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
//! from the sessions. Removing a name from the order is DEV-937.

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

/// Are two sessions in the same group, treating "no group" as a group of its
/// own?
///
/// Both ungrouped is `true`: the ungrouped block is a run of rows like any
/// other, and dragging to reorder inside it worked before groups existed and
/// must keep working. One of each is `false` — dragging a session out of its
/// group is DEV-937, and until then the drop is a no-op rather than a silent
/// regroup the user did not ask for.
pub fn same_group_opt(a: Option<&str>, b: Option<&str>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => same_group(a, b),
        _ => false,
    }
}

/// A project's group names, in the order the project first saw them.
///
/// Append-only by design. A name that loses its last session stays in the list
/// so it reappears in its old position when the group refills, which is the
/// behaviour the issue asks for; reordering and removal are DEV-937.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GroupOrder {
    names: Vec<String>,
}

impl GroupOrder {
    pub fn from_names(names: Vec<String>) -> Self {
        Self { names }
    }

    pub fn names(&self) -> &[String] {
        &self.names
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

    /// Where `name` sits in the order, for sorting. `None` for a name the
    /// project has never seen.
    pub fn position(&self, name: &str) -> Option<usize> {
        self.names.iter().position(|known| same_group(known, name))
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
    /// Counts over `sessions` — i.e. over the rows that will actually be drawn.
    pub counts: GroupCounts,
}

/// A group header's content: its name and the counts beside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupHeader {
    pub name: String,
    pub counts: GroupCounts,
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

/// Flatten blocks into the sequence the sidebar draws: one entry per session
/// row, carrying the header that must be emitted immediately before it.
///
/// Flattened rather than handed over as nested blocks so the existing row loop
/// in `render.rs` keeps its shape — it stays one loop over session indices, and
/// the only new line in it emits a header. A nested loop would have re-indented
/// four hundred lines of unrelated rendering, which is the "aggressive refactor
/// in a feature PR" CONTRIBUTING.md asks people not to do.
pub fn draw_order(blocks: Vec<GroupBlock>) -> Vec<(Option<GroupHeader>, usize)> {
    let mut rows = Vec::new();
    for block in blocks {
        let mut header = block.name.map(|name| GroupHeader {
            name,
            counts: block.counts,
        });
        for idx in block.sessions {
            // `take` so the header lands on the group's first row only.
            rows.push((header.take(), idx));
        }
    }
    rows
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
                });
                blocks.last_mut().expect("just pushed")
            }
        };
        block.sessions.push(idx);
        block.counts.add(attention);
    }

    // Recorded order first, then anything the order has never seen, in the
    // order it appeared. `usize::MAX` rather than sorting unknowns first: a
    // name allele did not intern is the odd case and belongs at the end.
    blocks.sort_by_key(|b| {
        b.name
            .as_deref()
            .and_then(|n| group_order.position(n))
            .unwrap_or(usize::MAX)
    });

    let mut out = Vec::with_capacity(blocks.len() + 1);
    if !ungrouped.sessions.is_empty() {
        out.push(ungrouped);
    }
    out.extend(blocks);
    out
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

    #[test]
    fn two_ungrouped_sessions_are_in_the_same_group() {
        // The ungrouped block is reorderable, exactly as the flat list was.
        assert!(same_group_opt(None, None));
    }

    #[test]
    fn a_grouped_and_an_ungrouped_session_are_not() {
        assert!(!same_group_opt(Some("review"), None));
        assert!(!same_group_opt(None, Some("review")));
    }

    #[test]
    fn two_sessions_in_differently_spelled_one_group_match() {
        assert!(same_group_opt(Some("review"), Some("Review")));
        assert!(!same_group_opt(Some("review"), Some("linear")));
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
            GroupBlock {
                name: None,
                sessions: vec![3, 4],
                counts: GroupCounts::default(),
            },
            GroupBlock {
                name: Some("review".into()),
                sessions: vec![0, 1, 2],
                counts: GroupCounts {
                    running: 3,
                    ..Default::default()
                },
            },
        ];
        let rows = draw_order(blocks);

        let headers: Vec<Option<&str>> = rows
            .iter()
            .map(|(h, _)| h.as_ref().map(|h| h.name.as_str()))
            .collect();
        assert_eq!(headers, [None, None, Some("review"), None, None]);
        let indices: Vec<usize> = rows.iter().map(|(_, i)| *i).collect();
        assert_eq!(indices, [3, 4, 0, 1, 2]);
    }

    #[test]
    fn the_header_carries_its_groups_counts() {
        let blocks = vec![GroupBlock {
            name: Some("review".into()),
            sessions: vec![0],
            counts: GroupCounts {
                running: 2,
                finished: 1,
                blocked: 1,
            },
        }];
        let rows = draw_order(blocks);
        let header = rows[0].0.as_ref().expect("first row carries the header");
        assert_eq!(
            header.counts.summary().as_deref(),
            Some("2 running · 1 finished · 1 blocked")
        );
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
