//! #1314: the process secret set is a union over the sessions of a process.
//! Two sessions with different provider keys (profiles, #1038) each build their
//! provider on every turn; after both did, BOTH keys are redacted at every
//! egress that reads the process set, and the storage's own set agrees.

mod common;
use common::TestDir;
use tole_cli::provider_chain;
use tole_core::openai::OpenAiConfig;
use tole_core::turn::TurnOutcome;

const KEY_A: &str = "key-of-session-a-0123456789abcdef";
const KEY_B: &str = "key-of-session-b-fedcba9876543210";

fn build_for(key: &str) {
    let cfg = OpenAiConfig::new("http://provider.invalid/v1", "m", key);
    provider_chain::build(cfg, &|_: &str| -> Option<String> { None }, |p| p).expect("a provider");
}

#[test]
fn both_keys_are_redacted_at_every_egress_after_both_sessions_ran_a_turn() {
    build_for(KEY_A);
    build_for(KEY_B);
    // A second turn of A: the set is a union, never replaced by the turn's own.
    build_for(KEY_A);
    let both = format!("a={KEY_A} b={KEY_B}");
    let gone = |what: &str, text: &str| {
        for key in [KEY_A, KEY_B] {
            assert!(!text.contains(key), "{what} leaked {key}: {text}");
        }
    };

    gone(
        "process set",
        &tole_core::secrets::process_secrets().redact(&both),
    );
    gone(
        "GET /approvals",
        &tole_cli::approvals::redact_description(&both),
    );
    gone(
        "serve and ACP error",
        &tole_cli::session::problem::egress_error(&both),
    );

    // `problem.detail`, redacted with the session storage's set (the host).
    let dir = TestDir::new("tole-process-secrets");
    let storage = tole_core::storage::JsonlStorage::create(dir.path(), "s", None).unwrap();
    let outcome = TurnOutcome::ProviderFailed { message: both };
    let set = tole_core::storage::Storage::egress_secrets(&storage);
    let problem = tole_cli::session::problem::wire(&outcome, &set).expect("a problem");
    gone(
        "problem.detail",
        problem.detail.as_deref().unwrap_or_default(),
    );
}
