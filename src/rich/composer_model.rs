//! Unified composer model (DEV-30).
//!
//! The Narrative compose bar and the Scratch Pad each grew their own
//! submission logic. This module is the single, view-agnostic model both are
//! meant to sit on, so text-only and attachment-only submissions, inline
//! attachment-failure surfacing, prompt history, and agent-capability
//! adaptation behave identically in both places.
//!
//! It owns *policy*, not *pixels*: what counts as submittable, which
//! attachments are ready, how history navigation moves. The GPUI widgets keep
//! their own editor/rendering and delegate these decisions here.

/// What the target agent is willing to accept, so the composer can adapt
/// (e.g. hide the attach button, or reject an image for a text-only agent).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ComposerCapabilities {
    pub accepts_attachments: bool,
    pub accepts_images: bool,
}

impl Default for ComposerCapabilities {
    fn default() -> Self {
        Self { accepts_attachments: true, accepts_images: true }
    }
}

impl ComposerCapabilities {
    /// A text-only agent: no attachments of any kind.
    pub const TEXT_ONLY: ComposerCapabilities =
        ComposerCapabilities { accepts_attachments: false, accepts_images: false };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachmentKind {
    File,
    Image,
}

/// Lifecycle of an attachment as it is prepared for submission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttachmentState {
    /// Still being read/encoded.
    Pending,
    /// Ready to send.
    Ready,
    /// Failed to prepare; carries a reason to surface inline.
    Failed(String),
}

/// One attachment chip in the composer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentChip {
    pub name: String,
    pub kind: AttachmentKind,
    pub state: AttachmentState,
}

impl AttachmentChip {
    pub fn is_ready(&self) -> bool {
        matches!(self.state, AttachmentState::Ready)
    }
    pub fn is_pending(&self) -> bool {
        matches!(self.state, AttachmentState::Pending)
    }
    pub fn failure(&self) -> Option<&str> {
        match &self.state {
            AttachmentState::Failed(reason) => Some(reason),
            _ => None,
        }
    }
}

/// Why a draft can't be submitted right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubmitError {
    /// No text and no ready attachments.
    Empty,
    /// Has attachments but the agent accepts none.
    AttachmentsUnsupported,
    /// Has an image attachment but the agent accepts no images.
    ImagesUnsupported,
    /// One or more attachments are still preparing.
    AttachmentsNotReady,
}

/// A composition in progress: the text plus its attachment chips.
#[derive(Debug, Default, Clone)]
pub struct Draft {
    pub text: String,
    pub attachments: Vec<AttachmentChip>,
}

impl Draft {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_attachment(&mut self, chip: AttachmentChip) {
        self.attachments.push(chip);
    }

    /// Attachments that failed to prepare — surfaced inline, excluded from the
    /// submission (they never block sending the rest).
    pub fn failed_attachments(&self) -> impl Iterator<Item = &AttachmentChip> {
        self.attachments.iter().filter(|a| a.failure().is_some())
    }

    fn ready_attachments(&self) -> impl Iterator<Item = &AttachmentChip> {
        self.attachments.iter().filter(|a| a.is_ready())
    }

    fn has_text(&self) -> bool {
        !self.text.trim().is_empty()
    }

    /// Validate against agent capabilities. Text-only and attachment-only
    /// submissions are both allowed. Failed attachments are ignored (surfaced
    /// separately); pending ones block until they resolve.
    pub fn validate(&self, caps: ComposerCapabilities) -> Result<(), SubmitError> {
        let has_ready = self.ready_attachments().next().is_some();
        let has_pending = self.attachments.iter().any(|a| a.is_pending());
        let has_any_nonfailed = has_ready || has_pending;

        // Capability gating applies to any non-failed attachment the user added.
        if has_any_nonfailed && !caps.accepts_attachments {
            return Err(SubmitError::AttachmentsUnsupported);
        }
        if !caps.accepts_images
            && self
                .attachments
                .iter()
                .any(|a| a.kind == AttachmentKind::Image && a.failure().is_none())
        {
            return Err(SubmitError::ImagesUnsupported);
        }
        // Nothing to send at all.
        if !self.has_text() && !has_any_nonfailed {
            return Err(SubmitError::Empty);
        }
        // Something is still preparing.
        if has_pending {
            return Err(SubmitError::AttachmentsNotReady);
        }
        Ok(())
    }

    pub fn can_submit(&self, caps: ComposerCapabilities) -> bool {
        self.validate(caps).is_ok()
    }
}

/// Bounded prompt history with cursor navigation (up/down recall). Consecutive
/// duplicates are collapsed; oldest entries are evicted past the cap.
#[derive(Debug, Clone)]
pub struct PromptHistory {
    entries: Vec<String>,
    /// Navigation cursor; `None` means "at the live draft" (past the newest).
    cursor: Option<usize>,
    cap: usize,
}

impl Default for PromptHistory {
    fn default() -> Self {
        Self::with_capacity(100)
    }
}

impl PromptHistory {
    pub fn with_capacity(cap: usize) -> Self {
        Self { entries: Vec::new(), cursor: None, cap: cap.max(1) }
    }

