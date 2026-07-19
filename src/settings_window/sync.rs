//! Session-sync settings section (DEV-188) — S3 store configuration + a
//! connection test that validates the bucket and resolves its region.
//!
//! No credentials are stored: the user supplies an AWS profile name that the
//! sync layer resolves from `~/.aws/credentials`. "Test connection" runs the
//! same calls sync makes (via the GPUI→tokio bridge), so a misconfiguration is
//! caught here, not at first sync.

use gpui::*;

use super::widgets::{card, input_frame, labeled_row, section_note, section_title};
use super::SettingsWindowState;
use crate::settings::SyncSettings;
use crate::sync::s3_store::S3Config;
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

/// Owns the S3 config inputs + the connection-test state.
pub(super) struct SyncSection {
    profile_input: Entity<TextInput>,
    bucket_input: Entity<TextInput>,
    endpoint_input: Entity<TextInput>,
    /// Resolved region (set by a successful test; persisted with the config).
    region: Option<String>,
    status: TestStatus,
}

fn non_empty(s: &str) -> Option<String> {
    let t = s.trim();
    (!t.is_empty()).then(|| t.to_string())
}

impl SyncSection {
    pub(super) fn new(cx: &mut Context<SettingsWindowState>, settings: &SyncSettings) -> Self {
        let profile_input = cx.new(|cx| {
            TextInput::new(
                cx,
                settings.profile.clone().unwrap_or_default(),
                "AWS profile (e.g. deverge-sandbox)",
            )
        });
        cx.subscribe(
            &profile_input,
            |this, _input, event: &TextInputEvent, cx| {
                if matches!(event, TextInputEvent::Changed | TextInputEvent::Submitted) {
                    this.sync.persist(&this.app, cx);
                }
            },
        )
        .detach();

        let bucket_input = cx.new(|cx| {
            TextInput::new(
                cx,
                settings.bucket.clone().unwrap_or_default(),
                "Bucket name",
            )
        });
        cx.subscribe(&bucket_input, |this, _input, event: &TextInputEvent, cx| {
            if matches!(event, TextInputEvent::Changed | TextInputEvent::Submitted) {
                this.sync.persist(&this.app, cx);
            }
        })
        .detach();

        let endpoint_input = cx.new(|cx| {
            TextInput::new(
                cx,
                settings.endpoint.clone().unwrap_or_default(),
                "Custom endpoint (R2/MinIO/NAS) — leave blank for AWS",
            )
        });
        cx.subscribe(
            &endpoint_input,
            |this, _input, event: &TextInputEvent, cx| {
                if matches!(event, TextInputEvent::Changed | TextInputEvent::Submitted) {
                    this.sync.persist(&this.app, cx);
                }
            },
        )
        .detach();

        Self {
            profile_input,
            bucket_input,
            endpoint_input,
            region: settings.region.clone(),
            status: TestStatus::Idle,
        }
    }

    /// Persist the current config to `user_settings.sync`, preserving the
    /// device id (not user-editable).
    fn persist(&self, app: &WeakEntity<AppState>, cx: &mut Context<SettingsWindowState>) {
        let Some(app_entity) = app.upgrade() else {
            return;
        };
        let device_id = app_entity.read(cx).user_settings.sync.device_id.clone();
        let sync = SyncSettings {
            bucket: non_empty(&self.bucket_input.read(cx).text()),
            region: self.region.clone(),
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

    /// Run the connection test: validate access to the bucket and resolve its
    /// region, off the UI thread via the sync tokio bridge.
    fn test_connection(
        &mut self,
        app: WeakEntity<AppState>,
        cx: &mut Context<SettingsWindowState>,
    ) {
        let profile = self.profile_input.read(cx).text().trim().to_string();
        let bucket = self.bucket_input.read(cx).text().trim().to_string();
        let endpoint = non_empty(&self.endpoint_input.read(cx).text());
        if profile.is_empty() || bucket.is_empty() {
            self.status = TestStatus::Failed("Enter a profile and a bucket first.".to_string());
            cx.notify();
            return;
        }

        self.status = TestStatus::Testing;
        cx.notify();

        let config = S3Config {
            bucket_name: bucket,
            region: self
                .region
                .clone()
                .unwrap_or_else(|| "us-east-1".to_string()),
            profile,
            endpoint,
        };

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
                        this.sync.region = Some(region.clone());
                        this.sync.status = TestStatus::Ok(region);
                        this.sync.persist(&app, cx);
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

    pub(super) fn render(&self, cx: &mut Context<SettingsWindowState>) -> impl IntoElement {
        let region_label = self
            .region
            .clone()
            .unwrap_or_else(|| "— (resolved on test)".to_string());

        let (status_text, status_color) = match &self.status {
            TestStatus::Idle => (String::new(), theme().text_faint),
            TestStatus::Testing => ("Testing…".to_string(), theme().text_secondary),
            TestStatus::Ok(region) => (format!("✓ Connected — region {region}"), theme().success),
            TestStatus::Failed(msg) => (format!("✗ {msg}"), theme().danger),
        };

        let test_button = div()
            .id("sync-test-connection")
            .cursor_pointer()
            .px(px(12.0))
            .py(px(6.0))
            .rounded(px(6.0))
            .bg(theme().accent)
            .text_size(px(12.0))
            .text_color(theme().text_on_accent)
            .hover(|s| s.bg(theme().lavender))
            .child("Test connection")
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _event, _window, cx| {
                    cx.stop_propagation();
                    let app = this.app.clone();
                    this.sync.test_connection(app, cx);
                }),
            );

        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w(px(0.0))
            .overflow_hidden()
            .p(px(20.0))
            .gap(px(12.0))
            .child(section_title("Session Sync"))
            .child(section_note(
                "Push a session (its list row + Claude conversation) from this Mac \
                     and resume it on another, via an S3-compatible bucket. No \
                     credentials are stored — supply an AWS profile name; sync resolves \
                     it from ~/.aws/credentials. Keep the profile's session valid (SSO \
                     login / materialized keys).",
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
                        div()
                            .text_size(px(12.0))
                            .text_color(theme().text_secondary)
                            .child(region_label),
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
    }
}
