//! Golden fixtures (DEV-1254): every file in `fixtures/` parses into its type
//! and serialises back to the same JSON.
//!
//! The fixtures are what a non-Rust client is written against, so a change
//! that breaks one of these is a contract change, not a refactor.

use allele_contract::*;
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;

fn fixture(name: &str) -> Value {
    let path = format!("{}/fixtures/{name}.json", env!("CARGO_MANIFEST_DIR"));
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{path}: {e}"))
}

/// Parse `name` as `T`, re-encode it, and require the same JSON back.
fn round_trip<T: Serialize + DeserializeOwned>(name: &str) -> T {
    let golden = fixture(name);
    let parsed: T = serde_json::from_value(golden.clone())
        .unwrap_or_else(|e| panic!("{name} does not parse: {e}"));
    let again = serde_json::to_value(&parsed).expect("serialises");
    assert_eq!(again, golden, "{name} does not round-trip");
    // And the encoded form is one line, as the socket's framing requires.
    assert!(!serde_json::to_string(&parsed)
        .expect("encodes")
        .contains('\n'));
    parsed
}

#[test]
fn hello_round_trips() {
    let req: Request = round_trip("hello");
    let Request::Hello(hello) = req else {
        panic!("not a hello")
    };
    assert!(is_supported(&hello.contract));
    // `activity` is asked for, and parses, even though nothing grants it yet.
    assert_eq!(
        hello.capabilities,
        [Capability::Observe, Capability::Activity]
    );
}

#[test]
fn hello_result_round_trips() {
    let reply: Reply = round_trip("hello_result");
    assert!(
        matches!(reply, Reply::Hello(HelloResult { ref granted, .. }) if granted == &[Capability::Observe])
    );
}

#[test]
fn subscribe_round_trips() {
    let req: Request = round_trip("subscribe");
    let Request::Subscribe(sub) = req else {
        panic!("not a subscribe")
    };
    assert_eq!(sub.topics, [Topic::Sessions]);
    assert_eq!(sub.filter.and_then(|f| f.group).as_deref(), Some("review"));
}

#[test]
fn snapshot_round_trips() {
    let frame: Frame = round_trip("snapshot");
    let Frame::Snapshot { seq, sessions } = frame else {
        panic!("not a snapshot")
    };
    assert_eq!(seq, 0);
    assert_eq!(sessions.len(), 2);
    assert_eq!(sessions[1].input, Input::BlockedOnPrompt);
    assert_eq!(
        sessions[1].parent.as_deref(),
        Some(sessions[0].session_id.as_str())
    );
}

#[test]
fn every_delta_type_round_trips() {
    let cases: [(&str, fn(&Event) -> bool); 5] = [
        ("delta_session_added", |e| {
            matches!(e, Event::SessionAdded { .. })
        }),
        ("delta_session_status_changed", |e| {
            matches!(e, Event::SessionStatusChanged { .. })
        }),
        ("delta_session_removed", |e| {
            matches!(
                e,
                Event::SessionRemoved {
                    reason: RemovalReason::Discarded,
                    ..
                }
            )
        }),
        ("delta_dispatch_refused", |e| {
            matches!(
                e,
                Event::DispatchRefused(DispatchRefused {
                    code: Some(ErrorCode::DepthLimitExceeded),
                    ..
                })
            )
        }),
        ("delta_limits_changed", |e| {
            matches!(e, Event::LimitsChanged { .. })
        }),
    ];
    for (name, is_expected) in cases {
        let frame: Frame = round_trip(name);
        let Frame::Delta { event, .. } = frame else {
            panic!("{name} is not a delta")
        };
        assert!(is_expected(&event), "{name} parsed as {event:?}");
    }
}

#[test]
fn lagged_round_trips() {
    assert_eq!(round_trip::<Frame>("lagged"), Frame::Lagged);
}

#[test]
fn error_replies_round_trip() {
    for (name, code) in [
        ("error_unsupported_contract", ErrorCode::UnsupportedContract),
        (
            "error_capability_not_granted",
            ErrorCode::CapabilityNotGranted,
        ),
    ] {
        let reply: Reply = round_trip(name);
        assert!(
            matches!(reply, Reply::Error(ErrorReply { code: c, .. }) if c == code),
            "{name}"
        );
    }
}

#[test]
fn an_archived_status_round_trips() {
    let reply: Reply = round_trip("status_archived");
    let Reply::Status { session } = reply else {
        panic!("not a status")
    };
    assert_eq!(session.state, State::Archived);
    assert!(session.archive_ref.is_some() && session.archived_at.is_some());
}

/// Additive within a major: a newer Allele's unknown values and fields must
/// not break an older client.
#[test]
fn unknown_values_and_fields_parse_as_unknown() {
    let entry: SessionEntry = serde_json::from_str(
        r#"{"session_id":"s","state":"hibernating","input":"telepathy","brand_new_field":1}"#,
    )
    .expect("parses");
    assert_eq!(entry.state, State::Unknown);
    assert_eq!(entry.input, Input::Unknown);

    let frame: Frame =
        serde_json::from_str(r#"{"frame":"delta","seq":9,"event":"session.teleported"}"#)
            .expect("parses");
    assert_eq!(
        frame,
        Frame::Delta {
            seq: 9,
            event: Event::Unknown
        }
    );

    let frame: Frame =
        serde_json::from_str(r#"{"frame":"shutdown","reason":"quit"}"#).expect("parses");
    assert_eq!(frame, Frame::Unknown);

    let caps: Vec<Capability> = serde_json::from_str(r#"["observe","levitate"]"#).expect("parses");
    assert_eq!(caps, [Capability::Observe, Capability::Unknown]);
}

#[test]
fn only_major_version_one_is_supported() {
    assert!(is_supported("allele.ext/1"));
    assert!(is_supported("allele.ext/1.4"));
    assert!(!is_supported("allele.ext/2"));
    assert!(!is_supported("allele.ext/10"));
    assert!(!is_supported("allele.ext"));
    assert!(!is_supported(""));
}
