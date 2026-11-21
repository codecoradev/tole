//! Tests of the card: wording, care level, buttons.

use super::fixtures::{req, T0};
use super::*;

fn card(r: &ApprovalRequest, state: CardState) -> ApprovalCard {
    ApprovalCard::build(r, state, Wait::new(Some(T0), Some(900), T0), T0)
}

#[test]
fn a_card_is_worded_by_the_kind_and_the_effect_codes() {
    let c = card(&req("a", "s", "create_file", "adds"), CardState::Waiting);
    assert_eq!(c.title, "Tole wants to create a file");
    assert_eq!(c.effect_line, "Adds a new file. Nothing is replaced.");
    assert_eq!(
        (c.subject.as_str(), c.care),
        ("Weekly report.txt", Care::Notice)
    );
    assert!(c.warning.is_some() && c.answerable());
    assert_eq!(c.preview.added_total(), 2);
    assert_eq!(
        c.waits_line.as_deref(),
        Some("15 more minutes, then it counts as Don't allow")
    );
    let c = card(
        &req("a", "s", "delete_file", "cannot_be_undone"),
        CardState::Waiting,
    );
    assert_eq!((c.care, c.notes.len(), c.warning), (Care::Danger, 2, None));
}

#[test]
fn an_unknown_kind_or_effect_is_never_worded_softer() {
    for (kind, effect) in [("teleport", "adds"), ("create_file", "harmless"), ("", "")] {
        let c = card(&req("a", "s", kind, effect), CardState::Waiting);
        assert_eq!(c.care, Care::Danger, "{kind} {effect}");
    }
    let c = card(&req("a", "s", "teleport", "adds"), CardState::Waiting);
    assert_eq!(
        (c.wording, c.title.as_str()),
        (Wording::Unknown, strings::title(Wording::Unknown))
    );
    assert!(
        c.details.iter().any(|d| d.value == "teleport"),
        "the raw code stays visible"
    );
    // A row with no summary at all is the unknown tool, with its raw text.
    let mut bare = ApprovalRequest::new("dap-1", "frobnicate", "pending");
    bare.description = Some("run frobnicate\nsecond line".into());
    let c = card(&bare, CardState::Waiting);
    assert_eq!(
        (c.wording, c.care, c.subject.as_str()),
        (Wording::Unknown, Care::Danger, "frobnicate")
    );
    assert_eq!(c.preview.lines.len(), 2);
    assert_eq!(c.origin.as_deref(), Some("From a helper Tole started"));
    let mut r = req("a", "s", "replace_file", "replaces_no_backup");
    r.summary.as_mut().unwrap().inside_workspace = false;
    let c = card(&r, CardState::Waiting);
    assert_eq!((c.wording, c.care), (Wording::ReplaceFile, Care::Danger));
    assert!(c.outside_folder && c.where_line.starts_with("Outside"));
}

#[test]
fn the_risk_tier_can_only_make_a_card_more_careful() {
    let mut r = req("a", "s", "create_file", "adds");
    r.risk = Some("Destructive".into());
    assert_eq!(card(&r, CardState::Waiting).care, Care::Danger);
    r.risk = Some("Write".into());
    let rows = card(&r, CardState::Waiting).details;
    assert_eq!(
        (rows[2].value.as_str(), rows[3].value.as_str()),
        ("Changes things (Write)", strings::NOT_REPORTED)
    );
    assert_eq!(card(&r, CardState::Waiting).care, Care::Notice);
}

#[test]
fn no_card_ever_offers_always_allow() {
    assert_eq!(BUTTONS, [Button::DontAllow, Button::AllowOnce]);
    for button in BUTTONS {
        let word = strings::button_label(button).to_lowercase();
        assert!(!word.contains("always") && !word.contains("forever"));
    }
    let kinds = [
        "create_file",
        "replace_file",
        "delete_file",
        "run_program",
        "use_connected_tool",
        "x",
    ];
    for kind in kinds {
        for risk in ["ReadOnly", "Write", "Destructive", "weird"] {
            let mut r = req("a", "s", kind, "cannot_be_undone");
            r.risk = Some(risk.into());
            let text = format!("{:?}", card(&r, CardState::Waiting)).to_lowercase();
            assert!(!text.contains("always allow") && !text.contains("permanent allow"));
        }
    }
    assert!(!strings::FINE_PRINT.contains("permanent"));
    assert!(strings::FINE_PRINT.contains("Allow once covers this request"));
}

