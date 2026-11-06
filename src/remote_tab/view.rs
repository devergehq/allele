//! The view inside a remote tab: a header naming the session and its host,
//! then the host's grid at the host's size.
//!
//! ```text
//!   ┌ ▭ fix the build · on Desk ·  Live            [Fit to this window] [×] ┐
//!   │ ┌───────── host grid, host columns × host rows ─────────┐            │
//!   │ │                                                       │ letterbox  │
//!   │ └───────────────────────────────────────────────────────┘            │
//!   └──────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! A window smaller than the host grid scrolls it (both axes; the grid is
//! never reflowed). The wheel pans the grid first and only scrolls into the
//! host's history once the top edge is in view, so neither gesture hides the
//! other.

use std::sync::Arc;

use gpui::prelude::FluentBuilder as _;
use gpui::*;

use crate::icon::{icon, name as icons};
use crate::remote::{RemoteScreen, ScreenPhase};
use crate::terminal::grid_source::{GridSource, GridView as _};
use crate::terminal::keymap::{self, AppAction, KeymapConfig};
use crate::terminal::TerminalGridElement;
use crate::theme::{theme, with_alpha};

/// How long an input notice stays up.
const NOTICE_FOR: std::time::Duration = std::time::Duration::from_secs(6);

/// What the tab asks its owner to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RemoteTabEvent {
    Close,
    /// Resize the host's terminal to this window: `columns × lines`.
    FitToWindow {
        columns: u16,
        lines: u16,
    },
}

pub struct RemoteSessionView {
    screen: RemoteScreen,
    host_name: SharedString,
    label: SharedString,
    pub focus_handle: FocusHandle,
    scroll: ScrollHandle,
    font_size: f32,
    cell_width: f32,
    cell_height: f32,
    /// Sub-line wheel travel not yet turned into a scrollback line.
    wheel_px: f32,
    /// Whether the host said this Mac holds `control`: may type and resize.
    /// The host checks again on every keystroke.
    can_fit: bool,
    keymap: KeymapConfig,
    /// "View only" and other input problems, shown under the header until
    /// the next accepted keystroke or the notice times out.
    notice: Option<(SharedString, std::time::Instant)>,
    _redraw: Task<()>,
}

impl EventEmitter<RemoteTabEvent> for RemoteSessionView {}

fn mono() -> Font {
    Font {
        family: crate::theme::FONT_MONO.into(),
        weight: FontWeight::NORMAL,
        style: FontStyle::Normal,
        features: FontFeatures::disable_ligatures(),
        fallbacks: None,
    }
}

impl RemoteSessionView {
    pub fn new(
        screen: RemoteScreen,
        host_name: String,
        label: String,
        font_size: f32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let (w, h) = TerminalGridElement::measure_cell(window, &mono(), px(font_size));
        let mut ticks = screen.ticks();
        // Redraw on every frame the host sends and every phase change. The
        // channel is tokio's, which any executor can await; its wakes come
        // from the service's runtime threads. GPUI's test scheduler rejects
        // wakes from foreign threads, so handler tests run without it.
        let redraw = if cfg!(test) {
            Task::ready(())
        } else {
            cx.spawn(async move |this, cx| {
                while ticks.changed().await.is_ok() {
                    if this.update(cx, |_, cx| cx.notify()).is_err() {
                        break;
                    }
                }
            })
        };
        Self {
            screen,
            host_name: host_name.into(),
            label: label.into(),
            focus_handle: cx.focus_handle(),
            scroll: ScrollHandle::new(),
            font_size,
            cell_width: f32::from(w),
            cell_height: f32::from(h),
            wheel_px: 0.0,
            can_fit: false,
            keymap: KeymapConfig::default(),
            notice: None,
            _redraw: redraw,
        }
    }

    /// Keep the header in step with the host's feed (a rename, a re-pair
    /// under a new name) and with this peer's permissions.
    pub fn set_titles(&mut self, host_name: &str, label: &str, can_fit: bool) {
        if self.host_name.as_ref() != host_name {
            self.host_name = host_name.to_string().into();
        }
        if self.label.as_ref() != label {
            self.label = label.to_string().into();
        }
        self.can_fit = can_fit;
    }

    #[cfg(test)]
    pub fn host_name(&self) -> &str {
        &self.host_name
    }

    /// How many host columns and lines fit in the grid's viewport now.
    fn window_size(&self) -> Option<(u16, u16)> {
        let bounds = self.scroll.bounds();
        let columns = (f32::from(bounds.size.width) / self.cell_width).floor();
        let lines = (f32::from(bounds.size.height) / self.cell_height).floor();
        (columns >= 2.0 && lines >= 1.0).then_some((columns as u16, lines as u16))
    }

