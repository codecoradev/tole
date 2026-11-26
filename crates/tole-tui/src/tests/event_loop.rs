use super::*;

#[test]
fn loop_runs_a_turn_streams_it_and_quits() {
    let mut term = Terminal::with_options(
        TestBackend::new(60, 12),
        TerminalOptions {
            viewport: Viewport::Inline(VIEWPORT_HEIGHT),
        },
    )
    .unwrap();
    let (_p, rx) = approval_channel();
    let mut script: Vec<Option<Event>> = "hi"
        .chars()
        .map(|c| Some(Event::Key(key(KeyCode::Char(c)))))
        .collect();
    script.push(Some(Event::Key(key(KeyCode::Enter))));
    // idle ticks while the turn thread finishes, then quit
    script.extend(std::iter::repeat_n(None, 30));
    script.push(Some(Event::Key(ctrl('c'))));
    script.push(Some(Event::Key(ctrl('c'))));
    let mut it = script.into_iter();
    event_loop(&mut term, &Host, &mut App::new(footer()), &rx, |_| {
        std::thread::sleep(Duration::from_millis(5));
        Ok(it.next().unwrap_or(None))
    })
    .unwrap();
    let text = format!("{}", term.backend());
    assert!(text.contains("echo: hi"), "{text}");
    assert!(text.contains("proj · s-1"), "{text}");
}

struct Switchable(Mutex<Option<String>>);

impl TuiHost for Switchable {
    fn footer(&self) -> Footer {
        Footer {
            effort: self.0.lock().unwrap().clone(),
            ..footer()
        }
    }
    fn run_turn(&self, _: &str, _: &CancelToken, _: Arc<dyn TurnObserver>) -> TurnEnd {
        TurnEnd::Final(String::new())
    }
    fn apply(&self, cmd: HostCmd) -> Result<HostReply, String> {
        match cmd {
            HostCmd::SetEffort(level) => {
                *self.0.lock().unwrap() = level;
                Ok(HostReply::Done)
            }
            _ => unsupported(),
        }
    }
}

#[test]
fn a_switch_refreshes_the_footer_on_the_next_loop_turn() {
    let host = Switchable(Mutex::new(None));
    let mut term = Terminal::with_options(
        TestBackend::new(100, 14),
        TerminalOptions {
            viewport: Viewport::Inline(VIEWPORT_HEIGHT),
        },
    )
    .unwrap();
    let (_p, rx) = approval_channel();
    let mut script = vec![Some(Event::Key(shift(KeyCode::BackTab)))];
    script.extend(std::iter::repeat_n(None, 5));
    script.push(Some(Event::Key(ctrl('c'))));
    script.push(Some(Event::Key(ctrl('c'))));
    let mut it = script.into_iter();
    let mut app = App::new(host.footer());
    event_loop(&mut term, &host, &mut app, &rx, |_| {
        Ok(it.next().unwrap_or(None))
    })
    .unwrap();
    assert_eq!(host.0.lock().unwrap().as_deref(), Some("minimal"));
    assert_eq!(app.footer.effort.as_deref(), Some("minimal"));
    let text = format!("{}", term.backend());
    assert!(text.contains("effort: minimal"), "{text}");
    assert!(text.contains("thinking: minimal"), "editor border: {text}");
}

/// Steering reaches the running turn through the existing #377
/// soft-interrupt queue: the host sees the text on `cancel.interrupts()`.
struct SteerHost;

impl TuiHost for SteerHost {
    fn footer(&self) -> Footer {
        footer()
    }
    fn run_turn(&self, p: &str, c: &CancelToken, _o: Arc<dyn TurnObserver>) -> TurnEnd {
        if p == "first" {
            // Generous: only a run that never steers waits for it.
            let end = Instant::now() + Duration::from_secs(60);
            while c.interrupts().is_empty() && Instant::now() < end {
                std::thread::sleep(Duration::from_millis(5));
            }
            let got: Vec<_> = c.interrupts().drain().into_iter().map(|i| i.text).collect();
            return TurnEnd::Final(format!("steered-with {}", got.join("+")));
        }
        std::thread::sleep(Duration::from_millis(60));
        TurnEnd::Final(format!("ran {p}"))
    }
}

fn run_script(host: &dyn TuiHost, script: Vec<Option<Event>>) -> String {
    let mut term = Terminal::with_options(
        TestBackend::new(80, 14),
        TerminalOptions {
            viewport: Viewport::Inline(VIEWPORT_HEIGHT),
        },
    )
    .unwrap();
    let (_p, rx) = approval_channel();
    let mut it = script.into_iter();
    event_loop(&mut term, host, &mut App::new(footer()), &rx, |_| {
        std::thread::sleep(Duration::from_millis(5));
        Ok(it.next().unwrap_or(None))
    })
    .unwrap();
    format!("{}", term.backend())
}

fn keys(s: &str) -> Vec<Option<Event>> {
    s.chars()
        .map(|c| Some(Event::Key(key(KeyCode::Char(c)))))
        .collect()
}

#[test]
fn loop_steer_reaches_the_interrupt_queue_of_the_running_turn() {
    let mut script = keys("first");
    script.push(Some(Event::Key(key(KeyCode::Enter))));
    script.extend(keys("go left"));
    script.push(Some(Event::Key(key(KeyCode::Enter))));
    script.extend(std::iter::repeat_n(None, 40));
    script.push(Some(Event::Key(ctrl('c'))));
    script.push(Some(Event::Key(ctrl('c'))));
    let text = run_script(&SteerHost, script);
    assert!(text.contains("steered-with go left"), "{text}");
}

#[test]
fn loop_runs_queued_follow_ups_after_the_turn_settles_in_order() {
    let mut script = keys("slowish");
    script.push(Some(Event::Key(key(KeyCode::Enter))));
    for m in ["q1", "q2"] {
        script.extend(keys(m));
        script.push(Some(Event::Key(alt(KeyCode::Enter))));
    }
    script.extend(std::iter::repeat_n(None, 120));
    script.push(Some(Event::Key(ctrl('c'))));
    script.push(Some(Event::Key(ctrl('c'))));
    let text = run_script(&SteerHost, script);
    let (a, b) = (text.find("ran q1"), text.find("ran q2"));
    assert!(a.is_some() && b.is_some() && a < b, "{text}");
}
