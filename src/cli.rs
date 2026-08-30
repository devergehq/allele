//! Argument handling for the `Allele` binary.
//!
//! Allele is a GUI app with two flag-driven side modes and **no command-line
//! interface**. Anything `main` did not recognise used to fall straight through
//! to a normal launch, so an agent typing `allele sessions status <id>` — having
//! mistaken the MCP tool `allele_sessions_status` for a shell command — opened a
//! whole second copy of the app.
//!
//! That is not merely cosmetic. A second instance loads and atomically rewrites
//! `~/.allele/state.json` (see [`crate::state::PersistedState::save`]), so the
//! ghost can clobber the live app's session list on a last-writer-wins race.
//!
//! The guard is deliberately narrow: it rejects positional arguments and unknown
//! flags, and stays silent about the arguments macOS itself injects. Refusing a
//! legitimate launch would turn an annoyance into an app that will not start.

use std::path::PathBuf;

/// What the process should do with the arguments it was given.
#[derive(Debug, PartialEq, Eq)]
pub enum Launch {
    /// Recognised invocation — carry on and open the app.
    Gui {
        /// Root for Allele's own data, from `--home` or `--sandbox`. `None`
        /// leaves the choice to `ALLELE_HOME` and then the real home, so an
        /// ordinary launch is unchanged.
        home: Option<PathBuf>,
        /// `--sandbox` was passed: seed the root with a throwaway project and
        /// mark the window, so nobody drives the wrong one.
        sandbox: bool,
    },
    /// Print [`USAGE`] and exit with this code. Zero for `--help`, non-zero for
    /// a malformed invocation, so a caller in a script can tell them apart.
    Usage { code: i32 },
}

pub const USAGE: &str = "\
Allele is a macOS app, not a command-line tool — it has no subcommands.

Usage:
  Allele                open the app
  Allele --mcp-serve    run the stdio MCP server (registered in ~/.claude.json)
  Allele --capture-ui   capture the running app's UI and print the image path
  Allele --home <dir>   use <dir> instead of your home for Allele's own data
  Allele --sandbox      run against ~/.allele-sandbox with a throwaway project
  Allele --help         show this message

--home and --sandbox exist so Allele can be tested in Allele. They redirect
state.json, the workspaces root, settings, hooks, and the MCP control socket,
so a second instance cannot disturb the live one. ALLELE_HOME does the same as
--home; the flag wins. Data belonging to other tools (~/.claude, installed
agent binaries) is never redirected — a sandbox drives your real agent.

To drive Allele from an agent, call the MCP tools rather than the shell:
  allele_projects_list      allele_sessions_create     allele_sessions_list
  allele_sessions_status    allele_sessions_interrupt  allele_sessions_discard

`allele sessions status <id>` is not a command. Before this guard existed it
opened a second copy of the app, which shares ~/.allele/state.json with the
first and can overwrite its session list.";

