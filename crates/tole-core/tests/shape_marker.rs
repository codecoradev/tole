//! #597 review fixes: a `«NAME»` marker is only allowed where it is the
//! whole secret position; JSON fields are masked by their name; the log
//! records the masked shape kinds.

#[path = "support/test_dir.rs"]
mod test_dir;
use serde_json::{json, Value};
use test_dir::TestDir;
use tole_core::entry::{EntryType, NewEntry};
use tole_core::secrets::SecretSet;
use tole_core::storage::{Commit, JsonlStorage, Storage};

const U: &str = "Zq8vN2xL5mP9aR4tW7yB3cD6";

fn sets() -> Vec<SecretSet> {
    vec![
        SecretSet::default(),
        SecretSet::default().with("OTHER_TOKEN", "other-known-value-0597"),
    ]
}

fn pem() -> (String, Vec<String>) {
    let body: Vec<String> = (0..3)
        .map(|i| {
            format!("MIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQC{i}x7Zq8vN2xL5mP9aR4tW7y")
        })
        .collect();
    let head = ["-----BEGIN RSA ", "PRIVATE KEY-----"].concat();
    let foot = ["-----END RSA ", "PRIVATE KEY-----"].concat();
    (format!("{head}\n{}\n{foot}", body.join("\n")), body)
}

/// Text that merely CONTAINS a `«NAME»` marker next to a real secret must
/// not be kept: the marker is only allowed as the whole secret position.
#[test]
fn a_marker_next_to_a_real_secret_never_smuggles_it_through() {
    for s in sets() {
        for text in [
            format!("API_KEY=«A»{U}"),
            format!("API_KEY={U}«A»"),
            format!("https://«A»:{U}@h"),
            format!("https://bob:«A»{U}@h"),
            format!("curl --password=«A»{U} x"),
            format!("curl -u «A»:{U} x"),
            format!("curl -u bob:«A»{U} x"),
        ] {
            let out = s.redact(&text);
            assert!(!out.contains(U), "{text:?} leaked: {out}");
        }
        let (pem, body) = pem();
        let plain = s.redact(&pem).into_owned();
        assert!(
            !plain.contains(body[1].as_str()),
            "plain pem leaks: {plain}"
        );
        let tampered = pem.replacen(&body[1], &format!("«A»{}", body[1]), 1);
        let out = s.redact(&tampered);
        for line in &body {
            assert!(!out.contains(line.as_str()), "pem line leaked: {out}");
        }
    }
}

/// The legitimate marker positions stay readable: the name survives.
#[test]
fn a_marker_that_is_the_whole_secret_position_stays() {
    let v = "Zq8vN2xL5mP9aR4tW7yB3cD6xx";
    let s = SecretSet::default().with("MCP_URL", v);
    for (text, want) in [
        (
            format!("https://bob:{v}@host/p"),
            "https://bob:«MCP_URL»@host/p",
        ),
        (
            format!("curl --password={v} x"),
            "curl --password=«MCP_URL» x",
        ),
        (format!("curl -u bob:{v} x"), "curl -u bob:«MCP_URL» x"),
        (format!("API_KEY={v}"), "API_KEY=«MCP_URL»"),
    ] {
        assert_eq!(s.redact(&text), want, "{text}");
    }
}

/// A JSON field whose NAME makes the value secret is masked like
/// `API_KEY=value` in text (`redact_value` sees one string at a time).
#[test]
fn json_fields_are_masked_by_their_name() {
    for s in sets() {
        let v = json!({"api_key": U, "nested": [{"password": U}], "note": "plain words"});
        let out = s.redact_value(&v);
        assert!(!out.to_string().contains(U), "{out}");
        assert!(out.to_string().contains("plain words"), "{out}");
        assert_eq!(s.redact_value(&out), out, "idempotent");
    }
}

/// An entry whose payload only had a shape masked still says so.
#[test]
fn the_log_records_the_masked_shape_kinds() {
    let token = ["ghp", "_aB3dE5gH7jK9mN1pQ3rS5tU7vW9xY1zA3bC5"].concat();
    let dir = TestDir::new("tole-597-kinds");
    let mut st = JsonlStorage::create(&dir, "k", None).unwrap();
    st.set_secrets(SecretSet::default());
    for text in [format!("key {token}"), "nothing here".to_string()] {
        st.commit(Commit::new().entry(NewEntry::root(
            EntryType::new(EntryType::MESSAGE),
            json!({"role": "user", "text": text}),
        )))
        .unwrap();
    }
    let file = std::fs::read_to_string(dir.join("k.jsonl")).unwrap();
    let lines: Vec<Value> = file
        .lines()
        .skip(1)
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(
        lines[0]["redacted"],
        json!(["shape:github-token"]),
        "{file}"
    );
    assert!(lines[1].get("redacted").is_none());
    drop(st);
    JsonlStorage::open(dir.join("k.jsonl")).unwrap();
}

/// Control bytes (binary or ANSI output, injected text) around a marker do
/// not switch the protection off: the stand-in is chosen per call.
#[test]
fn control_bytes_around_a_marker_do_not_defeat_the_swap() {
    let secret = "Zx9$kQ2mPz7Lw4Rt8Vb";
    for s in sets() {
        for text in [
            format!("API_KEY=«A»\x01{secret}"),
            format!("TOKEN=«A»\x02{secret}"),
            format!("\x01API_KEY=«A»{U}\x02"),
            format!("\x01\x02 API_KEY=«A»\x01{U}"),
            format!("API_KEY=\x02«A»{U}"),
        ] {
            let out = s.redact(&text);
            assert!(
                !out.contains(U) && !out.contains(secret),
                "{text:?} leaked: {out:?}"
            );
        }
    }
}

