use super::*;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

static COUNTER: AtomicU64 = AtomicU64::new(0);
static ENV_LOCK: Mutex<()> = Mutex::new(());

fn tmpdir() -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let d = std::env::temp_dir().join(format!("tole-trust-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d.canonicalize().unwrap()
}

/// A project dir with `.tole/config.toml` = `content`, plus a store path
/// (not yet created) in a separate dir.
fn project(content: &str) -> (PathBuf, PathBuf) {
    let d = tmpdir();
    std::fs::create_dir_all(d.join(".tole")).unwrap();
    std::fs::write(d.join(CONFIG_REL_PATH), content).unwrap();
    let store = tmpdir().join("home").join("tole").join(STORE_FILE);
    (d, store)
}

fn write_cfg(dir: &Path, content: &str) {
    std::fs::write(dir.join(CONFIG_REL_PATH), content).unwrap();
}

struct Fake {
    interactive: bool,
    answer: Option<bool>, // None = EOF
    shown: String,        // everything printed, incl. prompts
    confirms: usize,
}

impl Fake {
    fn new(interactive: bool, answer: Option<bool>) -> Fake {
        Fake {
            interactive,
            answer,
            shown: String::new(),
            confirms: 0,
        }
    }
}

impl ConfigTrustIo for Fake {
    fn is_interactive(&self) -> bool {
        self.interactive
    }
    fn inform(&mut self, text: &str) {
        self.shown.push_str(text);
        self.shown.push('\n');
    }
    fn warn(&mut self, text: &str) {
        self.shown.push_str(text);
        self.shown.push('\n');
    }
    fn confirm(&mut self, prompt_text: &str) -> bool {
        self.confirms += 1;
        self.shown.push_str(prompt_text);
        self.answer.unwrap_or(false)
    }
}

fn subject(dir: &Path) -> (Subject, Origin) {
    Subject::resolve(dir, None).unwrap()
}

// ------------------------------------------------------------- store

#[test]
fn store_round_trip_and_missing_is_empty() {
    let (_, sp) = project("model = \"m\"\n");
    assert_eq!(load_store(&sp).unwrap(), Store::default());
    record(&sp, Path::new("/a/b"), "x = 1\r\n").unwrap();
    let s = load_store(&sp).unwrap();
    assert_eq!(s.entries["/a/b"].content, "x = 1\r\n");
    let raw: serde_json::Value = serde_json::from_slice(&std::fs::read(&sp).unwrap()).unwrap();
    assert_eq!(raw["version"], 1);
    assert!(raw["entries"]["/a/b"]["trusted_at"].is_u64());
}

#[test]
fn trusted_iff_byte_identical() {
    let (_, sp) = project("");
    let k = Path::new("/p");
    assert_eq!(status(&sp, k, "a\n").unwrap(), TrustStatus::NotTrusted);
    record(&sp, k, "a\n").unwrap();
    assert_eq!(status(&sp, k, "a\n").unwrap(), TrustStatus::Trusted);
    assert_eq!(status(&sp, k, "a\n ").unwrap(), TrustStatus::Changed);
    assert_eq!(status(&sp, k, "a\r\n").unwrap(), TrustStatus::Changed);
    // same length, different bytes
    assert_eq!(status(&sp, k, "b\n").unwrap(), TrustStatus::Changed);
    assert_eq!(
        status(&sp, Path::new("/other"), "a\n").unwrap(),
        TrustStatus::NotTrusted
    );
}

#[test]
fn corrupt_store_is_a_hard_error_and_untouched() {
    let (_, sp) = project("");
    std::fs::create_dir_all(sp.parent().unwrap()).unwrap();
    std::fs::write(&sp, "{ not json").unwrap();
    let e = load_store(&sp).unwrap_err().to_string();
    assert!(e.contains(&sp.display().to_string()), "{e}");
    assert!(e.contains("fix or delete"), "{e}");
    assert!(record(&sp, Path::new("/p"), "x").is_err());
    assert!(remove(&sp, Path::new("/p")).is_err());
    assert_eq!(std::fs::read_to_string(&sp).unwrap(), "{ not json");
}

#[test]
fn unsupported_or_missing_version_errors() {
    let (_, sp) = project("");
    std::fs::create_dir_all(sp.parent().unwrap()).unwrap();
    std::fs::write(&sp, r#"{"version":2,"entries":{}}"#).unwrap();
    let e = load_store(&sp).unwrap_err().to_string();
    assert!(e.contains("unsupported version 2"), "{e}");
    std::fs::write(&sp, r#"{"entries":{}}"#).unwrap();
    assert!(load_store(&sp).is_err());
    std::fs::write(&sp, r#"{"version":1,"entries":{"/p":{"content":1}}}"#).unwrap();
    assert!(load_store(&sp).unwrap_err().to_string().contains("shape"));
}

#[cfg(unix)]
#[test]
fn store_perms_are_0600_file_0700_dir() {
    use std::os::unix::fs::PermissionsExt;
    let (_, sp) = project("");
    record(&sp, Path::new("/p"), "x").unwrap();
    let f = std::fs::metadata(&sp).unwrap().permissions().mode() & 0o777;
    let d = std::fs::metadata(sp.parent().unwrap())
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(f, 0o600);
    assert_eq!(d, 0o700);
}

#[test]
fn atomic_write_leaves_no_temp_file() {
    let (_, sp) = project("");
    record(&sp, Path::new("/p"), "x").unwrap();
    record(&sp, Path::new("/q"), "y").unwrap();
    let names: Vec<String> = std::fs::read_dir(sp.parent().unwrap())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, vec![STORE_FILE.to_string()]);
}

#[test]
fn content_cap_is_enforced() {
    let (_, sp) = project("");
    let big = "a".repeat(MAX_CONFIG_BYTES as usize + 1);
    assert!(record(&sp, Path::new("/p"), &big).is_err());
    assert!(!sp.exists());
    let ok = "a".repeat(MAX_CONFIG_BYTES as usize);
    record(&sp, Path::new("/p"), &ok).unwrap();
}

#[test]
fn remove_is_idempotent() {
    let (_, sp) = project("");
    assert!(!remove(&sp, Path::new("/p")).unwrap());
    assert!(!sp.exists(), "no store created for a no-op remove");
    record(&sp, Path::new("/p"), "x").unwrap();
    assert!(remove(&sp, Path::new("/p")).unwrap());
    assert!(!remove(&sp, Path::new("/p")).unwrap());
}

#[test]
fn default_store_path_honors_codecora_home_and_refuses_relative() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let prev = std::env::var("CODECORA_HOME").ok();
    std::env::set_var("CODECORA_HOME", "/tmp/cc-home-test");
    let good = default_store_path();
    // Relative or empty must never resolve into the cwd (the project).
    std::env::set_var("CODECORA_HOME", "rel/dir");
    let relative = default_store_path();
    std::env::set_var("CODECORA_HOME", "");
    let empty = default_store_path();
    match prev {
        Some(v) => std::env::set_var("CODECORA_HOME", v),
        None => std::env::remove_var("CODECORA_HOME"),
    }
    assert_eq!(
        good.unwrap(),
        PathBuf::from("/tmp/cc-home-test/tole/trusted-configs.json")
    );
    assert!(relative.is_err());
    // Empty falls through to HOME (absolute in any sane env) or errors; it
    // is never the relative path.
    if let Ok(p) = empty {
        assert!(p.is_absolute(), "{p:?}");
    }
}

// -------------------------------------------------------------- gate

const CFG: &str = "model = \"m\"\n";

#[test]
fn gate_explicit_is_trusted_without_store() {
    let (d, sp) = project(CFG);
    let (s, _) = subject(&d);
    let mut io = Fake::new(false, None);
    let out = gate(&s, Origin::Explicit, &sp, &mut io).unwrap();
    assert_eq!(out.kind, GateKind::Explicit);
    assert_eq!(out.content, CFG);
    assert!(!sp.exists());
    assert_eq!(io.confirms, 0);
}

#[test]
fn gate_discovered_trusted_is_ok() {
    let (d, sp) = project(CFG);
    let (s, o) = subject(&d);
    record(&sp, &s.key, CFG).unwrap();
    let mut io = Fake::new(false, None);
    let out = gate(&s, o, &sp, &mut io).unwrap();
    assert_eq!(out.kind, GateKind::AlreadyTrusted);
    assert!(io.shown.is_empty());
}

#[test]
fn gate_untrusted_interactive_yes_persists() {
    let (d, sp) = project(CFG);
    let (s, o) = subject(&d);
    let mut io = Fake::new(true, Some(true));
    let out = gate(&s, o, &sp, &mut io).unwrap();
    assert_eq!(out.kind, GateKind::TrustedNow);
    assert!(io.shown.contains(&s.shown().display().to_string()));
    assert!(io.shown.contains("model = \"m\""), "full content shown");
    assert!(io.shown.contains("trust this config? [y/N]"));
    let mut io2 = Fake::new(false, None);
    assert_eq!(
        gate(&s, o, &sp, &mut io2).unwrap().kind,
        GateKind::AlreadyTrusted
    );
}

#[test]
fn gate_no_and_eof_refuse_and_persist_nothing() {
    for ans in [Some(false), None] {
        let (d, sp) = project(CFG);
        let (s, o) = subject(&d);
        let mut io = Fake::new(true, ans);
        let e = gate(&s, o, &sp, &mut io).unwrap_err().to_string();
        assert_eq!(e, "config not trusted");
        assert!(!sp.exists());
    }
}

#[test]
fn is_yes_accepts_only_y_and_yes() {
    for y in ["y", "Y", "yes", "YES", " Yes \n"] {
        assert!(is_yes(y), "{y:?}");
    }
    for n in ["", "\n", "n", "no", "yep", "yes please", "ye"] {
        assert!(!is_yes(n), "{n:?}");
    }
}

#[test]
fn gate_non_interactive_exact_error_and_never_prompts() {
    let (d, sp) = project(CFG);
    let (s, o) = subject(&d);
    let mut io = Fake::new(false, Some(true));
    let e = gate(&s, o, &sp, &mut io).unwrap_err().to_string();
    assert_eq!(
        e,
        format!(
            "{} is not trusted (or changed since it was trusted). Run: tole config trust",
            s.shown().display()
        )
    );
    assert_eq!(io.confirms, 0);
    assert!(io.shown.is_empty());
    assert!(!sp.exists());
}

#[test]
fn gate_non_interactive_non_default_path_names_config_flag() {
    let (d, _) = project(CFG);
    let other = d.join("other.toml");
    std::fs::write(&other, CFG).unwrap();
    let (s, _) = Subject::resolve(&d, Some(&other)).unwrap();
    let e = gate(
        &s,
        Origin::Discovered,
        &tmpdir().join("s.json"),
        &mut Fake::new(false, None),
    )
    .unwrap_err()
    .to_string();
    assert!(e.contains("Run: tole config trust --config "), "{e}");
}

#[test]
fn gate_changed_content_prompts_with_diff() {
    let (d, sp) = project("model = \"old\"\nplan_mode = true\n");
    let (s, o) = subject(&d);
    record(&sp, &s.key, "model = \"old\"\nplan_mode = true\n").unwrap();
    write_cfg(&d, "model = \"new\"\nplan_mode = true\n");
    let mut io = Fake::new(true, Some(true));
    let out = gate(&s, o, &sp, &mut io).unwrap();
    assert_eq!(out.kind, GateKind::TrustedNow);
    assert!(io.shown.contains("- model = \"old\""), "{}", io.shown);
    assert!(io.shown.contains("+ model = \"new\""), "{}", io.shown);
    assert_eq!(
        status(&sp, &s.key, "model = \"new\"\nplan_mode = true\n").unwrap(),
        TrustStatus::Trusted
    );
}

#[test]
fn gate_corrupt_store_is_a_hard_error() {
    let (d, sp) = project(CFG);
    std::fs::create_dir_all(sp.parent().unwrap()).unwrap();
    std::fs::write(&sp, "garbage").unwrap();
    let (s, o) = subject(&d);
    let e = gate(&s, o, &sp, &mut Fake::new(true, Some(true)))
        .unwrap_err()
        .to_string();
    assert!(e.contains("trust store"), "{e}");
    assert_eq!(std::fs::read_to_string(&sp).unwrap(), "garbage");
}

// ---------------------------------------------------- terminal safety

const NASTY: &[char] = &[
    '\u{1b}', '\r', '\u{202e}', '\u{2028}', '\u{2029}', '\0', '\u{7f}', '\u{85}', '\u{200b}',
    '\u{feff}', '\u{2066}',
];

fn assert_clean(text: &str) {
    for c in NASTY {
        assert!(!text.contains(*c), "raw {:?} leaked in {text:?}", c);
    }
    assert!(!text.contains('\t'));
}

#[test]
fn sanitize_policy() {
    assert_eq!(sanitize("a\nb c"), "a\nb c");
    assert_eq!(sanitize("a\tb"), "a\\tb");
    assert_eq!(sanitize("\x1b[2J"), "\\u{1b}[2J");
    assert_eq!(sanitize("x\ry"), "x\\u{d}y");
    assert_eq!(sanitize("\u{202e}"), "\\u{202e}");
    assert_eq!(sanitize("\u{2028}"), "\\u{2028}");
    assert_eq!(sanitize("a\0b"), "a\\u{0}b");
    assert_eq!(sanitize("héllo ✓"), "héllo ✓");
}

#[test]
fn prompt_text_escapes_content_path_and_diff() {
    // A project dir whose NAME contains an ESC sequence and RLO.
    let base = tmpdir();
    let dir = base.join("p\u{1b}[2J\u{202e}x");
    std::fs::create_dir_all(dir.join(".tole")).unwrap();
    let sp = tmpdir().join(STORE_FILE);
    let old = "model = \"a\"\n";
    let new = "# \u{1b}[2J\r\u{202e}\u{2028}\0\nmodel = \"b\"\u{200b}\n";
    std::fs::write(dir.join(CONFIG_REL_PATH), new).unwrap();
    let (s, o) = Subject::resolve(&dir, None).unwrap();
    record(&sp, &s.key, old).unwrap();
    let mut io = Fake::new(true, Some(false));
    let _ = gate(&s, o, &sp, &mut io);
    assert_clean(&io.shown);
    assert!(io.shown.contains("\\u{1b}[2J"), "{}", io.shown);
    assert!(io.shown.contains("\\u{202e}"));
    assert!(io.shown.contains("\\u{2028}"));
    assert!(io.shown.contains("\\u{0}"));
    // Non-interactive error carries the path: also clean.
    let e = gate(&s, o, &sp, &mut Fake::new(false, None))
        .unwrap_err()
        .to_string();
    assert_clean(&e);
}

// -------------------------------------------------------------- diff

#[test]
fn diff_small_cases() {
    assert_eq!(line_diff("a\nb\nc", "a\nB\nc"), "  a\n- b\n+ B\n  c");
    assert_eq!(line_diff("a\nb", "a\nb\nc"), "  a\n  b\n+ c");
    assert_eq!(line_diff("a\nb\nc", "a\nc"), "  a\n- b\n  c");
    let far = line_diff("1\n2\n3\n4\n5\n6\n7\nx", "1\n2\n3\n4\n5\n6\n7\ny");
    assert!(far.starts_with("  ...\n"), "{far}");
    assert!(far.ends_with("- x\n+ y"), "{far}");
    // CRLF-only change is visible, not invisible.
    let crlf = line_diff("a\nb", "a\r\nb");
    assert!(crlf.contains("+ a\\u{d}"), "{crlf}");
}

#[test]
fn diff_over_limit_falls_back_to_summary() {
    let old = "x\n".repeat(MAX_DIFF_LINES + 5);
    let new = "y\n".repeat(10);
    let d = line_diff(&old, &new);
    assert_eq!(
        d,
        format!("changed ({} lines -> {} lines)", MAX_DIFF_LINES + 6, 11)
    );
}

// ---------------------------------------------------------- commands

#[test]
fn trust_cmd_refuses_invalid_file() {
    let (d, sp) = project("model = 1\n");
    let e = trust_cmd(&d, None, true, &sp, &mut Fake::new(true, Some(true)))
        .unwrap_err()
        .to_string();
    assert!(e.contains("config.toml"), "{e}");
    assert!(!sp.exists());
    write_cfg(&d, "api_key = \"x\"\n");
    assert!(trust_cmd(&d, None, true, &sp, &mut Fake::new(true, Some(true))).is_err());
    assert!(!sp.exists());
}

#[test]
fn trust_cmd_non_tty_needs_yes() {
    let (d, sp) = project(CFG);
    let mut io = Fake::new(false, Some(true));
    let e = trust_cmd(&d, None, false, &sp, &mut io)
        .unwrap_err()
        .to_string();
    assert!(e.contains("--yes"), "{e}");
    assert_eq!(io.confirms, 0);
    assert!(!sp.exists());
}

#[test]
fn trust_cmd_yes_still_prints_and_persists() {
    let (d, sp) = project(CFG);
    let mut io = Fake::new(false, None);
    trust_cmd(&d, None, true, &sp, &mut io).unwrap();
    assert!(io.shown.contains("model = \"m\""), "{}", io.shown);
    assert_eq!(io.confirms, 0);
    let (s, _) = subject(&d);
    assert_eq!(status(&sp, &s.key, CFG).unwrap(), TrustStatus::Trusted);
    // second time: already trusted
    let mut io2 = Fake::new(false, None);
    trust_cmd(&d, None, false, &sp, &mut io2).unwrap();
    assert!(io2.shown.contains("already trusted"));
}

#[test]
fn trust_cmd_tty_no_refuses() {
    let (d, sp) = project(CFG);
    let e = trust_cmd(&d, None, false, &sp, &mut Fake::new(true, Some(false)))
        .unwrap_err()
        .to_string();
    assert_eq!(e, "config not trusted");
    assert!(!sp.exists());
}

#[test]
fn trust_cmd_missing_default_file_errors() {
    let d = tmpdir();
    let sp = tmpdir().join(STORE_FILE);
    assert!(trust_cmd(&d, None, true, &sp, &mut Fake::new(true, None)).is_err());
}

#[test]
fn untrust_cmd_is_idempotent() {
    let (d, sp) = project(CFG);
    trust_cmd(&d, None, true, &sp, &mut Fake::new(false, None)).unwrap();
    let mut io = Fake::new(false, None);
    untrust_cmd(&d, None, &sp, &mut io).unwrap();
    assert!(io.shown.starts_with("untrusted:"), "{}", io.shown);
    let mut io2 = Fake::new(false, None);
    untrust_cmd(&d, None, &sp, &mut io2).unwrap();
    assert!(io2.shown.contains("nothing to remove"), "{}", io2.shown);
    let (s, _) = subject(&d);
    assert_eq!(status(&sp, &s.key, CFG).unwrap(), TrustStatus::NotTrusted);
}

#[test]
fn trust_line_variants() {
    let (d, sp) = project(CFG);
    let (s, _) = subject(&d);
    assert_eq!(
        trust_line(&s, &sp).unwrap(),
        "trust: NOT trusted — run: tole config trust"
    );
    record(&sp, &s.key, CFG).unwrap();
    assert_eq!(trust_line(&s, &sp).unwrap(), "trust: trusted");
    write_cfg(&d, "model = \"z\"\n");
    assert_eq!(
        trust_line(&s, &sp).unwrap(),
        "trust: CHANGED since it was trusted — run: tole config trust"
    );
}
