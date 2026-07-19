//! Session-sync settings section — S3 store configuration + a connection test,
//! plus the end-to-end-encryption **bootstrap** (DEV-188 / DEV-209).
//!
//! No credentials are stored: the user supplies an AWS profile name resolved
//! from `~/.aws/credentials`. Encryption uses a random data key (cached in the
//! Keychain) that a passphrase wraps into `keyring/identity.age` in the bucket;
//! this section walks a device through creating or unlocking that key. The
//! crypto primitives are the tested DEV-189 ones — this is orchestration + UI.

use gpui::*;

use super::widgets::{card, input_frame, labeled_row, section_note, section_title};
use super::SettingsWindowState;
use crate::settings::SyncSettings;
use crate::sync::crypto::{self, DataKey, KEYRING_OBJECT_KEY};
use crate::sync::s3_store::{S3Config, S3Store};
use crate::sync::store::SyncStore;
use crate::text_input::{TextInput, TextInputEvent};
use crate::theme::theme;
use crate::AppState;

/// Result of the last connection test.
enum TestStatus {
    Idle,
    Testing,
    Ok(String),
    Failed(String),
}

/// Encryption bootstrap state for this device. Errors are held out-of-band in
/// [`SyncSection::enc_error`] so a failed op stays on the right form.
#[derive(Clone, Copy, PartialEq)]
enum EncState {
    /// Not yet determined (needs a reachable bucket to check).
    Unknown,
    /// Working — the label lives in [`SyncSection::enc_busy`].
    Working,
    /// Data key is in the Keychain — this device can sync.
    Ready,
    /// No local key, but the bucket has a keyring — enter the passphrase.
    NeedsUnlock,
    /// No local key and no keyring — set a new passphrase.
    NeedsSetup,
}

/// Owns the S3 config inputs, the connection-test state, and the encryption
/// bootstrap state + passphrase fields.
pub(super) struct SyncSection {
    profile_input: Entity<TextInput>,
    bucket_input: Entity<TextInput>,
    endpoint_input: Entity<TextInput>,
    region_input: Entity<TextInput>,
    status: TestStatus,
    enc_state: EncState,
    /// In-progress label while `enc_state == Working`.
    enc_busy: String,
    /// Last encryption error, shown on the current form.
    enc_error: Option<String>,
    passphrase_input: Entity<TextInput>,
    confirm_input: Entity<TextInput>,
    /// First click of a destructive reset arms it; the second confirms.
    reset_armed: bool,
}

fn non_empty(s: &str) -> Option<String> {
    let t = s.trim();
    (!t.is_empty()).then(|| t.to_string())
}

// --- background key operations (run off the UI thread via `sync::rt`) --------

fn check_key(config: &S3Config) -> anyhow::Result<EncState> {
    if crypto::keychain::load()?.is_some() {
        return Ok(EncState::Ready);
    }
    let store = S3Store::new(config)?;
    let exists = crate::sync::rt::block_on(store.get(KEYRING_OBJECT_KEY))?.is_some();
    Ok(if exists {
        EncState::NeedsUnlock
    } else {
        EncState::NeedsSetup
    })
}

/// Generate a fresh data key, cache it, and upload the passphrase-wrapped
/// keyring. Used for first-device setup and (destructively) for reset.
fn create_key(config: &S3Config, passphrase: &str) -> anyhow::Result<()> {
    let key = DataKey::generate();
    crypto::keychain::store(&key)?;
    let wrapped = key.wrap_with_passphrase(passphrase)?;
    let store = S3Store::new(config)?;
    crate::sync::rt::block_on(store.put(KEYRING_OBJECT_KEY, wrapped))?;
    Ok(())
}

fn unlock_key(config: &S3Config, passphrase: &str) -> anyhow::Result<()> {
    let store = S3Store::new(config)?;
    let blob = crate::sync::rt::block_on(store.get(KEYRING_OBJECT_KEY))?
        .ok_or_else(|| anyhow::anyhow!("no keyring found in the bucket"))?;
    let key = DataKey::unwrap_with_passphrase(&blob, passphrase)?;
    crypto::keychain::store(&key)?;
    Ok(())
}