/// A string that looks like a stand-in, supplied by the text, is never
/// restored as a marker, and does not stop the real markers being swapped.
#[test]
fn a_stand_in_look_alike_in_the_text_is_never_restored() {
    for s in sets() {
        for text in [
            "x «A» qzk0x qzkk0x qzkkk1x".to_string(),
            "qzk0x «A» qzk0x".to_string(),
            "QZKK0X «A» qZkKk0x".to_string(),
        ] {
            assert_eq!(s.redact(&text), text, "{text}");
        }
        let out = s
            .redact(&format!("API_KEY=«A»{U} qzk0x qzkk0x"))
            .into_owned();
        assert!(!out.contains(U), "{out}");
        assert!(
            out.contains("qzk0x qzkk0x"),
            "look-alikes kept as written: {out}"
        );
    }
}

/// A marker inside a PEM body, single-line or multi-line, does not break the
/// key block: the whole key is masked.
#[test]
fn a_marker_inside_a_pem_body_does_not_leak_the_key() {
    let half = "MIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQCx7Zq8vN2xL5mP9aR4";
    let head = ["-----BEGIN RSA ", "PRIVATE KEY-----"].concat();
    let foot = ["-----END RSA ", "PRIVATE KEY-----"].concat();
    for s in sets() {
        for pem in [
            format!("{head}\n{half}«A»{half}\n{foot}"),
            format!("{head}\n{half}\n{half}«A»{half}\n{half}\n{foot}"),
            format!("{head}\n{half}«A»\n{half}\n{foot}"),
        ] {
            let out = s.redact(&pem);
            assert!(!out.contains(half), "pem body leaked: {out}");
        }
    }
}

/// Every real marker is swapped, whatever its name holds: a known secret
/// whose NAME has other characters (an env var like `ProgramFiles(x86)`)
/// must not leave a non-ASCII `«NAME»` that switches the detector off for a
/// secret glued to it.
#[test]
fn markers_with_unusual_names_never_hide_a_glued_secret() {
    let value = "kv_Value-0597-known";
    let long64 = "N".repeat(64);
    let long65 = "N".repeat(65);
    let long300 = "L".repeat(300);
    let names: Vec<&str> = vec![
        "ProgramFiles(x86)",
        "A:B",
        "BASH_FUNC_x%%",
        "ÄB",
        "a b",
        &long64,
        &long65,
        &long300,
        "a»b",
        "»",
        "a«b",
        "«",
        "qzk",
        "qzkk0x",
        "qzkk0x»",
        "9",
        "1x",
        "x",
        "tab\there",
        "bell\u{7}",
        "",
    ];
    for name in names {
        let s = SecretSet::default().with(name, value);
        for text in [
            format!("API_KEY={value}{U}"),
            format!("API_KEY={U}{value}"),
            format!("https://bob:{value}{U}@h"),
            format!("https://{value}:{U}@h"),
            format!("Authorization: Bearer {value}{U}"),
            format!("curl --password={value}{U} x"),
        ] {
            let out = s.redact(&text);
            assert!(!out.contains(U), "name {name:?}: {text:?} leaked: {out:?}");
            assert!(
                !out.contains(value),
                "name {name:?}: known value leaked: {out:?}"
            );
        }
        // The legitimate position keeps the (unusual) name.
        let out = s
            .redact(&format!("API_KEY={value} and https://bob:{value}@h"))
            .into_owned();
        assert!(
            out.contains(&format!("«{name}»")),
            "name {name:?} lost: {out:?}"
        );
    }
}

/// The restore is one pass: a large marker count stays linear (the old
/// restore was one `replace` per marker over the whole text).
#[test]
fn a_large_marker_count_is_restored_in_one_pass() {
    let n = 50_000;
    let text = format!("{}API_KEY={U}", "«A»".repeat(n));
    let out = SecretSet::default().redact(&text);
    assert!(!out.contains(U));
    assert_eq!(out.matches("«A»").count(), n, "every marker restored once");
    // A stand-in look-alike beyond the swapped indexes is left as written.
    let odd = SecretSet::default().redact("«A» qzk99999x qzk0x");
    assert_eq!(odd, "«A» qzk99999x qzk0x");
}

/// The longest marker that matches wins: with names `A` and `A»B`, the text
/// `«A»B»<secret>` is ONE marker (`«A»B»`), not `«A»` followed by `B»` (a
/// non-ASCII neighbour that would switch the detector off for the secret).
#[test]
fn the_longest_known_marker_wins() {
    let (va, vb) = ("kv_Value-0597-aaa", "kv_Value-0597-bbb");
    // `A` is added first, so only the sort puts `A»B` ahead of it.
    let s = SecretSet::default().with("A", va).with("A»B", vb);
    let out = s.redact(&format!("API_KEY={vb}{U}")).into_owned();
    assert_eq!(out, "API_KEY=[REDACTED:env-secret]");
    // Each marker alone still keeps its name.
    assert_eq!(s.redact(&format!("X={vb}")), "X=«A»B»");
    assert_eq!(s.redact(&format!("X={va}")), "X=«A»");
}
