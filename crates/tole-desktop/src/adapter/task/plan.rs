//! The plan list, read from the two places an entry carries it.
//!
//! - `update_plan`: the intent's `input.entries`. The intent is recorded before
//!   core validates it, so the same rules as core's `normalize_plan` apply here
//!   (a non-empty `content`, `priority` and `status` from their enums, the
//!   defaults for a missing or non-string one). Input core would refuse is not
//!   a plan: nothing is published, and the intent settles as an error.
//! - `todo_write`: the settlement's `output`, the list core already normalised.
//!   Read leniently: an item without a text is skipped, an unknown status is
//!   `Pending`.
//!
//! Text is raw (the model cleans it). `update_plan` input is validated whole,
//! like core does (one bad entry refuses the plan, wherever it is), but at most
//! [`MAX_READ_ITEMS`] items are copied, so a hostile list cannot make the adapter
//! allocate without bound; the rest of a longer list is dropped, as the model
//! keeps far fewer anyway.

use serde_json::Value;
use tole_desktop_model::task::{PlanItem, PlanStatus};

/// Items read from one list; the model keeps fewer.
const MAX_READ_ITEMS: usize = 200;

fn status(v: &str) -> PlanStatus {
    match v {
        "completed" => PlanStatus::Completed,
        "in_progress" => PlanStatus::InProgress,
        _ => PlanStatus::Pending,
    }
}

fn text(item: &Value) -> Option<&str> {
    item.get("content").and_then(Value::as_str)
}

/// `update_plan` input: strict, like core. `None` when core would refuse it.
pub(super) fn from_input(input: Option<&Value>) -> Option<Vec<PlanItem>> {
    let entries = input?.get("entries")?.as_array()?;
    let mut items = Vec::new();
    for e in entries {
        let content = text(e).filter(|c| !c.trim().is_empty())?;
        let priority = e
            .get("priority")
            .and_then(Value::as_str)
            .unwrap_or("medium");
        let st = e.get("status").and_then(Value::as_str).unwrap_or("pending");
        if !matches!(priority, "high" | "medium" | "low")
            || !matches!(st, "pending" | "in_progress" | "completed")
        {
            return None;
        }
        if items.len() < MAX_READ_ITEMS {
            items.push(PlanItem {
                text: content.to_string(),
                status: status(st),
            });
        }
    }
    Some(items)
}

/// `todo_write` output: lenient. `None` when the output is not a list.
pub(super) fn from_output(output: Option<&Value>) -> Option<Vec<PlanItem>> {
    let list = output?.as_array()?;
    let item = |e: &Value| {
        Some(PlanItem {
            text: text(e)?.to_string(),
            status: status(e.get("status").and_then(Value::as_str).unwrap_or("")),
        })
    };
    Some(list.iter().take(MAX_READ_ITEMS).filter_map(item).collect())
}
