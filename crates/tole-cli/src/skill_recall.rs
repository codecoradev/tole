//! Skill exposure for a session (#590): today's full index, or a pinned
//! core plus skills recalled by meaning through uteke.
//!
//! Opt-in through the environment (a `[skills]` config table is a
//! follow-up):
//!
//! - `TOLE_SKILL_INDEX`: `local` (default: every discovered skill is listed,
//!   exactly as before) or `uteke` (pinned + top K by meaning, plus the
//!   `skill_search` tool). `uteke` without the binary falls back to `local`
//!   with a note.
//! - `TOLE_SKILLS_PINNED`: comma-separated skill names always listed
//!   (at most 5).
//! - `TOLE_SKILL_RECALL_K`: skills recalled per task, 0 to 10 (default 5).
//! - `TOLE_SKILL_FLOOR`: cosine similarity floor, 0.0 to 1.0 (default
//!   0.35, a starting value to calibrate with `tole skills search` on a
//!   real store).
//!
//! Any other value is a startup error. The active `--profile` name scopes
//! the recall (skills indexed for other profiles are not recalled).
//!
//! Every query sent to the index (the recall brief, a model-written
//! `skill_search` query, a `tole skills search` query) goes through the
//! process [`SecretSet`] first: `uteke doc search` may forward it to a
//! cloud store, so a known secret value never leaves in a query.

use std::path::Path;
use std::sync::Arc;
use tole_core::secrets::SecretSet;
use tole_core::skill_index::{
    select, selection_block, Candidate, RecallQuery, SkillEntry, SkillFilter, SkillIndex,
    SkillSearchTool, DEFAULT_RECALL_K, DEFAULT_SIMILARITY_FLOOR, MAX_RECALL_K,
};
use tole_core::skills::Gate;
use tole_core::tool::ToolRegistry;

pub const INDEX_ENV: &str = "TOLE_SKILL_INDEX";
pub const PINNED_ENV: &str = "TOLE_SKILLS_PINNED";
pub const K_ENV: &str = "TOLE_SKILL_RECALL_K";
pub const FLOOR_ENV: &str = "TOLE_SKILL_FLOOR";

/// Which index decides what a task is shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum IndexMode {
    /// Every discovered skill is listed (the pre-#590 behavior).
    #[default]
    Local,
    /// Pinned core + top K by meaning from the `uteke doc` index.
    Uteke,
}

/// The recall settings, from the environment.
#[derive(Debug, Clone, PartialEq)]
pub struct RecallSettings {
    pub mode: IndexMode,
    pub pinned: Vec<String>,
    pub k: usize,
    pub floor: f32,
}

impl Default for RecallSettings {
    fn default() -> Self {
        Self {
            mode: IndexMode::Local,
            pinned: Vec::new(),
            k: DEFAULT_RECALL_K,
            floor: DEFAULT_SIMILARITY_FLOOR,
        }
    }
}

impl RecallSettings {
    /// Read the four variables; a blank variable counts as unset.
    pub fn from_env(env: &dyn Fn(&str) -> Option<String>) -> Result<Self, String> {
        let get = |k: &str| {
            env(k)
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };
        let mut s = Self::default();
        if let Some(v) = get(INDEX_ENV) {
            s.mode = match v.as_str() {
                "local" => IndexMode::Local,
                "uteke" => IndexMode::Uteke,
                _ => return Err(format!("{INDEX_ENV}={v:?}: expected local or uteke")),
            };
        }
        if let Some(v) = get(PINNED_ENV) {
            s.pinned = v
                .split(',')
                .map(str::trim)
                .filter(|n| !n.is_empty())
                .map(String::from)
                .collect();
        }
        if let Some(v) = get(K_ENV) {
            s.k = v
                .parse::<usize>()
                .ok()
                .filter(|k| *k <= MAX_RECALL_K)
                .ok_or_else(|| format!("{K_ENV}={v:?}: expected 0 to {MAX_RECALL_K}"))?;
        }
        if let Some(v) = get(FLOOR_ENV) {
            s.floor = v
                .parse::<f32>()
                .ok()
                .filter(|f| (0.0..=1.0).contains(f))
                .ok_or_else(|| format!("{FLOOR_ENV}={v:?}: expected 0.0 to 1.0"))?;
        }
        Ok(s)
    }
}

