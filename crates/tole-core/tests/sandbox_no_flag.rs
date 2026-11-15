//! #750: `--no-sandbox` reads as "no policy" for an attended run and is
//! ignored the moment the process is unattended, whatever the host did.
//! One process, one order: statics are process-wide.

use tole_core::sandbox::flags::{self, Override};
use tole_core::sandbox::{self, Mode, Policy};

fn required() -> Policy {
    Policy {
        mode: Mode::Required,
        profile_name: "server".into(),
        profile: sandbox::builtin_profile("server").unwrap(),
        servers: Default::default(),
        skip: Vec::new(),
        max_servers: 4,
        idle_secs: 60,
    }
}

#[test]
fn no_sandbox_is_ignored_once_the_process_is_unattended() {
    sandbox::set_policy(Some(required())).unwrap();
    assert_eq!(sandbox::policy().map(|p| p.mode), Some(Mode::Required));
    assert!(flags::note().is_none());

    flags::set(Override::NoSandbox {
        configured: Mode::Required,
    })
    .unwrap();
    assert!(
        flags::set(Override::Profile {
            name: "strict".into(),
            configured: "server".into()
        })
        .is_err(),
        "a different override is refused"
    );
    assert!(
        sandbox::policy().is_none(),
        "attended: the flag is honoured"
    );
    assert!(flags::note().unwrap().contains("--no-sandbox overrides"));

    sandbox::mark_unattended();
    assert_eq!(
        sandbox::policy().map(|p| p.mode),
        Some(Mode::Required),
        "unattended: the configured policy is back"
    );
    assert!(flags::note().unwrap().contains("ignored in an unattended"));
    let launch = sandbox::launch("srv", None, "true", &[]);
    assert!(
        !matches!(launch, sandbox::Launch::Direct { .. }),
        "never unsandboxed for an unattended run: {launch:?}"
    );
}
