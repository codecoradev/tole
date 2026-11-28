//! #1314: the `usage.error` of a cron run is redacted before it is stored, so
//! `GET /cron/runs`, `tole cron runs` and the delivery targets never carry a
//! secret that a spawn error echoes (an agent job names a workdir that is gone).

use tole_cli::cron::{self, store, CreateSpec, TickOptions};

mod common;
use common::TestDir;

const T: i64 = 1_767_225_600;
const LEAKED: &str = "sk-leak-0123456789abcdefXYZ";

#[test]
fn a_secret_in_a_run_error_never_reaches_the_run_record() {
    // The agent fails before any provider call; it only needs a configuration.
    std::env::set_var("TOLE_BASE_URL", "http://127.0.0.1:9/v1");
    std::env::set_var("TOLE_MODEL", "mock-model");
    std::env::set_var("TOLE_API_KEY", "sk-test-cron-egress");
    // A secret the process holds by role (a webhook key): `run_claimed` redacts
    // with it.
    tole_core::secrets::add_role_secret("hooks.test", LEAKED);
    let dir = TestDir::new("tole-cron-egress");
    let work = dir.join(LEAKED);
    std::fs::create_dir_all(&work).unwrap();
    let spec = CreateSpec {
        id: Some("leaky".into()),
        schedule: "* * * * *".into(),
        prompt: Some("say hi".into()),
        workdir: work.clone(),
        ..CreateSpec::default()
    };
    cron::create(dir.path(), spec, T).unwrap();
    // The workdir is gone when the job runs: the error names its path.
    std::fs::remove_dir_all(&work).unwrap();
    let r = cron::tick(dir.path(), T + 60, &TickOptions::standalone(1)).unwrap();
    assert_eq!(r.ran.len(), 1, "{r:?}");
    let l = store::open_read(dir.path()).unwrap();
    let done = l.state().done(&r.ran[0].0).unwrap().clone();
    let usage = done.usage.expect("usage").to_string();
    assert!(usage.contains("\"error\":\""), "no error recorded: {usage}");
    assert!(
        !usage.contains(LEAKED),
        "the secret is in the record: {usage}"
    );
    let runs = cron::runs_json(dir.path(), None, None).unwrap().to_string();
    assert!(!runs.contains(LEAKED), "the secret is served: {runs}");
}