impl SyncSection {
    pub(super) fn new(cx: &mut Context<SettingsWindowState>, settings: &SyncSettings) -> Self {
        let mk = |cx: &mut Context<SettingsWindowState>, text: &str, ph: &str| {
            let input = cx.new(|cx| TextInput::new(cx, text.to_string(), ph.to_string()));
            cx.subscribe(&input, |this, _input, event: &TextInputEvent, cx| {
                if matches!(event, TextInputEvent::Changed | TextInputEvent::Submitted) {
                    this.sync.persist(&this.app, cx);
                }
            })
            .detach();
            input
        };

        let profile_input = mk(
            cx,
            &settings.profile.clone().unwrap_or_default(),
            "AWS profile (e.g. deverge-sandbox)",
        );
        let bucket_input = mk(
            cx,
            &settings.bucket.clone().unwrap_or_default(),
            "Bucket name",
        );
        let endpoint_input = mk(
            cx,
            &settings.endpoint.clone().unwrap_or_default(),
            "Custom endpoint (R2/MinIO/NAS) — leave blank for AWS",
        );
        let region_input = mk(
            cx,
            &settings.region.clone().unwrap_or_default(),
            "Bucket region (e.g. ap-southeast-2)",
        );

        let passphrase_input = cx.new(|cx| TextInput::new(cx, "", "Sync passphrase").masked());
        let confirm_input = cx.new(|cx| TextInput::new(cx, "", "Confirm passphrase").masked());

        Self {
            profile_input,
            bucket_input,
            endpoint_input,
            region_input,
            status: TestStatus::Idle,
            enc_state: EncState::Unknown,
            enc_busy: String::new(),
            enc_error: None,
            passphrase_input,
            confirm_input,
            reset_armed: false,
        }
    }

    /// Build the S3 config from the current inputs, or `None` if profile/bucket
    /// aren't both set.
    fn s3_config(&self, cx: &Context<SettingsWindowState>) -> Option<S3Config> {
        let profile = non_empty(&self.profile_input.read(cx).text())?;
        let bucket = non_empty(&self.bucket_input.read(cx).text())?;
        let region = non_empty(&self.region_input.read(cx).text())?;
        Some(S3Config {
            bucket_name: bucket,
            region,
            profile,
            endpoint: non_empty(&self.endpoint_input.read(cx).text()),
        })
    }

    fn persist(&self, app: &WeakEntity<AppState>, cx: &mut Context<SettingsWindowState>) {
        let Some(app_entity) = app.upgrade() else {
            return;
        };
        let device_id = app_entity.read(cx).user_settings.sync.device_id.clone();
        let sync = SyncSettings {
            bucket: non_empty(&self.bucket_input.read(cx).text()),
            region: non_empty(&self.region_input.read(cx).text()),
            profile: non_empty(&self.profile_input.read(cx).text()),
            endpoint: non_empty(&self.endpoint_input.read(cx).text()),
            device_id,
        };
        app.update(cx, |state: &mut AppState, cx| {
            state.pending_action = Some(crate::SettingsAction::UpdateSync(sync).into());
            cx.notify();
        })
        .ok();
    }

