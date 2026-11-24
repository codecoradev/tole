//! The events and the scenes of the task view's render harness: a history, a
//! turn going on, a failed step, a long task, each as the model's own events.

use std::rc::Rc;

use tole_desktop_model::task::{ApprovalInfo, TaskEvent, UserKind};

use super::harness_task::{Calls, Scene};
use crate::driver::task::Preview;

// ---- the events of the scenes ----

pub(super) fn said(seq: u64, text: &str) -> TaskEvent {
    TaskEvent::UserMessage {
        seq,
        text: text.into(),
        images: 0,
        kind: UserKind::Prompt,
    }
}

fn answer(seq: u64, text: &str) -> TaskEvent {
    TaskEvent::AnswerEntry {
        seq,
        text: text.into(),
    }
}

fn started(seq: u64, tool: &str, target: &str) -> TaskEvent {
    let target = Some(target.to_owned());
    TaskEvent::ToolIntent {
        seq,
        id: format!("i{seq}"),
        tool: tool.into(),
        target,
    }
}

fn settled(seq: u64, of: u64, ok: bool) -> TaskEvent {
    TaskEvent::ToolSettled {
        seq,
        intent_id: format!("i{of}"),
        ok,
    }
}

fn delta(text: &str) -> TaskEvent {
    TaskEvent::TextDelta { text: text.into() }
}

const PROMPT: &str = "Put my invoices into folders by month. Keep the originals.";
const LEAD_IN: &str =
    "I will look through the folder first, then show you a plan before I change anything.";
const PLAN: &str = "I found 36 invoices with a clear date and 2 without one. My plan:\n\n- Make 6 folders, one for each month from June to November.\n- Copy each invoice into the folder for its month. The originals stay where they are.\n- Leave the 2 invoices without a date in place and tell you which ones.\n\nCopying files is done by running a program, so Tole will ask you before it starts.";
const REPORT: &str = "## What I did\n\nI read **38 files** in `Invoices` and found a date in 36 of them.\n\n1. `2026-06-invoice-01.pdf` and 17 more are from June.\n2. The rest are from July to November.\n\n> Nothing was copied yet.\n\n```\nls Invoices | wc -l\n38\n```\n\nSee [the notes](https://example.com/notes) for how dates are read.";

/// The history of a task with `n` exchanges: a question, two reads, an answer.
fn long(n: u64) -> Vec<TaskEvent> {
    let mut events = Vec::new();
    for k in 0..n {
        let s = 10 * k;
        events.push(said(
            s + 1,
            &format!("Question number {k}: what is in folder {k}?"),
        ));
        events.push(started(s + 2, "read_file", &format!("folder-{k}/a.txt")));
        events.push(settled(s + 3, s + 2, true));
        events.push(started(s + 4, "read_file", &format!("folder-{k}/b.txt")));
        events.push(settled(s + 5, s + 4, true));
        let body = "A sentence about the folder. ".repeat(1 + (k % 7) as usize);
        events.push(answer(
            s + 6,
            &format!("Folder {k} holds two files. {body}"),
        ));
    }
    events
}

fn of_events(events: Vec<TaskEvent>) -> Preview {
    let task = Preview::new(false);
    task.feed(events);
    task
}

fn look_only_task() -> Preview {
    let task = Preview::new(true);
    task.feed([said(1, PROMPT), answer(2, "Here is what I found.")]);
    task
}

/// `n` more exchanges (a question and its answer), their entries numbered from `first`.
pub fn exchanges(first: u64, n: u64) -> Vec<TaskEvent> {
    (0..n)
        .flat_map(|k| {
            let seq = first + 2 * k;
            [
                said(seq, "And one more question?"),
                answer(seq + 1, "And one more answer."),
            ]
        })
        .collect()
}

/// Every scene worth a look.
pub fn scenes() -> Vec<Scene> {
    let busy = TaskEvent::Ready { busy: true };
    let calls = |extra: Option<TaskEvent>| {
        let mut e = vec![
            busy.clone(),
            said(1, PROMPT),
            delta(LEAD_IN),
            started(2, "read_file", "2026-06-invoice-01.pdf"),
            settled(3, 2, true),
            started(4, "read_file", "2026-06-invoice-02.pdf"),
            settled(5, 4, true),
        ];
        e.extend(extra);
        e
    };
    let mut working = Scene::of(
        "task-working",
        calls(Some(started(6, "read_file", "scan 0412.pdf"))),
    );
    working.open = vec![2];
    let mut technical = Scene::of(
        "task-technical-details",
        calls(Some(started(6, "run_command", "mkdir 2026-06"))),
    );
    (technical.open, technical.technical) = (vec![2], vec![2]);
    let mut writing = calls(Some(started(6, "read_file", "scan 0412.pdf")));
    writing.push(settled(7, 6, true));
    writing.extend(PLAN.split_inclusive(' ').map(delta));
    let mut failed = calls(None);
    failed.extend([
        started(6, "write_file", "2026-06/invoice-01.pdf"),
        settled(7, 6, false),
        started(8, "run_command", "cp a b"),
        TaskEvent::TurnEnded,
    ]);
    let mut failed = Scene::of("task-failed-step", failed);
    failed.open = vec![2];
    let mut done = Scene::of(
        "task-answer-markdown",
        vec![
            said(1, PROMPT),
            started(2, "read_file", "a.pdf"),
            settled(3, 2, true),
            answer(4, REPORT),
        ],
    );
    done.open = vec![];
    let long_end = Scene::of("task-long-at-the-end", long(500));
    let mut long_up = Scene::of("task-long-scrolled-up", long(500));
    long_up.scroll = Some(60_000.0);
    let mut opening = Scene::of("task-opening", vec![]);
    opening.task = None;
    let ask = TaskEvent::ApprovalRequested(ApprovalInfo {
        id: "a1".into(),
        tool: "run_command".into(),
        summary: Some("Run a program to copy files".into()),
    });
    let mut waiting = calls(None);
    waiting.push(ask);
    let waiting = Scene::of("task-waiting-approval", waiting);
    let offline = Scene::of(
        "task-offline",
        vec![said(1, PROMPT), TaskEvent::ConnectionLost],
    );
    let mut look_only = Scene::of("task-look-only", vec![]);
    look_only.task = Some(Rc::new(look_only_task()));
    let reply = || vec![said(1, PROMPT), answer(2, "Here is what I found.")];
    let mut tabs = Scene::of("task-tabs", reply());
    tabs.others = vec![
        (
            "20261123-2",
            Rc::new(of_events(vec![said(1, "Weekly report")])),
        ),
        ("20261123-3", Rc::new(of_events(vec![]))),
    ];
    let mut typed = Scene::of("task-composer-text", reply());
    typed.typed = Some("Also keep the scans\nin a folder of their own.");
    vec![
        working,
        technical,
        Scene::of("task-thinking", vec![busy.clone(), said(1, PROMPT)]),
        Scene::of("task-writing", writing),
        failed,
        done,
        Scene::of("task-empty", vec![]),
        long_end,
        long_up,
        opening,
        waiting,
        offline,
        look_only,
        tabs,
        typed,
    ]
}
