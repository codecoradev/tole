//! The approval card on its own, one PNG per state (`approvals-card-*.png`),
//! and the checks that drive it through the real window (render harness only).
//!
//! The requests are built the way the server sends them and the cards by the
//! model, so what is drawn is what the model decides. The host is a bare view
//! that keeps the answers it was given, which is how a test sees which button
//! the keyboard reached and what it sent.

use std::fs;
use std::io::Write as _;
use std::path::Path;

use super::super::adapter::offscreen::Offscreen;
use super::super::adapter::*;
use super::super::harness::{Rendered, WindowSize};
use super::{approval_card, CardHost, StopFocus};
use tole_desktop_model::approvals::{
    page_stops, ActionSummary, ApprovalCard, ApprovalRequest, CardState, Decision, Outcome,
    PageStop, Wait,
};
use tole_desktop_model::tokens::{font, size, DARK};

/// The board the cards are compared with: the detail pane of S5 and the card
/// of S4.
const BOARD: &str = "s5-approvals.png (detail pane), s4-allow.png (card)";
/// The clock of every scene, in milliseconds.
pub const NOW: u64 = 1_000_000_000;
const PANE: WindowSize = WindowSize {
    width: 760.0,
    height: 780.0,
};

/// A request as the server lists it: 14 minutes left of 15.
pub fn request(
    id: &str,
    kind: &str,
    target: &str,
    effect: &str,
    inside: bool,
    preview: &[&str],
) -> ApprovalRequest {
    let mut r = ApprovalRequest::new(id, "write_file", "pending");
    r.session_id = Some("sess-1".into());
    r.created_at_ms = Some(NOW - 60_000);
    r.expires_in_secs = Some(900);
    r.summary = Some(ActionSummary {
        kind: kind.into(),
        target: target.into(),
        effect: effect.into(),
        inside_workspace: inside,
        target_truncated: false,
        preview: preview.iter().map(|l| (*l).to_owned()).collect(),
        preview_truncated: preview.iter().any(|l| l.starts_with("\u{2026} (")),
    });
    r
}

pub fn card_of(r: &ApprovalRequest, state: CardState, now: u64) -> ApprovalCard {
    let wait = Wait::new(r.created_at_ms, r.expires_in_secs, NOW);
    ApprovalCard::build(r, state, wait, now)
}

/// The five lines of the report of board S5, and the server's marker for nine more.
pub fn create_report() -> ApprovalRequest {
    request(
        "apr-1",
        "create_file",
        "Weekly report.txt",
        "adds",
        true,
        &[
            "+ Weekly report, 17 to 21 November",
            "+ ",
            "+ 1. Invoices: 36 sorted into monthly folders.",
            "+ 2. Meeting notes: 4 summaries written.",
            "+ 3. Next week: review the 2 invoices without a date.",
            "+ \u{2026} (9 more lines, see the full description) \u{2026}",
        ],
    )
}

pub fn replace_notes() -> ApprovalRequest {
    request(
        "apr-2",
        "replace_file",
        "Meeting summary.txt",
        "replaces_no_backup",
        true,
        &[
            "- Meeting on Monday",
            "+ Meeting on Tuesday",
            "  Notes follow.",
        ],
    )
}

pub fn delete_invoice() -> ApprovalRequest {
    let mut r = request(
        "apr-3",
        "delete_file",
        "Old invoice 2025-03.pdf",
        "cannot_be_undone",
        true,
        &[],
    );
    r.child_node = None;
    r
}

pub fn run_command() -> ApprovalRequest {
    let cmd = "cp -r invoices/ archive/2025 && echo done";
    request("apr-4", "run_program", cmd, "runs_program", true, &[cmd])
}

pub fn outside_folder() -> ApprovalRequest {
    request(
        "apr-5",
        "create_file",
        "../../etc/hosts",
        "adds",
        false,
        &["+ 127.0.0.1 example.test"],
    )
}

/// A target of 1,100 characters with no space to break at (the display bound).
pub fn long_target() -> ApprovalRequest {
    let target = "x".repeat(1100);
    let mut r = request(
        "apr-7",
        "create_file",
        &target,
        "adds",
        true,
        &["+ short line"],
    );
    r.description = Some(format!("path: {target}"));
    r
}

/// A target with a right-to-left override, a bell, a line break and a
/// zero-width space: what a hostile path or command could carry.
pub fn hostile_target() -> ApprovalRequest {
    let target = "invoice\u{202e}fdp.exe\u{7}\nsecond line\u{200b}";
    let cmd = "echo ok \u{202e}; rm -rf ~ \u{2066}";
    let mut r = request("apr-8", "run_program", target, "runs_program", true, &[cmd]);
    r.description = Some(cmd.to_owned());
    r
}

/// A tool the server could not describe: no summary at all.
pub fn unknown_tool() -> ApprovalRequest {
    let mut r = ApprovalRequest::new("apr-6", "frobnicate", "pending");
    r.description = Some("frobnicate --everything".into());
    r.created_at_ms = Some(NOW - 60_000);
    r.expires_in_secs = Some(900);
    r
}

/// A request of a helper Tole started, and a notification that was sent.
pub fn from_helper() -> ApprovalRequest {
    let mut r = run_command();
    r.id = "dap-7".into();
    r.session_id = None;
    r.child_node = Some("researcher".into());
    r.fingerprint = Some("1a2b3c4d".into());
    r.notified = vec!["gateway".into()];
    r
}

/// One image: the card, and what was done before it was taken.
pub struct Scene {
    pub name: &'static str,
    pub keys: &'static str,
    pub card: ApprovalCard,
    pub details_open: bool,
}

