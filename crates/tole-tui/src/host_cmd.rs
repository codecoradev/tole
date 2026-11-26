//! Slash commands run against the host: what is said for each outcome, and
//! the image attach paths (`/image`, a pasted image reference) and `/copy`.

use crate::{clipboard, App, HostCmd, HostReply, ImageItem, Item, PickKind, TuiHost};

/// Run a slash command against the host (no turn is running: the app only
/// emits these while idle). Every outcome is a transcript note, and a
/// successful change marks the footer stale so the next loop turn re-reads it.
pub(crate) fn run_host_cmd(host: &dyn TuiHost, app: &mut App, cmd: HostCmd) {
    // What to say, decided before the command is handed over (the host
    // gets it by value and answers with data only).
    #[derive(Clone, Copy)]
    enum Cmd {
        PickModel,
        PickSession,
        Tree,
        Fork,
        New,
        Switch,
        Status,
        Change,
        Attach,
        Clear,
        Images,
        Copy,
        Export,
    }
    let (kind, ok_note) = match &cmd {
        HostCmd::PickModel => (Cmd::PickModel, String::new()),
        HostCmd::PickSession => (Cmd::PickSession, String::new()),
        HostCmd::PickTree => (Cmd::Tree, String::new()),
        HostCmd::Fork(_) => (Cmd::Fork, String::new()),
        HostCmd::New => (Cmd::New, String::new()),
        HostCmd::Switch(_) => (Cmd::Switch, String::new()),
        HostCmd::Status => (Cmd::Status, String::new()),
        HostCmd::SetModel(id) => (Cmd::Change, format!("model → {id}")),
        HostCmd::SetEffort(l) => (
            Cmd::Change,
            format!("thinking → {}", l.as_deref().unwrap_or("unset")),
        ),
        HostCmd::Compact(instructions) => {
            let extra = if instructions.is_some() {
                "; instructions are not supported yet and were ignored"
            } else {
                ""
            };
            (
                Cmd::Change,
                format!("compaction runs before your next prompt{extra}"),
            )
        }
        HostCmd::Name(n) => (Cmd::Change, format!("named {n}")),
        HostCmd::AttachImage(_) => (Cmd::Attach, String::new()),
        HostCmd::ClearImages => (Cmd::Clear, String::new()),
        HostCmd::Images => (Cmd::Images, String::new()),
        HostCmd::LastAnswer => (Cmd::Copy, String::new()),
        HostCmd::Export(_) => (Cmd::Export, String::new()),
    };
    let had = app.images.len();
    let done = |app: &mut App, msg: String| {
        app.footer_stale = true;
        app.note(&format!("({msg})"));
    };
    match (kind, host.apply(cmd)) {
        (Cmd::PickModel, Ok(HostReply::Models(ids))) if ids.is_empty() => {
            app.note("(the provider lists no models; use /model <id>)")
        }
        (Cmd::PickModel, Ok(HostReply::Models(ids))) => app.open_picker(
            PickKind::Model,
            ids.into_iter().map(|i| Item::new(i.clone(), i)).collect(),
        ),
        (Cmd::PickModel, Err(e)) => app.note(&format!("(models: {e}; use /model <id>)")),
        (Cmd::PickSession, Ok(HostReply::Sessions(v))) if v.is_empty() => {
            app.note("(no other sessions)")
        }
        (Cmd::PickSession, Ok(HostReply::Sessions(v))) => app.open_picker(
            PickKind::Session,
            v.into_iter().map(|s| Item::new(s.id, s.label)).collect(),
        ),
        (Cmd::PickSession, Err(e)) => app.note(&format!("(sessions: {e})")),
        (Cmd::Tree, Ok(HostReply::Sessions(v))) => {
            if v.len() <= 1 {
                app.note("(this session has no parent and no forks)");
            }
            let current = app.footer.session.clone();
            app.open_picker(
                PickKind::Tree,
                v.into_iter().map(|s| Item::new(s.id, s.label)).collect(),
            );
            if let Some(p) = app.picker.as_mut() {
                p.select(&current);
            }
        }
        (Cmd::Tree, Err(e)) => app.note(&format!("(tree: {e})")),
        (Cmd::Fork, Ok(HostReply::Session(id))) => done(app, format!("forked → {id}")),
        (Cmd::Fork, Err(e)) => app.note(&format!("(fork: {e})")),
        (Cmd::New, Ok(HostReply::Session(id))) => done(app, format!("new session {id}")),
        (Cmd::Switch, Ok(HostReply::Session(id))) => done(app, format!("resumed {id}")),
        (Cmd::Status, Ok(HostReply::Lines(lines))) => {
            for l in lines {
                app.note(&l);
            }
        }
        (Cmd::Change, Ok(HostReply::Done)) => done(app, ok_note),
        (Cmd::Attach, Ok(HostReply::Images(v))) => attached(app, v),
        (Cmd::Clear, Ok(HostReply::Images(v))) => {
            app.set_images(v);
            app.note(&format!("({had} queued image(s) dropped)"));
        }
        (Cmd::Images, Ok(HostReply::Images(v))) => {
            app.note(&format!(
                "({} image(s) queued for your next message)",
                v.len()
            ));
            for i in &v {
                app.note(&format!("  {}", i.label()));
            }
            app.set_images(v);
        }
        (Cmd::Copy, Ok(HostReply::Answer(answer))) => copy_answer(app, answer),
        (Cmd::Copy, Err(e)) => app.note(&format!("(/copy: {e})")),
        (Cmd::Export, Ok(HostReply::Path(p))) => app.note(&format!("(transcript written to {p})")),
        (Cmd::Export, Err(e)) => app.note(&format!("(/export: {e})")),
        // The host's own error (jail, size cap, not an image), as chat shows it.
        (Cmd::Attach | Cmd::Clear | Cmd::Images, Err(e)) => app.note(&format!("(/image: {e})")),
        (_, Err(e)) => app.note(&format!("(failed: {e})")),
        // A host answered with the wrong kind of reply: say so, never panic.
        (_, Ok(other)) => app.note(&format!("(failed: unexpected host reply {other:?})")),
    }
}

