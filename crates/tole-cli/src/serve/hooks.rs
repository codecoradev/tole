//! `POST /hooks/<route>` (#582): signature check, rate limit, idempotency and the cron run
//! a delivery fires.

use super::request::Request;
use super::State;
use serde_json::json;
use std::sync::Arc;

/// An insecure loopback route's delivery: no signature, so the delivery id
/// and the event are taken as sent (still validated).
fn insecure_delivery(
    h: &tole_core::webhook::Headers,
) -> Result<tole_core::webhook::Verified, tole_core::webhook::Reject> {
    use tole_core::webhook::{self, Reject};
    if h.event.as_deref().is_some_and(|e| !webhook::valid_event(e)) {
        return Err(Reject::BadEvent);
    }
    let id = h
        .delivery
        .clone()
        .filter(|d| webhook::valid_delivery(d))
        .ok_or(Reject::BadDelivery)?;
    Ok(webhook::Verified {
        key: id.clone(),
        delivery: id,
        event: h.event.clone(),
    })
}

/// `POST /hooks/<route>` (#582): the signature check, then the route's rate
/// limit and the delivery's idempotency, both kept in the cron ledger so
/// they survive a restart. A failed signature counts toward the source's
/// failed-auth limit. Answers never echo a header value but the delivery
/// id, which passed validation.
pub(super) fn handle_hook(
    state: &State,
    name: &str,
    req: &Request,
    peer: Option<std::net::IpAddr>,
) -> (u16, serde_json::Value) {
    use tole_core::webhook::{self, Reject};
    let Some(route) = state.hooks.get(name) else {
        return (404, json!({"error": "no such hook route"}));
    };
    let now = tole_cli::cron::now();
    let checked = match route.key() {
        Some(key) => webhook::verify(key, &req.hook, &req.body, now),
        // An insecure route answers loopback clients only (and loads only
        // on a loopback bind).
        None if peer.is_some_and(|ip| ip.is_loopback()) => insecure_delivery(&req.hook),
        None => Err(Reject::Unsigned),
    };
    let verified = match checked {
        Ok(v) => v,
        Err(r) => {
            if r.status() == 401 {
                if let Some(ip) = peer {
                    state.auth_failures.record_failure(ip);
                }
            }
            return (r.status(), json!({"error": r.reason()}));
        }
    };
    let Some(dir) = &state.hook_dir else {
        return (503, json!({"error": "no cron dir for webhook state"}));
    };
    let rate = route.rate_per_minute as usize;
    let outcome = tole_cli::cron::store::with_lock(dir, |l| {
        if l.state().hooks_since(name, now - 59) >= rate {
            return Ok(None);
        }
        // #678: the key is signed material (the native delivery id, or the
        // GitHub body's digest), so a capture re-sent under a fresh
        // delivery header is a duplicate.
        Ok(Some(l.record_hook(name, &verified.key, now)?))
    });
    let id = verified.delivery.as_str();
    match outcome {
        Ok(None) => (429, json!({"error": "rate limit for this route"})),
        Ok(Some(false)) => (200, json!({"status": "duplicate", "delivery": id})),
        Ok(Some(true)) => hook_action(state, name, route, req, &verified, dir, now),
        Err(e) => {
            eprintln!("tole serve: webhook state: {e}");
            (500, json!({"error": "webhook state unavailable"}))
        }
    }
}

/// A verified, first-seen delivery (#582 part 2): the events allowlist and
/// the filter run on the payload before any LLM work (`200 ignored`); a
/// match fires the route's `cron_job` through the claim path a tick uses
/// (the #640 trust check, delivery, provider, skills, the daily cap) and
/// answers `202` once the run is claimed. The run gets the fenced, rendered
/// payload as context for this run only, and no Write tools unless the
/// route says `allow_writes`.
fn hook_action(
    state: &State,
    name: &str,
    route: &tole_cli::hooks::Route,
    req: &Request,
    verified: &tole_core::webhook::Verified,
    dir: &std::path::Path,
    now: i64,
) -> (u16, serde_json::Value) {
    use tole_cli::hook_filter;
    let (id, event) = (verified.delivery.as_str(), verified.event.as_deref());
    if !hook_filter::event_allowed(route.events.as_deref(), event) {
        return (
            200,
            json!({"status": "ignored", "reason": "event", "delivery": id}),
        );
    }
    let payload = match serde_json::from_slice::<serde_json::Value>(&req.body) {
        Ok(v) => v,
        Err(_) if route.filter.is_none() && route.template.is_none() => serde_json::Value::Null,
        Err(_) => return (400, json!({"error": "the body is not JSON"})),
    };
    if route
        .filter
        .as_ref()
        .is_some_and(|f| !hook_filter::matches(f, &payload))
    {
        return (
            200,
            json!({"status": "ignored", "reason": "filter", "delivery": id}),
        );
    }
    let Some(job) = &route.cron_job else {
        return (202, json!({"status": "accepted", "delivery": id}));
    };
    let secrets = tole_cli::provider_chain::process_secrets(&tole_cli::config_apply::real_env);
    let rendered = route
        .template
        .as_ref()
        .map(|t| t.render(&payload, &secrets))
        .unwrap_or_default();
    let trigger = tole_cli::cron::tick::Trigger {
        context: hook_filter::fenced(name, event, id, &rendered),
        read_only: !route.allow_writes,
    };
    let opts = tole_cli::cron::TickOptions {
        approvals: Some(Arc::clone(&state.approvals)),
        daily_token_cap: tole_cli::cron::daily_token_cap(),
        webhooks: tole_cli::webhook_out::loaded(),
        ..tole_cli::cron::TickOptions::standalone(1)
    };
    let mut report = tole_cli::cron::TickReport::default();
    match tole_cli::cron::claim_now(dir, job, now, &opts, &mut report) {
        Ok(Some(c)) => {
            let fire = c.fire_id().to_string();
            let claimed = vec![c.with_trigger(trigger)];
            let dir = dir.to_path_buf();
            std::thread::spawn(move || {
                let mut r = tole_cli::cron::TickReport::default();
                if let Err(e) = tole_cli::cron::tick::run_claimed(&dir, claimed, now, &opts, &mut r)
                {
                    eprintln!("tole serve: webhook run: {e}");
                }
            });
            (
                202,
                json!({"status": "accepted", "delivery": id, "fire": fire}),
            )
        }
        Ok(None) => (
            409,
            json!({"error": "the job did not run (blocked or out of budget; see tole cron list)", "delivery": id}),
        ),
        Err(e) => (409, json!({"error": e.to_string(), "delivery": id})),
    }
}
