//! Native macOS app menu and About panel.
//!
//! Extracted from `src/main.rs` to keep it under the §7.7 size ratchet —
//! ARCHITECTURE.md §8 names this as one of the candidates. Self-contained:
//! the menu wiring and the About panel are the only things in here, and
//! neither touches `AppState`.

use gpui::*;

use tracing::warn;

use crate::{commands, keymap, About};

/// The menu bar, by command name (DEV-1264). Each item's label and action
/// come from its row in `crate::commands`, so a menu cannot reach an action
/// that has no name. `None` is a separator.
///
/// The "Go" items MUST stay menu items: the focused terminal swallows any
/// Cmd-combo it doesn't recognise (see terminal/keymap.rs), so their keymap
/// bindings never fire on their own. macOS dispatches menu key-equivalents
/// before the terminal sees the event, which is what makes Cmd+P /
/// Cmd+Shift+F / Cmd+Shift+P work.
pub(crate) const MENUS: &[(&str, &[Option<&str>])] = &[
    (
        "Allele",
        &[
            Some("allele.about"),
            None,
            Some("allele.open_settings"),
            None,
            Some("allele.quit"),
        ],
    ),
    (
        "View",
        &[
            Some("allele.toggle_sidebar"),
            Some("allele.toggle_active_only"),
            Some("allele.toggle_drawer"),
            None,
            Some("allele.open_scratch_pad"),
        ],
    ),
    (
        "Go",
        &[
            Some("allele.open_command_palette"),
            None,
            Some("allele.open_file_palette"),
            Some("allele.open_search"),
        ],
    ),
    ("Sessions", &[Some("allele.away")]),
    ("Debug", &[Some("allele.capture_ui")]),
];

/// The menu item for one entry of [`MENUS`], or `None` for a name missing
/// from the command table (a test keeps that from shipping).
fn menu_item(entry: Option<&str>) -> Option<MenuItem> {
    let Some(name) = entry else {
        return Some(MenuItem::separator());
    };
    let Some(command) = commands::find(name) else {
        warn!("menu: no command named {name}");
        return None;
    };
    Some(MenuItem::Action {
        name: command.title.into(),
        action: command.action(),
        os_action: None,
        checked: false,
    })
}

/// Install the native macOS app menu.
///
/// Without this, a focused Allele window shows whatever menu the previously
/// focused app left on screen, and standard shortcuts like ⌘Q are no-ops.
pub(crate) fn install_app_menu(cx: &mut App) {
    // NOTE: the Quit action is handled per-window (see the App::on_action
    // block inside main()) so it can check for running sessions first.
    cx.on_action(|_: &About, _cx| show_about_panel());

    // All key bindings come from the command table, with user overrides
    // from `~/.allele/keymap.json` merged on top.
    keymap::load(cx);

    cx.set_menus(
        MENUS
            .iter()
            .map(|(title, items)| Menu {
                name: (*title).into(),
                items: items.iter().filter_map(|e| menu_item(*e)).collect(),
            })
            .collect(),
    );
}

/// Open the standard macOS About panel, populated with app details and a
/// clickable link to the GitHub repo.
///
/// The options dictionary is created once and retained for the process
/// lifetime.  Re-creating Obj-C objects on every invocation caused a
/// crash when the panel was already visible and focused.
fn show_about_panel() {
    #[cfg(target_os = "macos")]
    unsafe {
        use cocoa::appkit::NSApp;
        use cocoa::base::{id, nil};
        use cocoa::foundation::NSString;
        use objc::{class, msg_send, sel, sel_impl};
        use std::sync::OnceLock;

        // Store the retained NSDictionary pointer as a usize so it is
        // Send+Sync.  Created exactly once; leaked intentionally.
        static OPTIONS: OnceLock<usize> = OnceLock::new();

        let options_ptr = *OPTIONS.get_or_init(|| {
            #[repr(C)]
            struct NSRange {
                location: usize,
                length: usize,
            }

            let name: id = NSString::alloc(nil).init_str("Allele");
            let version: id = NSString::alloc(nil)
                .init_str(concat!("Version ", env!("CARGO_PKG_VERSION")));
            let copyright: id = NSString::alloc(nil).init_str(
                "Claude Code session manager — APFS clone management for parallel variant workflows.",
            );

            const URL: &str = "https://github.com/devergehq/allele";
            const BODY: &str = "Claude Code session manager\nAPFS clone management for parallel variant workflows.\n\n";
            let credits_text = format!("{BODY}{URL}");

            let ns_credits_str: id = NSString::alloc(nil).init_str(&credits_text);
            let credits: id = msg_send![class!(NSMutableAttributedString), alloc];
            let credits: id = msg_send![credits, initWithString: ns_credits_str];

            let url_str: id = NSString::alloc(nil).init_str(URL);
            let url: id = msg_send![class!(NSURL), URLWithString: url_str];
            let link_key: id = NSString::alloc(nil).init_str("NSLink");
            let range = NSRange {
                location: BODY.len(),
                length: URL.len(),
            };
            let _: () = msg_send![credits, addAttribute: link_key value: url range: range];

            let icon_data: &[u8] = include_bytes!("../assets/icons/allele-icon-256.png");
            let ns_icon_data: id = msg_send![class!(NSData), dataWithBytes: icon_data.as_ptr() length: icon_data.len()];
            let icon_image: id = msg_send![class!(NSImage), alloc];
            let icon_image: id = msg_send![icon_image, initWithData: ns_icon_data];

            let keys: [id; 5] = [
                NSString::alloc(nil).init_str("ApplicationName"),
                NSString::alloc(nil).init_str("ApplicationVersion"),
                NSString::alloc(nil).init_str("Copyright"),
                NSString::alloc(nil).init_str("Credits"),
                NSString::alloc(nil).init_str("ApplicationIcon"),
            ];
            let vals: [id; 5] = [name, version, copyright, credits, icon_image];
            let options: id = msg_send![
                class!(NSDictionary),
                dictionaryWithObjects: vals.as_ptr()
                forKeys: keys.as_ptr()
                count: 5usize
            ];
            let _: () = msg_send![options, retain];
            options as usize
        });

        let options = options_ptr as id;
        let app = NSApp();
        let _: () = msg_send![app, activateIgnoringOtherApps: true];
        let _: () = msg_send![app, orderFrontStandardAboutPanelWithOptions: options];
    }
}

#[cfg(test)]
mod tests {
    use super::MENUS;

    /// Acceptance (DEV-1264): every action reachable from a menu has a named
    /// entry in the command table.
    #[test]
    fn every_menu_item_names_a_command() {
        for (menu, items) in MENUS {
            for name in items.iter().flatten() {
                assert!(
                    crate::commands::find(name).is_some(),
                    "menu {menu} names unknown command `{name}`"
                );
            }
        }
    }
}