/// What one session needs to choose its skills.
#[derive(Clone, Default)]
pub struct SkillRecall {
    pub settings: RecallSettings,
    /// The owner-written task brief (the `run` prompt); `None` for an
    /// interactive session, which then gets its pinned core only.
    pub brief: Option<String>,
    /// The active profile's name (`--profile`): the scope filter. The
    /// recall query's role stays `None` until profiles carry a role.
    pub profile: Option<String>,
    /// The index for `uteke` mode; `None` = uteke unavailable.
    pub index: Option<Arc<dyn SkillIndex>>,
    /// Redacted from every query before it reaches the index.
    pub secrets: SecretSet,
    /// What may load (#591): every listed, recalled, searched or loaded
    /// skill goes through it. `Open` by default.
    pub gate: Gate,
    /// Where recalls and loads are counted for the curator (#593). `None`
    /// by default, so a test or an embedder never writes the user's ledger;
    /// [`SkillRecall::from_env`] sets the default one.
    pub usage: Option<tole_core::skill_curator::UsageLedger>,
}

/// An index whose queries are redacted first: a query may leave the
/// process (`uteke doc search` against a cloud store), so a known secret
/// value is replaced by its `«NAME»` marker before the inner index sees it.
pub struct RedactingIndex {
    inner: Arc<dyn SkillIndex>,
    secrets: SecretSet,
}

impl RedactingIndex {
    pub fn new(inner: Arc<dyn SkillIndex>, secrets: SecretSet) -> Self {
        Self { inner, secrets }
    }
}

impl SkillIndex for RedactingIndex {
    fn label(&self) -> &'static str {
        self.inner.label()
    }
    fn search(
        &self,
        query: &str,
        filter: &SkillFilter,
        k: usize,
    ) -> Result<Vec<Candidate>, String> {
        self.inner.search(&self.secrets.redact(query), filter, k)
    }
    /// The description leaves the process too: redacted, while the content
    /// hash stays the real file's.
    fn upsert(&self, entry: &SkillEntry) -> Result<(), String> {
        let entry = SkillEntry {
            description: self.secrets.redact(&entry.description).into_owned(),
            ..entry.clone()
        };
        self.inner.upsert(&entry)
    }
    fn retire(&self, name: &str) -> Result<(), String> {
        self.inner.retire(name)
    }
}

impl SkillRecall {
    /// From the environment, with the real `uteke` binary when it is on
    /// PATH.
    pub fn from_env(brief: Option<&str>) -> Result<Self, String> {
        let settings = RecallSettings::from_env(&crate::config_apply::real_env)?;
        #[cfg(feature = "shell-tools")]
        let index: Option<Arc<dyn SkillIndex>> = (settings.mode == IndexMode::Uteke
            && crate::tool_roster::binary_available("uteke"))
        .then(|| Arc::new(tole_core::skill_index::UtekeDocIndex::default()) as _);
        #[cfg(not(feature = "shell-tools"))]
        let index: Option<Arc<dyn SkillIndex>> = None;
        Ok(Self {
            settings,
            brief: brief.map(str::to_string),
            profile: crate::config_apply::profile_name(),
            index,
            secrets: crate::provider_chain::process_secrets(&crate::config_apply::real_env),
            gate: Gate::Open,
            usage: tole_core::skill_curator::UsageLedger::default_if_curated(),
        })
    }
}