/// Classify the arguments following `argv[0]`.
///
/// Unknown flags and positional arguments are refused. macOS launch arguments
/// are tolerated in silence: LaunchServices hands bundled apps a
/// `-psn_<n>_<n>` process serial number, and Xcode and Instruments inject Cocoa
/// user-defaults overrides such as `-NSDocumentRevisionsDebugMode YES`, which
/// arrive as a flag plus a separate value.
pub fn classify<S: AsRef<str>>(args: &[S]) -> Launch {
    let mut home: Option<PathBuf> = None;
    let mut sandbox = false;
    let mut args = args.iter().map(AsRef::as_ref);
    while let Some(arg) = args.next() {
        match arg {
            "--mcp-serve" | "--capture-ui" => {}
            "--help" | "-h" => return Launch::Usage { code: 0 },
            "--sandbox" => sandbox = true,
            "--home" => match args.next() {
                // A bare `--home` would otherwise silently launch against the
                // real home, which is the one outcome the flag exists to avoid.
                Some(dir) if !dir.trim().is_empty() && !dir.starts_with('-') => {
                    home = Some(PathBuf::from(dir))
                }
                _ => return Launch::Usage { code: 2 },
            },
            a if a.starts_with("--home=") => {
                let dir = a.trim_start_matches("--home=");
                if dir.trim().is_empty() {
                    return Launch::Usage { code: 2 };
                }
                home = Some(PathBuf::from(dir));
            }
            a if a.starts_with("-psn_") => {}
            a if a.starts_with("-NS") || a.starts_with("-Apple") => {
                // Swallow the value that belongs to this flag, so it is not
                // mistaken for a positional argument on the next iteration.
                let _ = args.next();
            }
            _ => return Launch::Usage { code: 2 },
        }
    }
    // Refused rather than resolved by precedence: the two disagree about where
    // to run, and guessing means a chance of writing to the live tree.
    if sandbox && home.is_some() {
        return Launch::Usage { code: 2 };
    }
    Launch::Gui { home, sandbox }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The plain launch every existing test means.
    fn gui() -> Launch {
        Launch::Gui {
            home: None,
            sandbox: false,
        }
    }

    #[test]
    fn no_arguments_opens_the_app() {
        assert_eq!(classify::<&str>(&[]), gui());
    }

    #[test]
    fn known_modes_are_allowed_through() {
        assert_eq!(classify(&["--mcp-serve"]), gui());
        assert_eq!(classify(&["--capture-ui"]), gui());
    }

    #[test]
    fn subcommands_are_refused() {
        // The invocation that caused this guard to exist.
        assert_eq!(
            classify(&["sessions", "status", "6fd37adf"]),
            Launch::Usage { code: 2 }
        );
    }

    #[test]
    fn a_lone_positional_argument_is_refused() {
        assert_eq!(classify(&["status"]), Launch::Usage { code: 2 });
    }

    #[test]
    fn unknown_flags_are_refused() {
        assert_eq!(classify(&["--serve"]), Launch::Usage { code: 2 });
    }

    #[test]
    fn help_exits_zero() {
        assert_eq!(classify(&["--help"]), Launch::Usage { code: 0 });
        assert_eq!(classify(&["-h"]), Launch::Usage { code: 0 });
    }

    #[test]
    fn launch_services_process_serial_number_is_tolerated() {
        assert_eq!(classify(&["-psn_0_774221"]), gui());
    }

    #[test]
    fn cocoa_defaults_overrides_consume_their_value() {
        // `YES` must be eaten by the flag, not read as a positional argument.
        assert_eq!(classify(&["-NSDocumentRevisionsDebugMode", "YES"]), gui());
        assert_eq!(classify(&["-AppleLanguages", "(en)"]), gui());
    }

    #[test]
    fn a_bad_argument_after_a_good_one_is_still_refused() {
        assert_eq!(
            classify(&["-psn_0_774221", "sessions"]),
            Launch::Usage { code: 2 }
        );
    }

    #[test]
    fn home_flag_captures_its_value() {
        assert_eq!(
            classify(&["--home", "/tmp/alt"]),
            Launch::Gui {
                home: Some(PathBuf::from("/tmp/alt")),
                sandbox: false
            }
        );
        assert_eq!(
            classify(&["--home=/tmp/alt"]),
            Launch::Gui {
                home: Some(PathBuf::from("/tmp/alt")),
                sandbox: false
            }
        );
    }

    #[test]
    fn home_without_a_value_is_refused() {
        // Launching against the real home is exactly what this flag prevents,
        // so a missing value must not fall through to it.
        assert_eq!(classify(&["--home"]), Launch::Usage { code: 2 });
        assert_eq!(
            classify(&["--home", "--sandbox"]),
            Launch::Usage { code: 2 }
        );
        assert_eq!(classify(&["--home="]), Launch::Usage { code: 2 });
    }

    #[test]
    fn sandbox_sets_its_flag() {
        assert_eq!(
            classify(&["--sandbox"]),
            Launch::Gui {
                home: None,
                sandbox: true
            }
        );
    }

    #[test]
    fn sandbox_and_home_together_are_refused() {
        // They disagree about where to run; guessing risks the live tree.
        assert_eq!(
            classify(&["--sandbox", "--home", "/tmp/alt"]),
            Launch::Usage { code: 2 }
        );
    }

    #[test]
    fn a_home_value_is_not_mistaken_for_a_positional() {
        assert_eq!(
            classify(&["--home", "/tmp/alt", "--mcp-serve"]),
            Launch::Gui {
                home: Some(PathBuf::from("/tmp/alt")),
                sandbox: false
            }
        );
    }
}