/// `/copy`: build the OSC 52 sequence for the answer (the loop writes it) and
/// say what was attempted. A terminal cannot confirm OSC 52, so the note never
/// claims the clipboard changed.
fn copy_answer(app: &mut App, answer: Option<String>) {
    let Some(text) = answer.filter(|t| !t.trim().is_empty()) else {
        app.note("(nothing to copy: this session has no assistant answer yet)");
        return;
    };
    if text.len() > clipboard::MAX_COPY_BYTES {
        app.note(&format!(
            "(the last answer is {} KiB, over the {} KiB /copy limit; use /export to write the transcript to a file)",
            text.len().div_ceil(1024),
            clipboard::MAX_COPY_BYTES / 1024
        ));
        return;
    }
    app.note(&format!(
        "(sent {} characters to the terminal clipboard; needs OSC 52 support, tmux needs `set -g set-clipboard on`)",
        text.chars().count()
    ));
    app.osc52 = Some(clipboard::osc52(&text));
}

/// The host accepted an image: show the queue and name the new one.
fn attached(app: &mut App, images: Vec<ImageItem>) {
    if let Some(last) = images.last() {
        app.note(&format!(
            "(image attached to your next message: {})",
            last.label()
        ));
    }
    app.set_images(images);
}

/// A paste that looked like one image reference (#560): attach it, or, when
/// the host refuses, insert the text after all and say why it was not attached.
pub(crate) fn attach_pasted(host: &dyn TuiHost, app: &mut App, spec: String, text: &str) {
    match host.apply(HostCmd::AttachImage(spec)) {
        Ok(HostReply::Images(v)) => attached(app, v),
        Ok(other) => {
            app.paste_text(text);
            app.note(&format!(
                "(pasted as text: unexpected host reply {other:?})"
            ));
        }
        Err(e) => {
            app.paste_text(text);
            app.note(&format!("(pasted as text, not attached: {e})"));
        }
    }
}
