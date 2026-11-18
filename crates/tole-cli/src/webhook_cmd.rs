//! `tole webhook test <name>` (#911): sends one signed `webhook_test` event to
//! a destination of the user config and prints what came back, so a gateway
//! operator can check the wiring without a cron run. `tole doctor` makes no
//! network call; this is the explicit one.
//!
//! The destination is a NAME from `[webhook_out.<name>]`: there is no flag for
//! a URL, a header or a secret, so this cannot be pointed anywhere the owner
//! did not declare. The output is the destination name, the event id and a
//! status code or a closed error code: never the URL, a header, the key or any
//! text the receiver sent.

use anyhow::{bail, Result};
use clap::Subcommand;

use crate::webhook_out::probe::{self, Outcome, ProbeError};
use crate::webhook_out::send::{EnvFileKeys, NetPoster};

/// `tole webhook <action>`.
#[derive(Subcommand, Debug)]
pub enum WebhookAction {
    /// Send one signed `webhook_test` event to the destination NAME of the
    /// user config and print the status the receiver answered. One request, no
    /// retry, no outbox, no ledger row. Exits non-zero unless the receiver
    /// answered 2xx.
    Test { name: String },
}

/// Runs the subcommand.
pub fn cli(action: WebhookAction) -> Result<()> {
    match action {
        WebhookAction::Test { name } => test(&name),
    }
}

fn test(name: &str) -> Result<()> {
    let dests = crate::webhook_out::load_user().map_err(|e| anyhow::anyhow!("{e}"))?;
    if dests.is_empty() {
        bail!("no webhook destinations: declare [webhook_out.NAME] in the user config");
    }
    let now = crate::cron::now();
    // Unique per call: the time and this process.
    let nonce = format!(
        "{now}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos())
    );
    let r = probe::probe(&dests, name, now, &nonce, &NetPoster, &EnvFileKeys);
    match r {
        Err(ProbeError::NoSuchDestination) => {
            let known: Vec<&str> = dests.names().collect();
            bail!(
                "no webhook destination `{}` (known: {})",
                printable(name),
                known.join(", ")
            )
        }
        Err(ProbeError::KeyUnavailable) => bail!(
            "webhook destination `{name}`: its secret could not be read (the variable is not \
             set, or the key file is missing or shorter than 16 bytes)"
        ),
        Ok(p) => {
            println!("{}", report(name, &p));
            if p.outcome.ok() {
                Ok(())
            } else {
                bail!("webhook test to `{name}` was not delivered")
            }
        }
    }
}

/// The line printed for a result: the name, the event id and the status or the
/// code.
pub fn report(name: &str, p: &probe::Probed) -> String {
    match &p.outcome {
        Outcome::Status(s) if (200..300).contains(s) => {
            format!(
                "webhook test `{name}`: delivered, HTTP {s} (event {})",
                p.id
            )
        }
        Outcome::Status(s) => format!(
            "webhook test `{name}`: the receiver answered HTTP {s}, not 2xx (event {})",
            p.id
        ),
        Outcome::Failed(code) => {
            format!(
                "webhook test `{name}`: not delivered: {code} (event {})",
                p.id
            )
        }
    }
}

/// A name the user typed, for an error: printable ASCII only, cut short.
fn printable(s: &str) -> String {
    s.chars()
        .take(40)
        .map(|c| if c.is_ascii_graphic() { c } else { '?' })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::webhook_out::probe::Probed;

    fn p(outcome: Outcome) -> Probed {
        Probed {
            id: "test:0123456789abcdef".into(),
            outcome,
        }
    }

    #[test]
    fn the_report_says_what_happened_and_nothing_else() {
        let r = report("gateway", &p(Outcome::Status(204)));
        assert!(
            r.contains("delivered, HTTP 204") && r.contains("test:0123456789abcdef"),
            "{r}"
        );
        let r = report("gateway", &p(Outcome::Status(403)));
        assert!(r.contains("HTTP 403, not 2xx"), "{r}");
        let r = report("gateway", &p(Outcome::Failed("timeout")));
        assert!(r.contains("not delivered: timeout"), "{r}");
        for line in [
            report("gateway", &p(Outcome::Status(200))),
            report("gateway", &p(Outcome::Failed("dns"))),
        ] {
            assert!(
                !line.contains("http://") && !line.contains("https://"),
                "{line}"
            );
        }
    }

    #[test]
    fn a_typed_name_cannot_inject_control_characters_into_an_error() {
        assert_eq!(printable("a\nb\u{1b}[2Jc"), "a?b?[2Jc");
        assert_eq!(printable(&"x".repeat(100)).len(), 40);
    }
}