fn scene(name: &'static str, card: ApprovalCard) -> Scene {
    Scene {
        name,
        keys: "",
        card,
        details_open: false,
    }
}

pub fn scenes() -> Vec<Scene> {
    let waiting = |r: &ApprovalRequest| card_of(r, CardState::Waiting, NOW);
    let report = create_report();
    let mut focused = scene("approvals-card-allow-focused", waiting(&report));
    focused.keys = "tab tab"; // the two buttons come first: this is Allow once
    let mut details = scene("approvals-card-details", waiting(&delete_invoice()));
    details.details_open = true;
    let mut long_details = scene("approvals-card-long-details", waiting(&long_target()));
    long_details.details_open = true;
    let mut approved = report.clone();
    approved.status = "approved".into();
    vec![
        scene("approvals-card-create", waiting(&report)),
        focused,
        scene("approvals-card-replace", waiting(&replace_notes())),
        scene("approvals-card-delete", waiting(&delete_invoice())),
        details,
        scene("approvals-card-command", waiting(&run_command())),
        scene("approvals-card-outside", waiting(&outside_folder())),
        scene("approvals-card-unknown", waiting(&unknown_tool())),
        scene("approvals-card-long-target", waiting(&long_target())),
        scene("approvals-card-hostile-target", waiting(&hostile_target())),
        long_details,
        scene("approvals-card-helper", waiting(&from_helper())),
        scene(
            "approvals-card-expired",
            card_of(&report, CardState::Waiting, NOW + 901_000),
        ),
        scene(
            "approvals-card-answering",
            card_of(&report, CardState::Answering(Decision::AllowOnce), NOW),
        ),
        scene(
            "approvals-card-allowed",
            card_of(&approved, CardState::Decided(Outcome::Allowed), NOW),
        ),
    ]
}

/// A bare host of one card: it keeps the answers it was given.
pub struct Gallery {
    card: ApprovalCard,
    details_open: bool,
    focus: StopFocus,
    pub answered: Vec<(String, Decision)>,
}

impl Gallery {
    fn new(card: ApprovalCard, details_open: bool) -> Self {
        Gallery {
            card,
            details_open,
            focus: StopFocus::default(),
            answered: Vec::new(),
        }
    }

    fn stops(&self) -> Vec<PageStop> {
        page_stops(&[], Some(&self.card))
    }

    fn answer(&mut self, id: &str, decision: Decision, cx: &mut Context<Self>) {
        self.answered.push((id.to_owned(), decision));
        cx.notify();
    }

    fn toggle(&mut self, _id: &str, cx: &mut Context<Self>) {
        self.details_open = !self.details_open;
        cx.notify();
    }

    /// The control that has focus (for the checks).
    pub fn focused(&self, window: &Window) -> Option<PageStop> {
        self.focus.focused(&self.stops(), window)
    }
}

impl Render for Gallery {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = DARK;
        let stops = self.stops();
        self.focus.sync(&stops, cx);
        let host = CardHost {
            answer: Gallery::answer,
            toggle_details: Gallery::toggle,
        };
        let card = approval_card(&self.card, self.details_open, &self.focus, host, cx);
        div()
            .size_full()
            .p(len(size::PADDING))
            .bg(color(p.background))
            .text_color(color(p.text_secondary))
            .font_family(font::FAMILY_UI)
            .text_size(len(font::BODY))
            .child(card)
    }
}

fn open(card: ApprovalCard, details_open: bool) -> Result<Offscreen<Gallery>, String> {
    Offscreen::open(PANE.width, PANE.height, move |_cx| {
        Gallery::new(card, details_open)
    })
}

/// Renders every scene into `dir` (which must exist) and adds a line for each
/// to `index.tsv`, when there is one.
pub fn render_scenes(dir: &Path) -> Result<Vec<Rendered>, String> {
    let mut done = Vec::new();
    for scene in scenes() {
        let at = |what: &str, err: String| format!("{}: {what}: {err}", scene.name);
        let mut offscreen = open(scene.card, scene.details_open).map_err(|e| at("open", e))?;
        for key in scene.keys.split_whitespace() {
            offscreen.press(key).map_err(|e| at("press", e))?;
        }
        let path = dir.join(format!("{}.png", scene.name));
        let (width, height, rgba) = offscreen.write_png(&path).map_err(|e| at("write", e))?;
        let bytes = fs::metadata(&path)
            .map_err(|e| at("stat", e.to_string()))?
            .len();
        done.push(Rendered {
            name: scene.name,
            board: BOARD,
            keys: scene.keys,
            path,
            width,
            height,
            bytes,
            rgba,
        });
    }
    let index = dir.join("index.tsv");
    if index.exists() {
        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(&index)
            .map_err(|e| format!("{}: {e}", index.display()))?;
        for r in &done {
            let line = format!(
                "{}.png\t{}\t{}\t{}\t{}\t{}",
                r.name, r.board, r.keys, r.width, r.height, r.bytes
            );
            writeln!(file, "{line}").map_err(|e| format!("{}: {e}", index.display()))?;
        }
    }
    Ok(done)
}

/// The longest run of device pixels, along one row, that are exactly the
/// colour of the amber button: a button is a wide block, text is not.
#[cfg(test)]
pub(super) fn amber_run_of(width: u32, rgba: &[u8]) -> usize {
    let want = [DARK.amber.r(), DARK.amber.g(), DARK.amber.b(), 255];
    let mut best = 0;
    for row in rgba.chunks_exact(width as usize * 4) {
        let mut run = 0;
        for px in row.chunks_exact(4) {
            run = if px == want { run + 1 } else { 0 };
            best = best.max(run);
        }
    }
    best
}

#[cfg(test)]
mod tests;