    fn show_notice(&mut self, text: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.notice = Some((text.into(), std::time::Instant::now()));
        // Redraw once it has expired, so it goes away untouched.
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(NOTICE_FOR).await;
            let _ = this.update(cx, |_, cx| cx.notify());
        })
        .detach();
        cx.notify();
    }

    fn send(&mut self, sent: crate::errors::Result<()>, cx: &mut Context<Self>) {
        if let Err(e) = sent {
            self.show_notice(e.to_string(), cx);
        }
    }

    /// Typing goes to the host session (DEV-1166), in the order the local
    /// terminal resolves keys: Allele's Cmd shortcuts first, then the keymap.
    /// Nothing typed here ever reaches a local PTY.
    fn on_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        let key = event.keystroke.key.as_str();
        let mods = &event.keystroke.modifiers;
        if mods.platform {
            match keymap::app_action(key, mods) {
                Some(AppAction::CloseSession) => cx.emit(RemoteTabEvent::Close),
                Some(AppAction::Paste) if self.may_type(cx) => {
                    if let Some(text) = cx.read_from_clipboard().and_then(|c| c.text()) {
                        let sent = self.screen.send_paste(&text);
                        self.send(sent, cx);
                    }
                }
                // No selection in a mirror: Cmd+C is Ctrl+C, as locally.
                Some(AppAction::Copy) if self.may_type(cx) => {
                    let sent = self.screen.send_input(&[0x03]);
                    self.send(sent, cx);
                }
                Some(AppAction::SendBytes(bytes)) if self.may_type(cx) => {
                    let sent = self.screen.send_input(bytes);
                    self.send(sent, cx);
                }
                // Other shortcuts act on this Mac, never on the host.
                _ => {}
            }
            cx.stop_propagation();
            return;
        }
        let key_char = event.keystroke.key_char.as_deref();
        let Some(bytes) = self.keymap.resolve(key, mods, key_char) else {
            return;
        };
        cx.stop_propagation();
        if self.may_type(cx) {
            let sent = self.screen.send_input(&bytes);
            self.send(sent, cx);
        }
    }

    /// False, with the view-only notice, when the host's hello said this Mac
    /// has no `control`. The host would refuse anyway; this says so at once.
    fn may_type(&mut self, cx: &mut Context<Self>) -> bool {
        if !self.can_fit {
            let text = format!(
                "View only — {} hasn't given this Mac control of its sessions",
                self.host_name
            );
            self.show_notice(text, cx);
        }
        self.can_fit
    }

    /// Wheel up pans until the grid's top edge is in view, then scrolls the
    /// host's history; wheel down leaves history first, then pans.
    fn on_wheel(&mut self, event: &ScrollWheelEvent, cx: &mut Context<Self>) {
        let delta = match event.delta {
            ScrollDelta::Pixels(d) => {
                if d.x.abs() > d.y.abs() {
                    return; // horizontal: the scroll container pans
                }
                self.wheel_px += f32::from(d.y);
                let whole = (self.wheel_px / self.cell_height).trunc();
                self.wheel_px -= whole * self.cell_height;
                whole as i32
            }
            ScrollDelta::Lines(d) => {
                self.wheel_px = 0.0;
                if d.x.abs() > d.y.abs() {
                    return;
                }
                d.y.round() as i32
            }
        };
        if delta == 0 {
            return;
        }
        let at_top = f32::from(self.scroll.offset().y) >= 0.0;
        let in_history = self
            .screen
            .grid()
            .map(|g| g.lock().display_offset() > 0)
            .unwrap_or(false);
        let into_history = (delta > 0 && at_top) || (delta < 0 && in_history);
        if into_history {
            self.screen.scroll_display(delta);
            cx.stop_propagation();
            cx.notify();
        }
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let phase = self.screen.phase();
        let (phase_text, phase_color) = match &phase {
            ScreenPhase::Attaching => ("Connecting…", theme().text_faint),
            ScreenPhase::Live if !self.can_fit => ("Live · view only", theme().success),
            ScreenPhase::Live => ("Live", theme().success),
            ScreenPhase::Unreachable => ("Unreachable", theme().warning),
            ScreenPhase::Unavailable(_) => ("Unavailable", theme().warning),
        };
        let can_fit = self.can_fit && phase == ScreenPhase::Live;
        let fit = div()
            .id("remote-fit")
            .px(px(8.0))
            .py(px(3.0))
            .rounded(px(4.0))
            .text_size(px(11.0))
            .border_1()
            .border_color(theme().border_default)
            .when(can_fit, |d| {
                d.text_color(theme().text_secondary)
                    .cursor_pointer()
                    .hover(|s| s.bg(theme().bg_hover))
                    .on_click(cx.listener(|this, _, _window, cx| {
                        if let Some((columns, lines)) = this.window_size() {
                            cx.emit(RemoteTabEvent::FitToWindow { columns, lines });
                        }
                    }))
            })
            .when(!can_fit, |d| d.text_color(theme().text_ghost))
            .tooltip({
                let text: SharedString = if self.can_fit {
                    "Resize the session on the other Mac to this window. Using it there puts it back."
                        .into()
                } else {
                    "View only: resizing and typing need control of the other Mac's sessions"
                        .into()
                };
                move |_window, cx| {
                    cx.new(|_| crate::SimpleTooltip { text: text.clone() })
                        .into()
                }
            })
            .child("Fit to this window");
        let close = div()
            .id("remote-close")
            .p(px(4.0))
            .rounded(px(4.0))
            .cursor_pointer()
            .hover(|s| s.bg(theme().bg_hover))
            .on_click(cx.listener(|_this, _, _window, cx| cx.emit(RemoteTabEvent::Close)))
            .tooltip(|_window, cx| {
                cx.new(|_| crate::SimpleTooltip {
                    text: "Close this tab. The session keeps running on the other Mac.".into(),
                })
                .into()
            })
            .child(icon(icons::X, 11.0, theme().text_muted));
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .px(px(12.0))
            .py(px(6.0))
            .border_b_1()
            .border_color(theme().border_subtle)
            .bg(theme().bg_surface)
            .child(icon(icons::MONITOR, 12.0, theme().text_muted))
            .child(
                div()
                    .min_w(px(0.0))
                    .overflow_hidden()
                    .text_ellipsis()
                    .whitespace_nowrap()
                    .text_size(px(13.0))
                    .text_color(theme().text_primary)
                    .child(self.label.clone()),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .text_size(px(12.0))
                    .text_color(theme().text_muted)
                    .child(format!("on {}", self.host_name)),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .text_size(px(11.0))
                    .text_color(phase_color)
                    .child(phase_text),
            )
            .child(div().flex_1())
            .child(fit)
            .child(close)
    }

    /// Over the last frame: why it is not live. Nothing while live.
    fn render_overlay(&self) -> Option<AnyElement> {
        let (title, detail): (SharedString, SharedString) = match self.screen.phase() {
            ScreenPhase::Live => return None,
            ScreenPhase::Attaching if self.screen.grid().is_none() => (
                format!("Connecting to {}…", self.host_name).into(),
                "".into(),
            ),
            ScreenPhase::Attaching => (
                format!("Reconnecting to {}…", self.host_name).into(),
                "Showing the last screen received.".into(),
            ),
            ScreenPhase::Unreachable => (
                format!("{} is unreachable", self.host_name).into(),
                "Showing the last screen received. This tab reconnects on its own.".into(),
            ),
            ScreenPhase::Unavailable(message) => (
                format!("{} isn't showing this session", self.host_name).into(),
                message.into(),
            ),
        };
        Some(
            div()
                .absolute()
                .inset_0()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(px(6.0))
                .bg(with_alpha(theme().bg_base, 0.72))
                .child(
                    div()
                        .text_size(px(14.0))
                        .text_color(theme().text_primary)
                        .child(title),
                )
                .when(!detail.is_empty(), |d| {
                    d.child(
                        div()
                            .text_size(px(12.0))
                            .text_color(theme().text_muted)
                            .child(detail),
                    )
                })
                .into_any_element(),
        )
    }
}