/// The skills section for a session whose discovery root has at least one
/// skill, and the tools it needs (`load_skill`, plus `skill_search` in
/// `uteke` mode). Returns the block and the startup notes.
pub fn skills_section(
    recall: &SkillRecall,
    ws: Option<&Path>,
    registry: &mut ToolRegistry,
) -> Result<(String, Vec<String>), String> {
    let mut notes = tole_core::skills::listing_notes(ws);
    notes.extend(crate::skill_approve::startup_note(ws, &recall.gate));
    let index = match (recall.settings.mode, &recall.index) {
        (IndexMode::Uteke, Some(index)) => Some(index.clone()),
        (IndexMode::Uteke, None) => {
            notes.push(format!(
                "{INDEX_ENV}=uteke but the uteke binary is not available: listing every skill"
            ));
            None
        }
        (IndexMode::Local, _) => None,
    };
    let ws_buf = ws.map(Path::to_path_buf);
    registry
        .register(Box::new(
            tole_core::skills::LoadSkillTool::new(ws_buf.clone())
                .with_gate(recall.gate.clone())
                .with_usage(recall.usage.clone()),
        ))
        .map_err(|e| format!("registering load_skill: {e}"))?;
    let Some(index) = index else {
        return Ok((
            tole_core::skills::index_block_gated(ws, &recall.gate),
            notes,
        ));
    };
    let index: Arc<dyn SkillIndex> = Arc::new(RedactingIndex::new(index, recall.secrets.clone()));
    let filter = SkillFilter {
        profile: recall.profile.clone(),
        floor: recall.settings.floor,
    };
    // The role is `None`: profiles carry no role yet (#584).
    let query = recall
        .brief
        .as_deref()
        .and_then(|b| RecallQuery::new(b, None));
    let sel = select(
        index.as_ref(),
        ws,
        &recall.settings.pinned,
        query.as_ref(),
        &filter,
        recall.settings.k,
        &recall.gate,
    );
    notes.extend(sel.notes.iter().cloned());
    record_recalled(recall.usage.as_ref(), &sel);
    registry
        .register(Box::new(SkillSearchTool::new(
            index,
            ws_buf,
            filter,
            recall.gate.clone(),
        )))
        .map_err(|e| format!("registering skill_search: {e}"))?;
    Ok((selection_block(&sel), notes))
}

/// Count each recalled skill in the usage ledger (#593). A recall alone is
/// not use, so it never keeps a skill alive; it shows the curator which
/// skills are offered but never loaded. Best effort.
fn record_recalled(
    ledger: Option<&tole_core::skill_curator::UsageLedger>,
    sel: &tole_core::skill_index::Selection,
) {
    let Some(ledger) = ledger else {
        return;
    };
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    for (skill, _) in &sel.recalled {
        let _ = ledger.append(&tole_core::skill_curator::UsageRecord {
            t,
            skill: Some(skill.name.clone()),
            kind: tole_core::skill_curator::UsageKind::Recalled,
        });
    }
}

/// Upsert every skill discovered from `ws` (the same discovery as a
/// session) into `index`. Returns how many were indexed and one line per
/// skill that was not.
pub fn index_all(index: &dyn SkillIndex, ws: Option<&Path>) -> (usize, Vec<String>) {
    let mut done = 0;
    let mut failed = Vec::new();
    for skill in tole_core::skills::available_skills(ws) {
        let raw = tole_core::skills::skill_file(ws, &skill.name)
            .ok_or_else(|| "SKILL.md not found".to_string())
            .and_then(|p| {
                tole_core::skills::read_file_capped(&p, tole_core::skills::MAX_SKILL_FILE_BYTES)
                    .map_err(|e| format!("{}: {e}", p.display()))
            });
        let res = raw.and_then(|raw| index.upsert(&SkillEntry::from_local(&skill, &raw)));
        match res {
            Ok(()) => done += 1,
            Err(e) => failed.push(format!("{}: {e}", skill.name)),
        }
    }
    (done, failed)
}