    /// Commit a submitted prompt to history and reset navigation.
    pub fn push(&mut self, prompt: &str) {
        let p = prompt.trim();
        if p.is_empty() {
            return;
        }
        if self.entries.last().map(|e| e == p).unwrap_or(false) {
            self.cursor = None;
            return;
        }
        self.entries.push(p.to_string());
        if self.entries.len() > self.cap {
            let overflow = self.entries.len() - self.cap;
            self.entries.drain(0..overflow);
        }
        self.cursor = None;
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Recall the previous (older) entry — the "up arrow" action.
    pub fn prev(&mut self) -> Option<&str> {
        if self.entries.is_empty() {
            return None;
        }
        let next = match self.cursor {
            None => self.entries.len() - 1,
            Some(0) => 0,
            Some(i) => i - 1,
        };
        self.cursor = Some(next);
        Some(&self.entries[next])
    }

    /// Recall the next (newer) entry — the "down arrow" action. Returns `None`
    /// when navigating back past the newest entry (i.e. to the live draft).
    pub fn next(&mut self) -> Option<&str> {
        match self.cursor {
            Some(i) if i + 1 < self.entries.len() => {
                self.cursor = Some(i + 1);
                Some(&self.entries[i + 1])
            }
            Some(_) => {
                self.cursor = None;
                None
            }
            None => None,
        }
    }

    /// Reset navigation back to the live draft (e.g. after the user edits).
    pub fn reset_cursor(&mut self) {
        self.cursor = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ready(name: &str, kind: AttachmentKind) -> AttachmentChip {
        AttachmentChip { name: name.into(), kind, state: AttachmentState::Ready }
    }

    #[test]
    fn text_only_submission_is_valid() {
        let mut d = Draft::new();
        d.text = "hello".into();
        assert!(d.can_submit(ComposerCapabilities::default()));
    }

    #[test]
    fn attachment_only_submission_is_valid() {
        let mut d = Draft::new();
        d.add_attachment(ready("a.png", AttachmentKind::Image));
        assert!(d.can_submit(ComposerCapabilities::default()));
    }

    #[test]
    fn empty_draft_is_rejected() {
        let d = Draft::new();
        assert_eq!(d.validate(ComposerCapabilities::default()), Err(SubmitError::Empty));
        // Whitespace-only text is also empty.
        let mut ws = Draft::new();
        ws.text = "   \n".into();
        assert_eq!(ws.validate(ComposerCapabilities::default()), Err(SubmitError::Empty));
    }

    #[test]
    fn failed_attachments_do_not_block_text_submission() {
        let mut d = Draft::new();
        d.text = "send anyway".into();
        d.add_attachment(AttachmentChip {
            name: "big.bin".into(),
            kind: AttachmentKind::File,
            state: AttachmentState::Failed("too large".into()),
        });
        assert!(d.can_submit(ComposerCapabilities::default()));
        assert_eq!(d.failed_attachments().count(), 1);
    }

    #[test]
    fn pending_attachment_blocks_until_ready() {
        let mut d = Draft::new();
        d.text = "wait".into();
        d.add_attachment(AttachmentChip {
            name: "x.png".into(),
            kind: AttachmentKind::Image,
            state: AttachmentState::Pending,
        });
        assert_eq!(d.validate(ComposerCapabilities::default()), Err(SubmitError::AttachmentsNotReady));
    }

    #[test]
    fn capabilities_gate_attachments_and_images() {
        let mut img = Draft::new();
        img.add_attachment(ready("a.png", AttachmentKind::Image));
        // Text-only agent rejects any attachment.
        assert_eq!(img.validate(ComposerCapabilities::TEXT_ONLY), Err(SubmitError::AttachmentsUnsupported));
        // Attachments-yes, images-no agent rejects the image specifically.
        let caps = ComposerCapabilities { accepts_attachments: true, accepts_images: false };
        assert_eq!(img.validate(caps), Err(SubmitError::ImagesUnsupported));
        // A file attachment is fine for that agent.
        let mut file = Draft::new();
        file.add_attachment(ready("a.txt", AttachmentKind::File));
        assert!(file.can_submit(caps));
    }

    #[test]
    fn history_recall_walks_backwards_and_forwards() {
        let mut h = PromptHistory::with_capacity(10);
        h.push("first");
        h.push("second");
        h.push("third");
        assert_eq!(h.prev(), Some("third"));
        assert_eq!(h.prev(), Some("second"));
        assert_eq!(h.prev(), Some("first"));
        assert_eq!(h.prev(), Some("first")); // clamps at oldest
        assert_eq!(h.next(), Some("second"));
        assert_eq!(h.next(), Some("third"));
        assert_eq!(h.next(), None); // back to live draft
    }

    #[test]
    fn history_collapses_consecutive_dupes_and_caps() {
        let mut h = PromptHistory::with_capacity(2);
        h.push("a");
        h.push("a"); // dup, ignored
        assert_eq!(h.len(), 1);
        h.push("b");
        h.push("c"); // evicts "a"
        assert_eq!(h.len(), 2);
        assert_eq!(h.prev(), Some("c"));
        assert_eq!(h.prev(), Some("b"));
        assert_eq!(h.prev(), Some("b")); // "a" was evicted
    }

    #[test]
    fn empty_prompts_are_not_stored() {
        let mut h = PromptHistory::default();
        h.push("   ");
        h.push("");
        assert!(h.is_empty());
    }
}