impl Focusable for RemoteSessionView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for RemoteSessionView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let live = self.screen.phase() == ScreenPhase::Live;
        if let Some(refusal) = self.screen.take_refusal() {
            self.show_notice(
                format!("{} refused the input: {refusal}", self.host_name),
                cx,
            );
        }
        let notice = self
            .notice
            .as_ref()
            .filter(|(_, at)| at.elapsed() < NOTICE_FOR)
            .map(|(text, _)| {
                div()
                    .px(px(12.0))
                    .py(px(4.0))
                    .bg(theme().tint_warning_soft)
                    .text_size(px(12.0))
                    .text_color(theme().text_secondary)
                    .child(text.clone())
            });
        let grid = self.screen.grid().map(|grid| {
            let source: Arc<dyn GridSource> = grid;
            div().flex_shrink_0().bg(theme().bg_base).child(
                TerminalGridElement::new(
                    source,
                    mono(),
                    px(self.font_size),
                    px(self.cell_width),
                    px(self.cell_height),
                )
                .cursor_visible(live),
            )
        });
        let body = div()
            .id("remote-screen")
            .flex_1()
            .min_h(px(0.0))
            .overflow_scroll()
            .track_scroll(&self.scroll)
            // The letterbox around a grid smaller than the window.
            .bg(theme().bg_sunken)
            .child(
                div()
                    .id("remote-screen-content")
                    .min_w_full()
                    .min_h_full()
                    .flex()
                    .flex_col()
                    .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, _w, cx| {
                        this.on_wheel(event, cx)
                    }))
                    .children(grid),
            );
        div()
            .id("remote-session")
            .track_focus(&self.focus_handle)
            .size_full()
            .flex()
            .flex_col()
            .bg(theme().bg_base)
            .on_key_down(
                cx.listener(|this, event: &KeyDownEvent, _window, cx| this.on_key(event, cx)),
            )
            .child(self.render_header(cx))
            .children(notice)
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.0))
                    .relative()
                    .flex()
                    .flex_col()
                    .child(body)
                    .children(self.render_overlay()),
            )
    }
}