#[test]
fn the_outside_folder_card_says_at_least_as_much_as_the_inside_one() {
    for (kind, effect) in [
        ("create_file", "adds"),
        ("replace_file", "replaces_no_backup"),
        ("delete_file", "cannot_be_undone"),
    ] {
        let mut r = req("a", "s", kind, effect);
        let inside = card(&r, CardState::Waiting);
        r.summary.as_mut().unwrap().inside_workspace = false;
        let out = card(&r, CardState::Waiting);
        let verb = kind.split('_').next().unwrap();
        assert!(
            out.title.contains(verb) && out.title.contains("outside"),
            "{}",
            out.title
        );
        assert_eq!(out.warning, inside.warning, "{kind}");
        assert!(out.notes.len() >= inside.notes.len() && out.care == Care::Danger);
        assert!(out.where_line.to_lowercase().contains("outside"));
    }
}

#[test]
fn a_mismatched_kind_and_effect_reads_as_the_most_careful_wording() {
    let ok = card(
        &req("a", "s", "run_program", "runs_program"),
        CardState::Waiting,
    );
    assert_eq!(ok.effect_line, strings::effect_line("runs_program"));
    for (kind, effect) in [
        ("run_program", "adds"),
        ("create_file", "cannot_be_undone"),
        ("delete_file", "adds"),
    ] {
        let c = card(&req("a", "s", kind, effect), CardState::Waiting);
        assert_eq!(c.care, Care::Danger, "{kind} {effect}");
        assert_ne!(
            c.effect_line,
            strings::effect_line(effect),
            "{kind} {effect}"
        );
        assert!(
            !c.effect_line.contains("Nothing is replaced"),
            "{kind} {effect}"
        );
    }
}

#[test]
fn a_card_is_answerable_only_while_pending_and_in_time() {
    let at = |r: &ApprovalRequest, now| {
        let wait = Wait::new(r.created_at_ms, r.expires_in_secs, T0);
        ApprovalCard::build(r, CardState::Waiting, wait, now)
    };
    let r = req("a", "s", "create_file", "adds");
    assert!(at(&r, T0).answerable() && at(&r, T0 + 899_999).answerable());
    assert!(!at(&r, T0 + 900_000).answerable() && !at(&r, T0 + 901_000).answerable());
    for status in ["approved", "denied", "expired", "weird", ""] {
        let mut d = req("a", "s", "create_file", "adds");
        d.status = status.into();
        assert!(!at(&d, T0).answerable(), "{status:?}");
    }
    // No expiry and no creation time in the row: it counts from the first sight.
    let mut bare = req("a", "s", "create_file", "adds");
    (bare.created_at_ms, bare.expires_in_secs) = (None, None);
    assert!(at(&bare, T0).answerable() && !at(&bare, T0 + 900_000).answerable());
}

#[test]
fn the_servers_answer_wins_over_a_local_state_and_a_decided_card_has_no_waiting_words() {
    let mut r = req("a", "s", "create_file", "adds");
    r.status = "approved".into();
    for state in [
        CardState::Waiting,
        CardState::Answering(Decision::DontAllow),
        CardState::Decided(Outcome::Expired),
    ] {
        let c = card(&r, state);
        assert_eq!(c.state, CardState::Decided(Outcome::Allowed), "{state:?}");
        assert!(!c.answerable());
        assert_eq!((c.waits_line.as_ref(), c.left_short.as_ref()), (None, None));
        assert_eq!(c.warning, None, "nothing to decide before any more");
    }
    r.status = "pending".into();
    let c = card(&r, CardState::Waiting);
    assert!(c.waits_line.is_some() && c.left_short.is_some() && c.warning.is_some());
}