/// The uteke-index actions of `tole skills` (shell-tools builds; the
/// approval actions are in [`crate::skill_approve`]).
#[cfg(feature = "shell-tools")]
#[derive(Debug, clap::Subcommand)]
pub enum IndexAction {
    /// Index every discovered skill in uteke (tag `skill`, slug
    /// `skill-<name>`): name, description, status `approved` and the
    /// SKILL.md content hash. The body is not sent.
    Index {
        /// Discovery root (default: the current directory).
        #[arg(long)]
        workspace: Option<std::path::PathBuf>,
    },
    /// Retire a skill in the uteke index: it stays on disk and in git and
    /// is never recalled. `tole skills index` approves it again.
    Retire { name: String },
    /// Search the uteke index the way a task does (with
    /// `TOLE_SKILL_FLOOR`), to check what a brief would recall.
    Search {
        query: String,
        #[arg(long, default_value_t = DEFAULT_RECALL_K)]
        limit: usize,
        /// Only skills in scope for this profile (plus unscoped ones).
        #[arg(long)]
        profile: Option<String>,
        /// Discovery root (default: the current directory).
        #[arg(long)]
        workspace: Option<std::path::PathBuf>,
    },
}

#[cfg(feature = "shell-tools")]
pub fn cli(action: IndexAction) -> anyhow::Result<()> {
    use anyhow::Context as _;
    if !crate::tool_roster::binary_available("uteke") {
        anyhow::bail!("the uteke binary is not on PATH; the skill index needs it");
    }
    let index = RedactingIndex::new(
        Arc::new(tole_core::skill_index::UtekeDocIndex::default()),
        crate::provider_chain::process_secrets(&crate::config_apply::real_env),
    );
    let root = |ws: Option<std::path::PathBuf>| -> anyhow::Result<std::path::PathBuf> {
        match ws {
            Some(p) => Ok(p),
            None => std::env::current_dir().context("reading the current directory"),
        }
    };
    match action {
        IndexAction::Index { workspace } => {
            let ws = root(workspace)?;
            let (done, failed) = index_all(&index, Some(&ws));
            println!("indexed {done} skill(s) in uteke");
            for f in &failed {
                eprintln!("tole: not indexed: {f}");
            }
            if !failed.is_empty() {
                anyhow::bail!("{} skill(s) not indexed", failed.len());
            }
        }
        IndexAction::Retire { name } => {
            index.retire(&name).map_err(anyhow::Error::msg)?;
            println!("retired {name}: it stays on disk and is no longer recalled");
        }
        IndexAction::Search {
            query,
            limit,
            profile,
            workspace,
        } => {
            let ws = root(workspace)?;
            let settings = RecallSettings::from_env(&crate::config_apply::real_env)
                .map_err(anyhow::Error::msg)?;
            let filter = SkillFilter {
                profile: profile.or_else(crate::config_apply::profile_name),
                floor: settings.floor,
            };
            let hits = index
                .search(&query, &filter, limit.min(MAX_RECALL_K))
                .map_err(anyhow::Error::msg)?;
            if hits.is_empty() {
                println!("no skill above the floor {:.2}", filter.floor);
            }
            for c in hits {
                match tole_core::skills::load_skill(Some(&ws), &c.name) {
                    Ok(s) => println!("{:.3}  {}", c.score, s.index_line()),
                    Err(_) => println!(
                        "{:.3}  {} (indexed, not on disk: never shown)",
                        c.score, c.name
                    ),
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tole_core::skill_index::{Candidate, SkillEntry};

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let pairs: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k| pairs.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone())
    }

    #[test]
    fn settings_default_to_local_and_reject_bad_values() {
        assert_eq!(
            RecallSettings::from_env(&env(&[])).unwrap(),
            RecallSettings::default()
        );
        let s = RecallSettings::from_env(&env(&[
            (INDEX_ENV, " uteke "),
            (PINNED_ENV, "a, b,,c"),
            (K_ENV, "3"),
            (FLOOR_ENV, "0.5"),
        ]))
        .unwrap();
        assert_eq!(s.mode, IndexMode::Uteke);
        assert_eq!(s.pinned, ["a", "b", "c"]);
        assert_eq!((s.k, s.floor), (3, 0.5));
        // Blank = unset.
        assert_eq!(
            RecallSettings::from_env(&env(&[(INDEX_ENV, " "), (K_ENV, "")])).unwrap(),
            RecallSettings::default()
        );
        for bad in [
            &[(INDEX_ENV, "cloud")][..],
            &[(K_ENV, "11")],
            &[(K_ENV, "-1")],
            &[(FLOOR_ENV, "1.5")],
            &[(FLOOR_ENV, "x")],
        ] {
            assert!(RecallSettings::from_env(&env(bad)).is_err(), "{bad:?}");
        }
    }

    struct Fixed(Vec<&'static str>);
    impl SkillIndex for Fixed {
        fn label(&self) -> &'static str {
            "fixed"
        }
        fn search(&self, _: &str, _: &SkillFilter, k: usize) -> Result<Vec<Candidate>, String> {
            Ok(self
                .0
                .iter()
                .take(k)
                .map(|n| Candidate {
                    name: n.to_string(),
                    score: 0.9,
                })
                .collect())
        }
        fn upsert(&self, _: &SkillEntry) -> Result<(), String> {
            Ok(())
        }
        fn retire(&self, _: &str) -> Result<(), String> {
            Ok(())
        }
    }

    fn ws(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("tole-skrecall-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for n in ["alpha", "beta", "gamma"] {
            let d = dir.join("skills").join(n);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(
                d.join("SKILL.md"),
                format!("---\nname: {n}\ndescription: \"{n} d\"\n---\nb"),
            )
            .unwrap();
        }
        dir
    }

    #[test]
    fn local_mode_lists_everything_like_before() {
        let dir = ws("local");
        let mut reg = ToolRegistry::new();
        let recall = SkillRecall {
            index: Some(Arc::new(Fixed(vec!["beta"]))),
            ..SkillRecall::default()
        };
        let (block, notes) = skills_section(&recall, Some(&dir), &mut reg).unwrap();
        assert_eq!(block, tole_core::skills::index_block(Some(&dir)));
        assert!(notes.is_empty());
        assert!(reg.get("load_skill").is_some());
        assert!(reg.get("skill_search").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn uteke_mode_shows_pinned_plus_recalled_and_registers_skill_search() {
        let dir = ws("uteke");
        let mut reg = ToolRegistry::new();
        let recall = SkillRecall {
            settings: RecallSettings {
                mode: IndexMode::Uteke,
                pinned: vec!["alpha".into()],
                k: 1,
                ..RecallSettings::default()
            },
            brief: Some("ship the release".into()),
            profile: None,
            index: Some(Arc::new(Fixed(vec!["beta", "gamma"]))),
            secrets: SecretSet::default(),
            gate: Gate::Open,
            usage: None,
        };
        let (block, _) = skills_section(&recall, Some(&dir), &mut reg).unwrap();
        assert!(
            block.contains("- alpha — alpha d\n- beta — beta d\n"),
            "{block}"
        );
        assert!(!block.contains("gamma"), "K = 1: {block}");
        assert!(reg.get("skill_search").is_some());
        assert!(reg.get("load_skill").is_some());
        // An interactive session (no brief) gets the pinned core only.
        let mut reg = ToolRegistry::new();
        let no_brief = SkillRecall {
            brief: None,
            ..recall
        };
        let (block, _) = skills_section(&no_brief, Some(&dir), &mut reg).unwrap();
        assert!(
            block.contains("- alpha") && !block.contains("beta"),
            "{block}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Records every upsert; refuses one name.
    struct Recording(std::sync::Mutex<Vec<SkillEntry>>);
    impl SkillIndex for Recording {
        fn label(&self) -> &'static str {
            "rec"
        }
        fn search(&self, _: &str, _: &SkillFilter, _: usize) -> Result<Vec<Candidate>, String> {
            Ok(vec![])
        }
        fn upsert(&self, e: &SkillEntry) -> Result<(), String> {
            if e.name == "gamma" {
                return Err("refused".into());
            }
            self.0.lock().unwrap().push(e.clone());
            Ok(())
        }
        fn retire(&self, _: &str) -> Result<(), String> {
            Ok(())
        }
    }

    #[test]
    fn index_all_upserts_each_discovered_skill_with_its_file_hash() {
        let dir = ws("indexall");
        let rec = Recording(Default::default());
        let (done, failed) = index_all(&rec, Some(&dir));
        assert_eq!(done, 2);
        assert_eq!(failed, ["gamma: refused"]);
        let got = rec.0.lock().unwrap().clone();
        assert_eq!(
            got.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(),
            ["alpha", "beta"]
        );
        let raw = std::fs::read(dir.join("skills/alpha/SKILL.md")).unwrap();
        assert_eq!(
            got[0].content_hash,
            tole_core::skill_index::content_hash(&raw)
        );
        assert_eq!(got[0].description, "alpha d");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Records the query and filter of every search.
    #[derive(Default)]
    struct Queries(std::sync::Mutex<Vec<(String, Option<String>)>>);
    impl SkillIndex for Queries {
        fn label(&self) -> &'static str {
            "queries"
        }
        fn search(&self, q: &str, f: &SkillFilter, _: usize) -> Result<Vec<Candidate>, String> {
            self.0
                .lock()
                .unwrap()
                .push((q.to_string(), f.profile.clone()));
            Ok(vec![])
        }
        fn upsert(&self, _: &SkillEntry) -> Result<(), String> {
            Ok(())
        }
        fn retire(&self, _: &str) -> Result<(), String> {
            Ok(())
        }
    }

    /// Review note on #616: an upserted entry leaves the process too; its
    /// description is redacted, its content hash stays the real file's.
    #[test]
    fn upserted_descriptions_are_redacted_and_keep_their_hash() {
        let secret = "ghp_0123456789abcdefSECRET";
        let rec = Arc::new(Recording(Default::default()));
        let idx = RedactingIndex::new(rec.clone(), SecretSet::default().with("GH_TOKEN", secret));
        let entry = SkillEntry {
            name: "deploy".into(),
            description: format!("push with {secret}"),
            profiles: vec![],
            status: tole_core::skill_index::SkillStatus::Approved,
            content_hash: "sha256:real".into(),
        };
        idx.upsert(&entry).unwrap();
        let got = rec.0.lock().unwrap()[0].clone();
        assert!(!got.description.contains(secret), "{got:?}");
        assert!(got.description.contains("«GH_TOKEN»"), "{got:?}");
        assert_eq!(got.content_hash, "sha256:real");
        assert_eq!(got.name, "deploy");
    }

    /// A query may leave the process (uteke against a cloud store): the
    /// brief and a model-written `skill_search` query reach the index with
    /// every known secret redacted, scoped to the active profile.
    #[test]
    fn queries_reach_the_index_redacted_and_profile_scoped() {
        let dir = ws("redact");
        let secret = "sk-live-0123456789abcdef";
        let seen = Arc::new(Queries::default());
        let recall = SkillRecall {
            settings: RecallSettings {
                mode: IndexMode::Uteke,
                ..RecallSettings::default()
            },
            brief: Some(format!("deploy with key {secret} today")),
            profile: Some("coder".into()),
            index: Some(seen.clone()),
            secrets: SecretSet::default().with("DEPLOY_TOKEN", secret),
            gate: Gate::Open,
            usage: None,
        };
        let mut reg = ToolRegistry::new();
        skills_section(&recall, Some(&dir), &mut reg).unwrap();
        reg.get("skill_search")
            .unwrap()
            .execute(serde_json::json!({ "query": format!("auth {secret}") }))
            .unwrap();
        let got = seen.0.lock().unwrap().clone();
        assert_eq!(got.len(), 2, "{got:?}");
        for (q, profile) in &got {
            assert!(!q.contains(secret), "{q}");
            assert!(q.contains("«DEPLOY_TOKEN»"), "{q}");
            assert_eq!(profile.as_deref(), Some("coder"));
        }
        assert!(got[0].0.starts_with("deploy with key"), "{got:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// #591: the approval gate covers both modes. Local: the full listing
    /// names unapproved skills only. Uteke: pinned, recalled and searched
    /// skills that are not approved are named as unavailable, and
    /// load_skill refuses them.
    #[test]
    fn the_approval_gate_covers_local_and_uteke_sessions() {
        use tole_core::skill_approval::{read_package, Approvals};
        let dir = ws("gate");
        let mut ok = Approvals::default();
        ok.approve(&read_package(Some(&dir), "alpha").unwrap(), 1);
        let gate = Gate::Approved(Arc::new(ok));
        // Local mode.
        let mut reg = ToolRegistry::new();
        let local = SkillRecall {
            gate: gate.clone(),
            ..SkillRecall::default()
        };
        let (block, _) = skills_section(&local, Some(&dir), &mut reg).unwrap();
        assert!(block.contains("- alpha — alpha d\n"), "{block}");
        assert!(block.contains("- beta (unavailable"), "{block}");
        assert!(!block.contains("beta d"), "{block}");
        let load = reg.get("load_skill").unwrap();
        assert!(load.execute(serde_json::json!({"name": "beta"})).is_err());
        // Uteke mode: beta is pinned and recalled, gamma recalled.
        let mut reg = ToolRegistry::new();
        let uteke = SkillRecall {
            settings: RecallSettings {
                mode: IndexMode::Uteke,
                pinned: vec!["beta".into()],
                ..RecallSettings::default()
            },
            brief: Some("task".into()),
            index: Some(Arc::new(Fixed(vec!["gamma", "alpha"]))),
            gate,
            ..SkillRecall::default()
        };
        let (block, notes) = skills_section(&uteke, Some(&dir), &mut reg).unwrap();
        assert!(block.contains("- alpha — alpha d\n"), "{block}");
        assert!(block.contains("cannot be loaded: beta, gamma"), "{block}");
        assert!(
            !block.contains("beta d") && !block.contains("gamma d"),
            "{block}"
        );
        assert!(
            notes.iter().any(|n| n.contains("not approved")),
            "{notes:?}"
        );
        let out = reg
            .get("skill_search")
            .unwrap()
            .execute(serde_json::json!({"query": "x"}))
            .unwrap();
        assert_eq!(
            out["skills"][0]["available"],
            serde_json::json!(false),
            "{out}"
        );
        assert!(!out.to_string().contains("gamma d"), "{out}");
        assert!(reg
            .get("load_skill")
            .unwrap()
            .execute(serde_json::json!({"name": "gamma"}))
            .is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An oversized SKILL.md is left out of the session listing with a
    /// startup note, and `tole skills index` never reads or indexes it.
    #[test]
    fn an_oversized_skill_is_noted_and_never_indexed() {
        let dir = ws("cap");
        std::fs::OpenOptions::new()
            .write(true)
            .open(dir.join("skills/beta/SKILL.md"))
            .unwrap()
            .set_len(tole_core::skills::MAX_SKILL_FILE_BYTES + 1)
            .unwrap();
        let mut reg = ToolRegistry::new();
        let (block, notes) = skills_section(&SkillRecall::default(), Some(&dir), &mut reg).unwrap();
        assert!(!block.contains("beta"), "{block}");
        assert!(
            notes.iter().any(|n| n.contains("\"beta\" skipped")),
            "{notes:?}"
        );
        let rec = Recording(Default::default());
        let (done, _) = index_all(&rec, Some(&dir));
        let got: Vec<String> = rec
            .0
            .lock()
            .unwrap()
            .iter()
            .map(|e| e.name.clone())
            .collect();
        assert_eq!(
            got,
            ["alpha"],
            "gamma is refused by Recording, beta is skipped"
        );
        assert_eq!(done, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn uteke_mode_without_the_binary_falls_back_with_a_note() {
        let dir = ws("nobin");
        let mut reg = ToolRegistry::new();
        let recall = SkillRecall {
            settings: RecallSettings {
                mode: IndexMode::Uteke,
                ..RecallSettings::default()
            },
            ..SkillRecall::default()
        };
        let (block, notes) = skills_section(&recall, Some(&dir), &mut reg).unwrap();
        assert_eq!(block, tole_core::skills::index_block(Some(&dir)));
        assert!(notes[0].contains("not available"), "{notes:?}");
        assert!(reg.get("skill_search").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