    fn test_connection(
        &mut self,
        app: WeakEntity<AppState>,
        cx: &mut Context<SettingsWindowState>,
    ) {
        let Some(config) = self.s3_config(cx) else {
            self.status =
                TestStatus::Failed("Enter a profile, bucket, and region first.".to_string());
            cx.notify();
            return;
        };
        self.status = TestStatus::Testing;
        cx.notify();

        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    crate::sync::rt::block_on(crate::sync::connect::validate_bucket(&config))
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(region) => {
                        this.sync.status = TestStatus::Ok(region);
                        this.sync.persist(&app, cx);
                        // A reachable bucket means we can determine encryption state.
                        this.sync.check_encryption(cx);
                    }
                    Err(e) => {
                        this.sync.status = TestStatus::Failed(format!("{e}"));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Determine the encryption bootstrap state (Keychain + bucket keyring).
    fn check_encryption(&mut self, cx: &mut Context<SettingsWindowState>) {
        let Some(config) = self.s3_config(cx) else {
            return;
        };
        self.enc_state = EncState::Working;
        self.enc_busy = "Checking encryption…".to_string();
        self.enc_error = None;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { check_key(&config) })
                .await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(state) => this.sync.enc_state = state,
                    Err(e) => {
                        this.sync.enc_state = EncState::Unknown;
                        this.sync.enc_error = Some(format!("{e}"));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// First-device setup: validate the passphrase, create + upload the key.
    fn do_setup(&mut self, cx: &mut Context<SettingsWindowState>) {
        let pass = self.passphrase_input.read(cx).text().to_string();
        let confirm = self.confirm_input.read(cx).text().to_string();
        if pass.chars().count() < 8 {
            return self.fail("Passphrase must be at least 8 characters.", cx);
        }
        if pass != confirm {
            return self.fail("Passphrases don't match.", cx);
        }
        self.run_key_op(
            "Setting up…",
            EncState::NeedsSetup,
            move |config| create_key(config, &pass),
            cx,
        );
    }

    fn do_unlock(&mut self, cx: &mut Context<SettingsWindowState>) {
        let pass = self.passphrase_input.read(cx).text().to_string();
        if pass.is_empty() {
            return self.fail("Enter your passphrase.", cx);
        }
        self.run_key_op(
            "Unlocking…",
            EncState::NeedsUnlock,
            move |config| unlock_key(config, &pass),
            cx,
        );
    }

    fn do_reset(&mut self, cx: &mut Context<SettingsWindowState>) {
        let pass = self.passphrase_input.read(cx).text().to_string();
        if pass.chars().count() < 8 {
            return self.fail("Set a new passphrase (at least 8 chars) to reset.", cx);
        }
        self.reset_armed = false;
        self.run_key_op(
            "Resetting…",
            EncState::NeedsUnlock,
            move |config| create_key(config, &pass),
            cx,
        );
    }

    /// Set an inline error on the current form (no state change).
    fn fail(&mut self, msg: &str, cx: &mut Context<SettingsWindowState>) {
        self.enc_error = Some(msg.to_string());
        cx.notify();
    }

    /// Shared runner for setup/unlock/reset: set Working, run the op off-thread,
    /// then land on Ready or fall back to `revert` with an inline error.
    fn run_key_op<F>(
        &mut self,
        label: &str,
        revert: EncState,
        op: F,
        cx: &mut Context<SettingsWindowState>,
    ) where
        F: FnOnce(&S3Config) -> anyhow::Result<()> + Send + 'static,
    {
        let Some(config) = self.s3_config(cx) else {
            return self.fail(
                "Test the connection first — the bucket must be reachable.",
                cx,
            );
        };
        self.enc_state = EncState::Working;
        self.enc_busy = label.to_string();
        self.enc_error = None;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { op(&config) })
                .await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(()) => {
                        this.sync.enc_state = EncState::Ready;
                        this.sync.enc_error = None;
                        this.sync
                            .passphrase_input
                            .update(cx, |i, cx| i.set_text_silent("", cx));
                        this.sync
                            .confirm_input
                            .update(cx, |i, cx| i.set_text_silent("", cx));
                    }
                    Err(e) => {
                        this.sync.enc_state = revert;
                        this.sync.enc_error = Some(format!("{e}"));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    // --- rendering ----------------------------------------------------------

    fn button(id: &'static str, label: &str, bg: Hsla, fg: Hsla) -> Stateful<Div> {
        div()
            .id(id)
            .cursor_pointer()
            .px(px(12.0))
            .py(px(6.0))
            .rounded(px(6.0))
            .bg(bg)
            .text_size(px(12.0))
            .text_color(fg)
            .child(label.to_string())
    }

    fn error_line(&self) -> AnyElement {
        match &self.enc_error {
            Some(msg) => div()
                .text_size(px(11.0))
                .text_color(theme().danger)
                .child(format!("✗ {msg}"))
                .into_any_element(),
            None => div().into_any_element(),
        }
    }

    fn setup_form(&self, cx: &mut Context<SettingsWindowState>) -> AnyElement {
        let t = theme();
        div()
            .flex()
            .flex_col()
            .gap(px(8.0))
            .child(section_note(
                "Set a sync passphrase. You enter it once on each of your Macs; it \
                     encrypts everything before it leaves this machine and can't be \
                     recovered if lost.",
            ))
            .child(labeled_row(
                "Passphrase",
                input_frame(self.passphrase_input.clone()),
            ))
            .child(labeled_row(
                "Confirm",
                input_frame(self.confirm_input.clone()),
            ))
            .child(self.error_line())
            .child(
                Self::button("sync-enc-create", "Create", t.accent, t.text_on_accent)
                    .hover(|s| s.bg(theme().lavender))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _e, _w, cx| {
                            cx.stop_propagation();
                            this.sync.do_setup(cx);
                        }),
                    ),
            )
            .into_any_element()
    }

    fn unlock_form(&self, cx: &mut Context<SettingsWindowState>) -> AnyElement {
        let t = theme();
        div()
            .flex()
            .flex_col()
            .gap(px(8.0))
            .child(section_note(
                "This bucket is already encrypted. Enter your sync passphrase to \
                     unlock sync on this Mac.",
            ))
            .child(labeled_row(
                "Passphrase",
                input_frame(self.passphrase_input.clone()),
            ))
            .child(self.error_line())
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(10.0))
                    .child(
                        Self::button("sync-enc-unlock", "Unlock", t.accent, t.text_on_accent)
                            .hover(|s| s.bg(theme().lavender))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _e, _w, cx| {
                                    cx.stop_propagation();
                                    this.sync.do_unlock(cx);
                                }),
                            ),
                    )
                    .child(self.reset_control(cx)),
            )
            .into_any_element()
    }

    /// The "Forgot passphrase? → Reset" destructive escape hatch (two clicks).
    fn reset_control(&self, cx: &mut Context<SettingsWindowState>) -> AnyElement {
        if self.reset_armed {
            div()
                .flex()
                .flex_col()
                .gap(px(4.0))
                .child(
                    div()
                        .text_size(px(10.0))
                        .text_color(theme().danger)
                        .child("Enter a new passphrase above, then confirm — every already-synced session becomes unreadable."),
                )
                .child(
                    Self::button(
                        "sync-enc-reset-confirm",
                        "Erase & start over",
                        theme().danger,
                        theme().bg_base,
                    )
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _e, _w, cx| {
                            cx.stop_propagation();
                            this.sync.do_reset(cx);
                        }),
                    ),
                )
                .into_any_element()
        } else {
            div()
                .id("sync-enc-forgot")
                .cursor_pointer()
                .text_size(px(11.0))
                .text_color(theme().text_faint)
                .child("Forgot passphrase?")
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _e, _w, cx| {
                        cx.stop_propagation();
                        this.sync.reset_armed = true;
                        cx.notify();
                    }),
                )
                .into_any_element()
        }
    }

    fn encryption_card(&self, cx: &mut Context<SettingsWindowState>) -> AnyElement {
        let t = theme();
        let body: AnyElement = match self.enc_state {
            EncState::Unknown => div()
                .flex()
                .flex_col()
                .gap(px(6.0))
                .child(
                    div()
                        .text_size(px(12.0))
                        .text_color(t.text_secondary)
                        .child("Test the connection above — encryption setup appears once the bucket is reachable."),
                )
                .child(self.error_line())
                .into_any_element(),
            EncState::Working => div()
                .text_size(px(12.0))
                .text_color(t.text_secondary)
                .child(self.enc_busy.clone())
                .into_any_element(),
            EncState::Ready => div()
                .text_size(px(12.0))
                .text_color(t.success)
                .child("🔒 Encryption ready on this device.")
                .into_any_element(),
            EncState::NeedsSetup => self.setup_form(cx),
            EncState::NeedsUnlock => self.unlock_form(cx),
        };

        card()
            .child(section_title("Encryption"))
            .child(body)
            .into_any_element()
    }

    pub(super) fn render(&self, cx: &mut Context<SettingsWindowState>) -> impl IntoElement {
        let (status_text, status_color) = match &self.status {
            TestStatus::Idle => (String::new(), theme().text_faint),
            TestStatus::Testing => ("Testing…".to_string(), theme().text_secondary),
            TestStatus::Ok(region) => (format!("✓ Connected — region {region}"), theme().success),
            TestStatus::Failed(msg) => (format!("✗ {msg}"), theme().danger),
        };

        let test_button = Self::button(
            "sync-test-connection",
            "Test connection",
            theme().accent,
            theme().text_on_accent,
        )
        .hover(|s| s.bg(theme().lavender))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|this, _event, _window, cx| {
                cx.stop_propagation();
                let app = this.app.clone();
                this.sync.test_connection(app, cx);
            }),
        );

        div()
            .id("sync-pane")
            .flex()
            .flex_col()
            .flex_1()
            .min_w(px(0.0))
            .overflow_y_scroll()
            .p(px(20.0))
            .gap(px(12.0))
            .child(section_title("Session Sync"))
            .child(section_note(
                "Push a session (its list row + Claude conversation) from this Mac and \
                     resume it on another, via an S3-compatible bucket. No credentials are \
                     stored — supply an AWS profile name; sync resolves it from \
                     ~/.aws/credentials. Keep the profile's session valid.",
            ))
            .child(
                card()
                    .child(labeled_row(
                        "Profile",
                        input_frame(self.profile_input.clone()),
                    ))
                    .child(labeled_row(
                        "Bucket",
                        input_frame(self.bucket_input.clone()),
                    ))
                    .child(labeled_row(
                        "Endpoint",
                        input_frame(self.endpoint_input.clone()),
                    ))
                    .child(labeled_row(
                        "Region",
                        input_frame(self.region_input.clone()),
                    ))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(10.0))
                            .child(test_button)
                            .child(
                                div()
                                    .text_size(px(11.0))
                                    .text_color(status_color)
                                    .child(status_text),
                            ),
                    ),
            )
            .child(self.encryption_card(cx))
    }
}
