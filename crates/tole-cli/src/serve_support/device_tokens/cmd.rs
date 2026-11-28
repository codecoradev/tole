//! `tole serve token add|list|revoke` (#1320, part 1).
//!
//! `add` prints the token on **stdout** and nothing else there, once, before
//! anything else is said (if that write fails the new token is revoked and
//! the command fails); the sentence about it goes to stderr, last. So `T=$(tole
//! serve token add phone --scope read-only)` captures just the secret. That
//! keeps it out of the process list only for `add`: using `$T` later in a
//! command line shows it to every local process. `add` and `revoke` write an
//! owner decision and refuse under an agent session (#684). `list` shows
//! names, scopes and times, never a token or a hash.

use std::io::Write;

use anyhow::{anyhow, Result};
use clap::Subcommand;

use super::{Entry, Scope, Store, StoreError};

/// `tole serve <action>`: what is not the server itself.
#[derive(Subcommand, Debug)]
pub enum ServeAction {
    /// Per-device tokens: one named, scoped secret per device (#1320).
    Token {
        #[command(subcommand)]
        action: TokenAction,
    },
}

#[derive(Subcommand, Debug)]
pub enum TokenAction {
    /// Make a token and print it once, on stdout. It cannot be shown again:
    /// only its hash is stored.
    Add {
        /// Lower-case letters, digits, '.', '_' and '-' (at most 64); unique
        /// among active tokens.
        name: String,
        /// `read-only` reads sessions and events and answers no approval;
        /// `full` is what the shared bearer token can do.
        #[arg(long, value_enum)]
        scope: Scope,
    },
    /// List the tokens (never their values), revoked ones included.
    List,
    /// Revoke the active token with this name (a new line in the file; the
    /// history stays).
    Revoke { name: String },
}

/// `tole serve <action>` on the real data dir and clock.
pub fn cli(action: ServeAction) -> Result<()> {
    let ServeAction::Token { action } = action;
    match &action {
        TokenAction::Add { .. } => crate::owner_only::refuse_under_session("tole serve token add")
            .map_err(|e| anyhow!(e))?,
        TokenAction::Revoke { .. } => {
            crate::owner_only::refuse_under_session("tole serve token revoke")
                .map_err(|e| anyhow!(e))?
        }
        TokenAction::List => {}
    }
    let store = Store::in_data_dir().map_err(|e| anyhow!("{e}"))?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    run(
        &store,
        action,
        now,
        &mut std::io::stdout(),
        &mut std::io::stderr(),
    )
}

/// The commands over any store, clock and writers.
pub fn run(
    store: &Store,
    action: TokenAction,
    now: u64,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<()> {
    let e = |e: StoreError| anyhow!("{e}");
    match action {
        TokenAction::Add { name, scope } => {
            let added = store.add(&name, scope, now).map_err(e)?;
            // The secret goes out FIRST and must arrive: a token nobody saw
            // is revoked at once, not left active and unknown.
            let shown = writeln!(out, "{}", added.secret.expose()).and_then(|()| out.flush());
            if let Err(io) = shown {
                return Err(match store.revoke(&name, now) {
                    Ok(()) => anyhow!(
                        "could not print the token for {name:?} ({io}); it was revoked, try again"
                    ),
                    Err(re) => anyhow!(
                        "could not print the token for {name:?} ({io}) and could not revoke it \
                         ({re}): run `tole serve token revoke {name}`"
                    ),
                });
            }
            writeln!(
                err,
                "tole: token {:?} ({}) created. It is shown only now; keep it.",
                added.name,
                scope.as_str()
            )?;
        }
        TokenAction::List => {
            let snap = store.load().map_err(e)?;
            if snap.entries().is_empty() {
                writeln!(err, "tole: no device tokens")?;
            } else {
                writeln!(
                    out,
                    "{:<24} {:<10} {:<8} {:<17} {:<17}",
                    "NAME", "SCOPE", "STATE", "CREATED", "LAST USED"
                )?;
                for t in snap.entries() {
                    writeln!(out, "{}", row(t))?;
                }
            }
        }
        TokenAction::Revoke { name } => {
            store.revoke(&name, now).map_err(e)?;
            writeln!(err, "tole: token {name:?} revoked")?;
        }
    }
    Ok(())
}

fn row(t: &Entry) -> String {
    let state = if t.active() { "active" } else { "revoked" };
    let used = t.last_used_at.map_or("never".to_string(), utc);
    format!(
        "{:<24} {:<10} {:<8} {:<17} {:<17}",
        t.name,
        t.scope.as_str(),
        state,
        utc(t.created_at),
        used
    )
}

/// `YYYY-MM-DD HH:MMZ`.
pub(super) fn utc(secs: u64) -> String {
    let (y, m, d) = tole_core::ledger::cron::civil_from_days((secs / 86_400) as i64);
    let rem = secs % 86_400;
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60
    )
}
