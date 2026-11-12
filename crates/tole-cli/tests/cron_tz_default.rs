//! `cron.timezone` (#580 part 2): the zone of a job saved without one. Its
//! own test binary, because it writes the user config.

#![cfg(feature = "shell-tools")]

use tole_cli::cron::{self, CreateSpec, EditSpec};

/// 2026-01-01T00:00:00Z.
const T: i64 = 1_767_225_600;

#[test]
fn a_new_job_takes_cron_timezone_and_an_edit_never_adds_one() {
    let base = std::env::temp_dir().join(format!("tole-cron-tzdef-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(base.join("home/tole")).unwrap();
    std::fs::create_dir_all(base.join("w")).unwrap();
    std::env::set_var("CODECORA_HOME", base.join("home"));
    let (dir, work) = (base.join("cron"), base.join("w"));
    let spec = |id: &str| CreateSpec {
        id: Some(id.into()),
        schedule: "0 9 * * *".into(),
        script: Some("echo hi".into()),
        workdir: work.clone(),
        ..CreateSpec::default()
    };
    // Saved before the setting: no zone.
    assert!(cron::create(&dir, spec("before"), T)
        .unwrap()
        .timezone
        .is_none());
    std::fs::write(
        base.join("home/tole/config.toml"),
        "[cron]\ntimezone = \"+07:00\"\n",
    )
    .unwrap();
    let j = cron::create(&dir, spec("after"), T).unwrap();
    assert_eq!(j.timezone.as_deref(), Some("+07:00"));
    // An edit of the older job keeps it zone-less (plain UTC).
    let renamed = EditSpec {
        name: Some("renamed".into()),
        ..EditSpec::default()
    };
    let j = cron::edit(&dir, "before", renamed, T + 10).unwrap();
    assert!(j.timezone.is_none());
}
