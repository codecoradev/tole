//! The `SkillIndex` seam (#590, epic #589): which skills a task sees.
//!
//! Today's `skills::index_block` lists every discovered skill in every
//! prompt, so cost and selection quality degrade as the library grows. This
//! module puts a seam between "which skills exist" and "which skills this
//! task is shown" (design: `docs/skills-registry-design.md` §4.1, §4.3,
//! §6.2, §7):
//!
//! - [`SkillIndex`]: `search(query, filter, k)`, `upsert`, `retire`.
//!   Adapters: [`LocalIndex`] (the skills directories, lexical match; the
//!   fallback without uteke), `UtekeSkillIndex` (`uteke skill search|list|
//!   show`, uteke >= 0.24) and `UtekeDocIndex` (`uteke doc`, tag `skill`,
//!   recall by meaning; the pre-0.24 surface); the uteke adapters are
//!   `shell-tools` builds only).
//! - [`select`]: per task, a **pinned core** (at most [`MAX_PINNED`]) plus
//!   the **top K recalled by meaning** (default [`DEFAULT_RECALL_K`], at
//!   most [`MAX_RECALL_K`]) above a similarity floor. Fewer, or zero, is
//!   fine: an irrelevant skill is not free.
//! - [`RecallQuery`]: built from the owner-written task brief and the
//!   profile role only, never from tool output (a web page must not steer
//!   which instructions get loaded).
//! - [`SkillSearchTool`]: the ReadOnly `skill_search` tool for on-demand
//!   discovery.
//!
//! Trust: the index only *finds* names. Every listed description and every
//! loaded body comes from the local skill files ([`crate::skills`]); a name
//! the index returns that has no local skill is dropped. An index entry can
//! therefore never inject text into the prompt (§4.2). Under an approval
//! [`Gate`] (#591) only owner-approved bytes load; a selected or searched
//! skill that is not approved is named as unavailable, with no description.

#[cfg(test)]
use crate::skills::load_skill;
use crate::skills::{available_skills, Gate, LoadError, Skill};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Default number of skills recalled by meaning per task (owner decision).
pub const DEFAULT_RECALL_K: usize = 5;
/// Upper bound on the recalled skills per task.
pub const MAX_RECALL_K: usize = 10;
/// Upper bound on the pinned core per task.
pub const MAX_PINNED: usize = 5;
/// Default similarity floor (cosine) for recall by meaning. A starting
/// value, to be calibrated on the owner's library and embedder.
pub const DEFAULT_SIMILARITY_FLOOR: f32 = 0.35;
/// Longest recall query passed to an index, in chars.
pub const MAX_QUERY_CHARS: usize = 1000;
/// The uteke document tag every indexed skill carries.
pub const SKILL_TAG: &str = "skill";
/// Slug prefix of an indexed skill's uteke document: `skill-<name>`.
pub const SLUG_PREFIX: &str = "skill-";

/// A skill the index returned for a query. `score` is the adapter's own
/// relevance (cosine for uteke, a lexical overlap for the local index).
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub name: String,
    pub score: f32,
}

/// Metadata filters applied by the index, besides meaning.
#[derive(Debug, Clone, PartialEq)]
pub struct SkillFilter {
    /// The active profile, if any: a skill scoped to profiles is returned
    /// only when this is one of them. An unscoped skill matches everyone.
    pub profile: Option<String>,
    /// Similarity floor; candidates scoring below it are dropped.
    pub floor: f32,
}

impl Default for SkillFilter {
    fn default() -> Self {
        Self {
            profile: None,
            floor: DEFAULT_SIMILARITY_FLOOR,
        }
    }
}

/// Lifecycle status recorded in the index. Only `Approved` is recalled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillStatus {
    Approved,
    Retired,
}

impl SkillStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            SkillStatus::Approved => "approved",
            SkillStatus::Retired => "retired",
        }
    }
}

/// What the index stores about one skill: never the body.
#[derive(Debug, Clone, PartialEq)]
pub struct SkillEntry {
    pub name: String,
    pub description: String,
    /// Profiles the skill is scoped to; empty = every profile.
    pub profiles: Vec<String>,
    pub status: SkillStatus,
    /// `sha256:<hex>` of the SKILL.md bytes the entry was built from.
    pub content_hash: String,
}

impl SkillEntry {
    /// An approved, unscoped entry for a local skill file's raw bytes.
    pub fn from_local(skill: &Skill, raw: &[u8]) -> Self {
        Self {
            name: skill.name.clone(),
            description: skill.description.clone(),
            profiles: Vec::new(),
            status: SkillStatus::Approved,
            content_hash: content_hash(raw),
        }
    }
}

/// `sha256:<lowercase hex>` of a skill file's bytes.
pub fn content_hash(raw: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(raw);
    let mut out = String::with_capacity(7 + 64);
    out.push_str("sha256:");
    for b in digest.iter() {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// The seam. Adapters find skill NAMES; [`select`] and [`SkillSearchTool`]
/// resolve them against the local skill files.
pub trait SkillIndex: Send + Sync {
    /// Short label for notes and `tole skills` output (`local`, `uteke`).
    fn label(&self) -> &'static str;
    /// Up to `k` candidates for `query`, best first, already filtered by
    /// `filter` (status, profile scope, floor).
    fn search(&self, query: &str, filter: &SkillFilter, k: usize)
        -> Result<Vec<Candidate>, String>;
    /// Create or update the index entry for one skill.
    fn upsert(&self, entry: &SkillEntry) -> Result<(), String>;
    /// Mark a skill retired: kept on disk and in git, never recalled.
    fn retire(&self, name: &str) -> Result<(), String>;
}

/// Validate and normalise a query before it reaches any index: trimmed,
/// non-empty, not flag-shaped (it becomes an argv element for uteke), and
/// capped at [`MAX_QUERY_CHARS`].
pub fn clean_query(query: &str) -> Result<String, String> {
    let q = query.trim();
    if q.is_empty() {
        return Err("query must not be empty".into());
    }
    if q.starts_with('-') {
        return Err("query must not start with '-'".into());
    }
    Ok(q.chars().take(MAX_QUERY_CHARS).collect())
}

/// The recall query for a task: the owner-written brief plus the profile
/// role. Constructed only from those two inputs, so tool output can never
/// steer which skills are recalled.
#[derive(Debug, Clone, PartialEq)]
pub struct RecallQuery(String);

impl RecallQuery {
    /// `None` when the brief is blank (nothing to recall for).
    pub fn new(brief: &str, role: Option<&str>) -> Option<Self> {
        let brief = brief.trim();
        if brief.is_empty() {
            return None;
        }
        let text = match role.map(str::trim).filter(|r| !r.is_empty()) {
            Some(r) => format!("{r}: {brief}"),
            None => brief.to_string(),
        };
        // A brief that starts with '-' is still a valid task; only the
        // argv form must not. Prefix it rather than refuse it.
        let text = if text.starts_with('-') {
            format!("task {text}")
        } else {
            text
        };
        Some(Self(text.chars().take(MAX_QUERY_CHARS).collect()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The local skills directories as an index: lexical overlap between the
/// query's words and a skill's name + description. The fallback without
/// uteke; its scores are not cosine, so the floor does not apply.
#[derive(Debug, Clone, Default)]
pub struct LocalIndex {
    pub workspace: Option<PathBuf>,
}

impl LocalIndex {
    pub fn new(workspace: Option<PathBuf>) -> Self {
        Self { workspace }
    }
}

fn words(s: &str) -> Vec<String> {
    s.split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.chars().count() >= 3)
        .map(str::to_lowercase)
        .collect()
}

impl SkillIndex for LocalIndex {
    fn label(&self) -> &'static str {
        "local"
    }

    fn search(
        &self,
        query: &str,
        _filter: &SkillFilter,
        k: usize,
    ) -> Result<Vec<Candidate>, String> {
        let q = clean_query(query)?;
        let mut qw = words(&q);
        qw.sort();
        qw.dedup();
        if qw.is_empty() {
            return Ok(Vec::new());
        }
        let mut out: Vec<Candidate> = available_skills(self.workspace.as_deref())
            .into_iter()
            .filter_map(|s| {
                let text = words(&format!("{} {}", s.name, s.description));
                let hits = qw.iter().filter(|w| text.contains(w)).count();
                (hits > 0).then(|| Candidate {
                    name: s.name,
                    score: hits as f32 / qw.len() as f32,
                })
            })
            .collect();
        out.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.name.cmp(&b.name)));
        out.truncate(k);
        Ok(out)
    }

    fn upsert(&self, _entry: &SkillEntry) -> Result<(), String> {
        // The skills directories ARE this index.
        Ok(())
    }

    fn retire(&self, name: &str) -> Result<(), String> {
        Err(format!(
            "the local index cannot retire {name:?}: it lists every skill directory; \
             move the directory out, or use the uteke index"
        ))
    }
}

/// The skills one task is shown, fixed at task start (prompt cache).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Selection {
    pub pinned: Vec<Skill>,
    /// Recalled by meaning, best first, with the index's score.
    pub recalled: Vec<(Skill, f32)>,
    /// Pinned or recalled names the gate refused as not approved: shown by
    /// name only, never loaded.
    pub unavailable: Vec<String>,
    /// One-line notes for the startup log (missing pins, index errors).
    pub notes: Vec<String>,
}

/// Pick the skills for one task: the pinned core (in the given order, at
/// most [`MAX_PINNED`]) plus up to `k` (at most [`MAX_RECALL_K`]) recalled
/// for `query`. Every skill is loaded from the local files; a name with no
/// local skill is dropped. An index failure is a note, not an error: the
/// task still gets its pinned core and the `skill_search` tool. Every skill
/// goes through `gate`; one it refuses as not approved is listed in
/// [`Selection::unavailable`] and does not use up the K.
pub fn select(
    index: &dyn SkillIndex,
    workspace: Option<&Path>,
    pinned: &[String],
    query: Option<&RecallQuery>,
    filter: &SkillFilter,
    k: usize,
    gate: &Gate,
) -> Selection {
    let mut sel = Selection::default();
    let mut seen = std::collections::HashSet::new();
    if pinned.len() > MAX_PINNED {
        sel.notes.push(format!(
            "{} pinned skills given; only the first {MAX_PINNED} are pinned",
            pinned.len()
        ));
    }
    for name in pinned.iter().take(MAX_PINNED) {
        if !seen.insert(name.clone()) {
            continue;
        }
        match gate.load(workspace, name) {
            Ok(s) => sel.pinned.push(s),
            Err(e @ LoadError::NotApproved { .. }) => {
                sel.notes.push(format!("pinned skill not loadable: {e}"));
                sel.unavailable.push(name.clone());
            }
            Err(e) => sel.notes.push(format!("pinned skill skipped: {e}")),
        }
    }
    let k = k.min(MAX_RECALL_K);
    let Some(query) = query.filter(|_| k > 0) else {
        return sel;
    };
    // Ask for extra so pinned or locally-missing names do not eat the K.
    match index.search(query.as_str(), filter, k + sel.pinned.len()) {
        Ok(cands) => {
            let mut refused = 0;
            for c in cands {
                if sel.recalled.len() >= k {
                    break;
                }
                if !seen.insert(c.name.clone()) {
                    continue;
                }
                match gate.load(workspace, &c.name) {
                    Ok(s) => sel.recalled.push((s, c.score)),
                    Err(LoadError::NotApproved { .. }) if refused < k => {
                        refused += 1;
                        sel.unavailable.push(c.name);
                    }
                    Err(_) => {}
                }
            }
        }
        Err(e) => sel.notes.push(format!(
            "skill recall ({}) unavailable, pinned skills only: {e}",
            index.label()
        )),
    }
    sel
}

/// The system-prompt block for a [`Selection`]. Always names
/// `skill_search`, so a task with nothing preselected can still find one.
pub fn selection_block(sel: &Selection) -> String {
    let mut out = String::from("\n\n# Skills\n");
    if sel.pinned.is_empty() && sel.recalled.is_empty() {
        out.push_str(
            "No skill was preselected for this task. Call skill_search with a short \
             description of what you need to find one, then load_skill to load it.\n",
        );
        push_unavailable(&mut out, &sel.unavailable);
        return out;
    }
    out.push_str(
        "Selected for this task (call load_skill with a name to load one; call \
         skill_search to find others):\n",
    );
    for s in &sel.pinned {
        out.push_str(&format!("- {}\n", s.index_line()));
    }
    for (s, _) in &sel.recalled {
        out.push_str(&format!("- {}\n", s.index_line()));
    }
    push_unavailable(&mut out, &sel.unavailable);
    out
}

fn push_unavailable(out: &mut String, names: &[String]) {
    if !names.is_empty() {
        out.push_str(&format!(
            "Not approved by the owner, so they cannot be loaded: {}\n",
            names.join(", ")
        ));
    }
}

/// The ReadOnly `skill_search` tool: on-demand discovery through the
/// configured index (the Tool Search pattern). Returns names and
/// descriptions from the local skill files; bodies still come through
/// `load_skill`. A hit the gate refuses as not approved comes back as
/// `{"name", "available": false}`, with no description.
#[derive(Clone)]
pub struct SkillSearchTool {
    index: Arc<dyn SkillIndex>,
    workspace: Option<PathBuf>,
    filter: SkillFilter,
    gate: Gate,
}

impl SkillSearchTool {
    pub fn new(
        index: Arc<dyn SkillIndex>,
        workspace: Option<PathBuf>,
        filter: SkillFilter,
        gate: Gate,
    ) -> Self {
        Self {
            index,
            workspace,
            filter,
            gate,
        }
    }
}

impl crate::tool::Tool for SkillSearchTool {
    fn name(&self) -> &str {
        "skill_search"
    }

    fn risk(&self) -> crate::tool::Risk {
        crate::tool::Risk::ReadOnly
    }

    fn summary(&self) -> String {
        "Search the skill library by meaning; returns skill names and descriptions to load with load_skill.".into()
    }

    fn describe(&self, input: &Value) -> String {
        let q = input.get("query").and_then(Value::as_str).unwrap_or("?");
        format!("search skills ({}): {q:?}", self.index.label())
    }

    fn spec(&self) -> Option<Value> {
        Some(json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "What the skill should help with" },
                "limit": { "type": "integer", "description": "Max results (default 5, max 10)" }
            },
            "required": ["query"]
        }))
    }

    fn execute(&self, input: Value) -> Result<Value, String> {
        let query = input
            .get("query")
            .and_then(Value::as_str)
            .ok_or_else(|| "input must be {\"query\": \"...\"}".to_string())?;
        let query = clean_query(query)?;
        let limit = input
            .get("limit")
            .and_then(Value::as_u64)
            .map_or(DEFAULT_RECALL_K, |n| n as usize)
            .clamp(1, MAX_RECALL_K);
        let cands = self.index.search(&query, &self.filter, limit)?;
        let skills: Vec<Value> = cands
            .into_iter()
            .filter_map(
                |c| match self.gate.load(self.workspace.as_deref(), &c.name) {
                    Ok(s) => Some(
                        json!({ "name": s.name, "description": s.description, "score": c.score }),
                    ),
                    Err(LoadError::NotApproved { name, .. }) => Some(json!({
                        "name": name,
                        "available": false,
                        "reason": "not approved by the owner",
                        "score": c.score
                    })),
                    Err(LoadError::Other(_)) => None,
                },
            )
            .collect();
        Ok(json!({ "index": self.index.label(), "skills": skills }))
    }
}

#[cfg(feature = "shell-tools")]
pub use uteke_doc::UtekeDocIndex;
#[cfg(feature = "shell-tools")]
pub use uteke_skill::{SkillMeta, UtekeSkillIndex};

/// Adapter 1: `uteke doc` (tag `skill`, slug `skill-<name>`, metadata
/// name / description / profiles / status / content hash). Shells out to
/// the uteke CLI like the other uteke integrations.
#[cfg(feature = "shell-tools")]
mod uteke_doc {
    use super::*;
    use crate::subprocess::{run_with_timeout, run_with_timeout_stdin, SUBPROCESS_TIMEOUT};
    use std::process::Command;

    /// Upper bound on candidates fetched from `uteke doc search` per call.
    const MAX_FETCH: usize = 30;

    #[derive(Debug, Clone)]
    pub struct UtekeDocIndex {
        bin: PathBuf,
    }

    impl Default for UtekeDocIndex {
        fn default() -> Self {
            Self::new("uteke")
        }
    }

    /// A skill name usable in a uteke slug: `[a-z0-9-]`, not dash-led.
    fn slug_name_ok(name: &str) -> bool {
        !name.is_empty()
            && !name.starts_with('-')
            && name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    }

    fn slug_for(name: &str) -> Result<String, String> {
        if slug_name_ok(name) {
            Ok(format!("{SLUG_PREFIX}{name}"))
        } else {
            Err(format!(
                "skill name {name:?} cannot be indexed in uteke: use [a-z0-9-], not starting with '-'"
            ))
        }
    }

    impl UtekeDocIndex {
        pub fn new(bin: impl Into<PathBuf>) -> Self {
            Self { bin: bin.into() }
        }

        pub(super) fn run(&self, args: &[&str], stdin: Option<&[u8]>) -> Result<String, String> {
            let mut cmd = Command::new(&self.bin);
            crate::subprocess::scrub_env_for(&mut cmd, "skills");
            cmd.args(args);
            let out = match stdin {
                Some(bytes) => run_with_timeout_stdin(&mut cmd, SUBPROCESS_TIMEOUT, bytes)?,
                None => run_with_timeout(&mut cmd, SUBPROCESS_TIMEOUT)?,
            };
            if !out.status.success() {
                let stderr = String::from_utf8_lossy(&out.stderr);
                let snippet: String = stderr.chars().take(500).collect();
                return Err(format!(
                    "uteke {} exited {}: {snippet}",
                    args[..2].join(" "),
                    out.status
                ));
            }
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        }

        fn get(&self, slug: &str) -> Result<Value, String> {
            let out = self.run(&["doc", "get", slug, "--json"], None)?;
            serde_json::from_str(&out).map_err(|e| format!("uteke doc get: invalid JSON: {e}"))
        }

        /// Does this stored document pass the metadata filters?
        fn admits(doc: &Value, name: &str, filter: &SkillFilter) -> bool {
            let tagged = doc["tags"]
                .as_array()
                .is_some_and(|t| t.iter().any(|v| v.as_str() == Some(SKILL_TAG)));
            let meta = &doc["metadata"];
            let approved = meta["status"].as_str() == Some(SkillStatus::Approved.as_str());
            let named = meta["name"].as_str() == Some(name);
            let scoped: Vec<&str> = meta["profiles"]
                .as_array()
                .map(|a| a.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            let in_scope = scoped.is_empty()
                || filter
                    .profile
                    .as_deref()
                    .is_some_and(|p| scoped.contains(&p));
            tagged && approved && named && in_scope
        }
    }

    impl SkillIndex for UtekeDocIndex {
        fn label(&self) -> &'static str {
            "uteke"
        }

        fn search(
            &self,
            query: &str,
            filter: &SkillFilter,
            k: usize,
        ) -> Result<Vec<Candidate>, String> {
            let q = clean_query(query)?;
            if k == 0 {
                return Ok(Vec::new());
            }
            // Semantic mode: the score is a cosine similarity, so the floor
            // means something (hybrid scores are rank fusions).
            let fetch = (k * 3).min(MAX_FETCH).to_string();
            let out = self.run(
                &[
                    "doc", "search", &q, "--mode", "semantic", "--limit", &fetch, "--json",
                ],
                None,
            )?;
            let hits: Value = serde_json::from_str(&out)
                .map_err(|e| format!("uteke doc search: invalid JSON: {e}"))?;
            let hits = hits
                .as_array()
                .ok_or("uteke doc search: output is not a JSON array")?;
            let mut out = Vec::new();
            for (i, h) in hits.iter().enumerate() {
                let slug = h["document"]["slug"]
                    .as_str()
                    .ok_or_else(|| format!("uteke doc search: hit #{} has no slug", i + 1))?;
                let score = h["score"]
                    .as_f64()
                    .ok_or_else(|| format!("uteke doc search: hit #{} has no score", i + 1))?
                    as f32;
                let Some(name) = slug.strip_prefix(SLUG_PREFIX).filter(|n| slug_name_ok(n)) else {
                    continue; // not a skill document
                };
                if score < filter.floor || out.iter().any(|c: &Candidate| c.name == name) {
                    continue;
                }
                // Metadata filters need the stored document; a document
                // that cannot be read is not recalled.
                match self.get(slug) {
                    Ok(doc) if Self::admits(&doc, name, filter) => out.push(Candidate {
                        name: name.to_string(),
                        score,
                    }),
                    _ => continue,
                }
                if out.len() >= k {
                    break;
                }
            }
            Ok(out)
        }

        fn upsert(&self, entry: &SkillEntry) -> Result<(), String> {
            let slug = slug_for(&entry.name)?;
            // The searchable text is the name and description only; the
            // body stays local (§4.2).
            let content = format!("# {}\n\n{}\n", entry.name, entry.description);
            self.run(
                &["doc", "create", &slug, "--file", "-", "--tags", SKILL_TAG],
                Some(content.as_bytes()),
            )?;
            let meta = json!({
                "kind": "tole-skill",
                "name": entry.name,
                "description": entry.description,
                "profiles": entry.profiles,
                "status": entry.status.as_str(),
                "content_hash": entry.content_hash,
            });
            self.run(
                &["doc", "update", &slug, "--metadata", &meta.to_string()],
                None,
            )?;
            Ok(())
        }

        fn retire(&self, name: &str) -> Result<(), String> {
            let slug = slug_for(name)?;
            let doc = self.get(&slug)?;
            let mut meta = match &doc["metadata"] {
                Value::Object(m) => Value::Object(m.clone()),
                _ => json!({ "name": name }),
            };
            meta["status"] = json!(SkillStatus::Retired.as_str());
            self.run(
                &["doc", "update", &slug, "--metadata", &meta.to_string()],
                None,
            )?;
            Ok(())
        }
    }

    #[cfg(all(test, unix))]
    mod tests {
        use super::*;

        /// A fake uteke: logs argv (one line per call) and stdin, answers
        /// `doc search` with `search` and `doc get <slug>` from `docs`.
        fn fake(
            tag: &str,
            search: &str,
            docs: &[(&str, Value)],
        ) -> (UtekeDocIndex, crate::test_dir::TestDir) {
            let dir = crate::test_dir::TestDir::new(&format!("tole-skidx-{tag}"));
            for (slug, doc) in docs {
                std::fs::write(dir.join(format!("{slug}.json")), doc.to_string()).unwrap();
            }
            std::fs::write(dir.join("search.json"), search).unwrap();
            let d = dir.display();
            let script = format!(
                "#!/bin/sh\necho \"$*\" >> {d}/argv.log\n\
                 case \"$1 $2\" in\n\
                 'doc search') cat {d}/search.json ;;\n\
                 'doc get') cat {d}/$3.json 2>/dev/null || {{ echo missing >&2; exit 1; }} ;;\n\
                 'doc create') cat > {d}/stdin.txt ;;\n\
                 esac\n"
            );
            let bin = dir.join("uteke");
            crate::test_dir::write_executable(&bin, script);
            (UtekeDocIndex::new(&bin), dir)
        }

        fn doc(name: &str, status: &str, profiles: &[&str]) -> Value {
            json!({ "slug": format!("skill-{name}"), "tags": ["skill"],
                    "metadata": { "name": name, "status": status, "profiles": profiles } })
        }

        fn hit(slug: &str, score: f64) -> Value {
            json!({ "document": { "slug": slug }, "score": score, "mode": "semantic" })
        }

        #[test]
        fn search_filters_by_tag_status_scope_and_floor() {
            let search = json!([
                hit("skill-deploy", 0.9),
                hit("release-notes", 0.85), // not a skill document
                hit("skill-retired-one", 0.8),
                hit("skill-coder-only", 0.7),
                hit("skill-untagged", 0.65),
                hit("skill-weak", 0.2),   // below the floor
                hit("skill-deploy", 0.6), // duplicate chunk hit
            ])
            .to_string();
            let mut untagged = doc("untagged", "approved", &[]);
            untagged["tags"] = json!(["notes"]);
            let (idx, dir) = fake(
                "filter",
                &search,
                &[
                    ("skill-deploy", doc("deploy", "approved", &[])),
                    ("skill-retired-one", doc("retired-one", "retired", &[])),
                    (
                        "skill-coder-only",
                        doc("coder-only", "approved", &["coder"]),
                    ),
                    ("skill-untagged", untagged),
                    ("skill-weak", doc("weak", "approved", &[])),
                ],
            );
            let f = SkillFilter {
                profile: None,
                floor: 0.5,
            };
            let names: Vec<String> = idx
                .search("ship it", &f, 5)
                .unwrap()
                .into_iter()
                .map(|c| c.name)
                .collect();
            assert_eq!(names, ["deploy"]);
            let f = SkillFilter {
                profile: Some("coder".into()),
                floor: 0.5,
            };
            let names: Vec<String> = idx
                .search("ship it", &f, 5)
                .unwrap()
                .into_iter()
                .map(|c| c.name)
                .collect();
            assert_eq!(names, ["deploy", "coder-only"]);
            // k bounds the result.
            assert_eq!(idx.search("ship it", &f, 1).unwrap().len(), 1);
            // Semantic mode, so the score is a cosine the floor can use.
            let log = std::fs::read_to_string(dir.join("argv.log")).unwrap();
            assert!(log.contains("doc search ship it --mode semantic"), "{log}");
            assert!(
                !log.contains("doc get skill-weak"),
                "below-floor hits are not fetched"
            );
            let _ = std::fs::remove_dir_all(&dir);
        }

        #[test]
        fn malformed_search_output_is_an_error_not_an_empty_result() {
            let (idx, dir) = fake("bad", r#"{"results":[]}"#, &[]);
            assert!(idx
                .search("q", &SkillFilter::default(), 5)
                .unwrap_err()
                .contains("not a JSON array"));
            let (idx, dir2) = fake("noslug", r#"[{"score":0.9}]"#, &[]);
            assert!(idx
                .search("q", &SkillFilter::default(), 5)
                .unwrap_err()
                .contains("hit #1"));
            assert!(idx.search("--json", &SkillFilter::default(), 5).is_err());
            let _ = std::fs::remove_dir_all(&dir);
            let _ = std::fs::remove_dir_all(&dir2);
        }

        #[test]
        fn upsert_indexes_name_and_description_only_and_retire_keeps_metadata() {
            let (idx, dir) = fake(
                "upsert",
                "[]",
                &[("skill-deploy", doc("deploy", "approved", &["coder"]))],
            );
            let entry = SkillEntry {
                name: "deploy".into(),
                description: "Ship a release".into(),
                profiles: vec![],
                status: SkillStatus::Approved,
                content_hash: content_hash(b"raw"),
            };
            idx.upsert(&entry).unwrap();
            assert_eq!(
                std::fs::read_to_string(dir.join("stdin.txt")).unwrap(),
                "# deploy\n\nShip a release\n"
            );
            idx.retire("deploy").unwrap();
            let log = std::fs::read_to_string(dir.join("argv.log")).unwrap();
            let lines: Vec<&str> = log.lines().collect();
            assert_eq!(lines[0], "doc create skill-deploy --file - --tags skill");
            assert!(
                lines[1].starts_with("doc update skill-deploy --metadata {"),
                "{}",
                lines[1]
            );
            assert!(
                lines[1].contains("\"content_hash\":\"sha256:"),
                "{}",
                lines[1]
            );
            assert!(lines[1].contains("\"status\":\"approved\""), "{}", lines[1]);
            // Retire rewrites the stored metadata with only the status changed.
            assert!(lines[3].contains("\"status\":\"retired\""), "{}", lines[3]);
            assert!(
                lines[3].contains("\"profiles\":[\"coder\"]"),
                "{}",
                lines[3]
            );
            // Names that cannot form a slug are refused before any call.
            let bad = SkillEntry {
                name: "Bad_Name".into(),
                ..entry
            };
            assert!(idx.upsert(&bad).is_err());
            assert!(idx.retire("-x").is_err());
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}

/// Adapter 2: the uteke 0.24 skill surfaces (`uteke skill search|list|
/// show`, #973). The index only finds NAMES; a hit carries no text that
/// reaches a prompt, and `show` is reduced to name + content hash on
/// purpose. Indexing (`upsert`, `retire`) is unchanged and delegates to
/// [`UtekeDocIndex`].
///
/// Output shape (read from the uteke v0.24.1 source, `skill_library.rs` and
/// `commands/skill.rs`; `--json` is a global flag): `skill search` and
/// `skill list` print a bare array. A search hit is `{slug, name?,
/// description?, status, hash, valid, score, file, snippet}`; a list entry
/// is `{slug, name?, description?, status, hash, valid, source_path, owner,
/// updated_at}`; `skill show` is `{slug, current: {hash, ..}, approved_hash,
/// ..}`. The skill's identity is its `slug` (the directory name), NOT the
/// optional frontmatter `name`. uteke's `status` (draft / approved / stale)
/// is uteke's own approval of a package, not tole's (#591): it is not used
/// to filter, the local gate stays the authority. Only `valid: false` hits
/// are skipped.
#[cfg(feature = "shell-tools")]
mod uteke_skill {
    use super::*;
    use std::time::Duration;

    /// uteke refuses a query over this many chars.
    const UTEKE_MAX_QUERY_CHARS: usize = 2000;
    const _: () = assert!(MAX_QUERY_CHARS <= UTEKE_MAX_QUERY_CHARS);
    /// `skill search` limit clamp.
    const SEARCH_LIMIT: (usize, usize) = (1, 50);
    /// `skill list` limit clamp.
    const LIST_LIMIT: (usize, usize) = (1, 200);
    /// Retries after a 429 / busy answer (uteke allows two concurrent
    /// remote embeds).
    const BUSY_RETRIES: u32 = 3;

    /// What `uteke skill show` is reduced to: never the body.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct SkillMeta {
        pub name: String,
        /// The hash uteke recorded at index time, as stated data.
        pub content_hash: Option<String>,
    }

    #[derive(Debug, Clone)]
    pub struct UtekeSkillIndex {
        docs: UtekeDocIndex,
        backoff: Duration,
    }

    impl Default for UtekeSkillIndex {
        fn default() -> Self {
            Self::new("uteke")
        }
    }

    fn name_ok(name: &str) -> bool {
        !name.is_empty()
            && name.len() <= 128
            && !name.starts_with('-')
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    }

    fn is_busy(err: &str) -> bool {
        let e = err.to_lowercase();
        e.contains("429") || e.contains("busy")
    }

    /// The skill's identity: the `slug` (v0.24.1 prints the directory name
    /// there); `name` is only the optional frontmatter display name, used
    /// when a hit carries no slug.
    fn hit_name(h: &Value) -> Option<&str> {
        h["slug"].as_str().or_else(|| h["name"].as_str())
    }

    fn hits_of(v: &Value) -> Option<&Vec<Value>> {
        v.as_array().or_else(|| {
            ["results", "hits", "skills"]
                .iter()
                .find_map(|k| v[*k].as_array())
        })
    }

    /// A hit is skipped only when uteke says the package is invalid
    /// (`valid: false`), or when it states a profile scope this run is not
    /// in. uteke's own `status` is NOT used (see the module note); a hit
    /// that states neither is admitted, because the local gate (#591)
    /// decides what may load.
    fn admits(h: &Value, filter: &SkillFilter) -> bool {
        if h["valid"].as_bool() == Some(false) {
            return false;
        }
        let scoped: Vec<&str> = h["profiles"]
            .as_array()
            .or_else(|| h["metadata"]["profiles"].as_array())
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        scoped.is_empty()
            || filter
                .profile
                .as_deref()
                .is_some_and(|p| scoped.contains(&p))
    }

    impl UtekeSkillIndex {
        pub fn new(bin: impl Into<PathBuf>) -> Self {
            Self {
                docs: UtekeDocIndex::new(bin),
                backoff: Duration::from_millis(250),
            }
        }

        #[cfg(test)]
        fn with_backoff(mut self, d: Duration) -> Self {
            self.backoff = d;
            self
        }

        /// Does this uteke have the `skill` subcommand (>= 0.24)? A pre-0.24
        /// uteke rejects the unknown subcommand.
        pub fn supported(&self) -> bool {
            self.docs.run(&["skill", "--help"], None).is_ok()
        }

        /// Run a `skill` call, backing off on a 429 / busy answer.
        fn run(&self, args: &[&str]) -> Result<Value, String> {
            let mut attempt = 0;
            loop {
                match self.docs.run(args, None) {
                    Ok(out) => {
                        return serde_json::from_str(&out).map_err(|e| {
                            format!("uteke {}: invalid JSON: {e}", args[..2].join(" "))
                        })
                    }
                    Err(e) if is_busy(&e) && attempt < BUSY_RETRIES => {
                        std::thread::sleep(self.backoff * 2u32.pow(attempt));
                        attempt += 1;
                    }
                    Err(e) => return Err(e),
                }
            }
        }

        /// Names in the library, at most `limit` (clamped to 1..=200).
        pub fn list(&self, limit: usize) -> Result<Vec<String>, String> {
            let n = limit.clamp(LIST_LIMIT.0, LIST_LIMIT.1).to_string();
            let v = self.run(&["skill", "list", "--limit", &n, "--json"])?;
            let hits = hits_of(&v).ok_or("uteke skill list: unexpected JSON shape")?;
            Ok(hits
                .iter()
                .filter_map(|h| h.as_str().or_else(|| hit_name(h)))
                .filter(|n| name_ok(n))
                .map(str::to_string)
                .collect())
        }

        /// One skill's name and recorded hash. The body, if uteke answers
        /// with one, is dropped here: bodies come from approved local bytes.
        pub fn show(&self, name: &str) -> Result<SkillMeta, String> {
            if !name_ok(name) {
                return Err(format!(
                    "skill name {name:?} is not a valid uteke skill name"
                ));
            }
            let v = self.run(&["skill", "show", name, "--json"])?;
            // The package hash uteke recorded: the approved one when there
            // is one, else the current version's.
            let hash = v["approved_hash"]
                .as_str()
                .or_else(|| v["current"]["hash"].as_str())
                .map(str::to_string);
            Ok(SkillMeta {
                name: name.to_string(),
                content_hash: hash,
            })
        }
    }

    impl SkillIndex for UtekeSkillIndex {
        fn label(&self) -> &'static str {
            "uteke"
        }

        fn search(
            &self,
            query: &str,
            filter: &SkillFilter,
            k: usize,
        ) -> Result<Vec<Candidate>, String> {
            let q: String = clean_query(query)?
                .chars()
                .take(UTEKE_MAX_QUERY_CHARS)
                .collect();
            if k == 0 {
                return Ok(Vec::new());
            }
            let fetch = (k * 3).clamp(SEARCH_LIMIT.0, SEARCH_LIMIT.1).to_string();
            let v = self.run(&["skill", "search", &q, "--limit", &fetch, "--json"])?;
            let hits = hits_of(&v).ok_or("uteke skill search: unexpected JSON shape")?;
            let mut out: Vec<Candidate> = Vec::new();
            for (i, h) in hits.iter().enumerate() {
                let name = hit_name(h)
                    .ok_or_else(|| format!("uteke skill search: hit #{} has no name", i + 1))?;
                let score = h["score"]
                    .as_f64()
                    .or_else(|| h["similarity"].as_f64())
                    .ok_or_else(|| format!("uteke skill search: hit #{} has no score", i + 1))?
                    as f32;
                if !name_ok(name)
                    || score < filter.floor
                    || !admits(h, filter)
                    || out.iter().any(|c| c.name == name)
                {
                    continue; // one hit per skill
                }
                out.push(Candidate {
                    name: name.to_string(),
                    score,
                });
                if out.len() >= k {
                    break;
                }
            }
            Ok(out)
        }

        fn upsert(&self, entry: &SkillEntry) -> Result<(), String> {
            self.docs.upsert(entry)
        }

        fn retire(&self, name: &str) -> Result<(), String> {
            self.docs.retire(name)
        }
    }

    #[cfg(all(test, unix))]
    mod tests {
        use super::*;
        use std::time::Duration;

        /// A fake uteke: logs argv, answers `skill --help` with `help_ok`,
        /// `skill search|list|show` from files; `doc *` is a failing
        /// tripwire. `busy` answers the first N search calls with 429.
        fn fake(
            tag: &str,
            help_ok: bool,
            busy: u32,
        ) -> (UtekeSkillIndex, crate::test_dir::TestDir) {
            let dir = crate::test_dir::TestDir::new(&format!("tole-skskill-{tag}"));
            let d = dir.display();
            std::fs::write(dir.join("busy.n"), busy.to_string()).unwrap();
            let script = format!(
                "#!/bin/sh\necho \"$*\" >> {d}/argv.log\n\
                 case \"$1 $2\" in\n\
                 'skill --help') {} ;;\n\
                 'skill search')\n\
                   n=$(cat {d}/busy.n)\n\
                   if [ \"$n\" -gt 0 ]; then echo $((n-1)) > {d}/busy.n; echo 'HTTP 429 busy' >&2; exit 1; fi\n\
                   cat {d}/search.json ;;\n\
                 'skill list') cat {d}/list.json ;;\n\
                 'skill show') cat {d}/show.json ;;\n\
                 'doc search'|'doc get') echo 'doc surface must not be called' >&2; exit 9 ;;\n\
                 esac\n",
                if help_ok { "exit 0" } else { "echo 'unrecognized subcommand' >&2; exit 2" }
            );
            let bin = dir.join("uteke");
            crate::test_dir::write_executable(&bin, script);
            (
                UtekeSkillIndex::new(&bin).with_backoff(Duration::from_millis(1)),
                dir,
            )
        }

        fn log(dir: &crate::test_dir::TestDir) -> String {
            std::fs::read_to_string(dir.join("argv.log")).unwrap_or_default()
        }

        #[test]
        fn search_calls_the_skill_surface_never_doc_and_filters() {
            let (idx, dir) = fake("search", true, 0);
            std::fs::write(
                dir.join("search.json"),
                // The real v0.24.1 shape: a bare array of hits keyed by `slug`.
                json!([
                    {"slug": "deploy", "name": "Deploy it", "description": "d", "status": "draft",
                     "hash": "sha256:aa", "valid": true, "score": 0.9, "file": "SKILL.md", "snippet": "s"},
                    {"slug": "dup-free", "status": "approved", "hash": "sha256:bb", "valid": true,
                     "score": 0.8, "file": "SKILL.md", "snippet": "s"},
                    {"slug": "deploy", "status": "approved", "valid": true, "score": 0.7},
                    {"slug": "invalid-one", "status": "draft", "valid": false, "score": 0.7},
                    {"slug": "stale-one", "status": "stale", "valid": true, "score": 0.65},
                    {"slug": "coder-only", "valid": true, "score": 0.6, "profiles": ["coder"]},
                    {"slug": "weak", "valid": true, "score": 0.1},
                    {"slug": "-flag", "valid": true, "score": 0.9},
                    {"slug": "bad name", "valid": true, "score": 0.9}
                ])
                .to_string(),
            )
            .unwrap();
            let f = SkillFilter {
                profile: None,
                floor: 0.5,
            };
            let names: Vec<String> = idx
                .search("ship it", &f, 5)
                .unwrap()
                .into_iter()
                .map(|c| c.name)
                .collect();
            // The slug is the identity (not the display name), uteke's own
            // draft / approved / stale status does not filter, an invalid
            // package is skipped.
            assert_eq!(names, ["deploy", "dup-free", "stale-one"]);
            let f = SkillFilter {
                profile: Some("coder".into()),
                floor: 0.5,
            };
            assert_eq!(idx.search("ship it", &f, 5).unwrap().len(), 4);
            assert_eq!(idx.search("ship it", &f, 1).unwrap().len(), 1);
            let l = log(&dir);
            assert!(l.contains("skill search ship it --limit 15 --json"), "{l}");
            assert!(!l.contains("doc "), "{l}");
            let _ = std::fs::remove_dir_all(&dir);
        }

        #[test]
        fn bounds_cap_query_and_clamp_limits() {
            let (idx, dir) = fake("bounds", true, 0);
            std::fs::write(dir.join("search.json"), "[]").unwrap();
            std::fs::write(dir.join("list.json"), "[]").unwrap();
            let f = SkillFilter::default();
            idx.search(&"a".repeat(5000), &f, 40).unwrap();
            idx.search("q", &f, 1).unwrap();
            idx.list(0).unwrap();
            idx.list(9999).unwrap();
            let l = log(&dir);
            // MAX_QUERY_CHARS (1000) applies first, below uteke's 2000.
            let q = l.lines().next().unwrap();
            assert_eq!(
                q,
                format!(
                    "skill search {} --limit 50 --json",
                    "a".repeat(MAX_QUERY_CHARS)
                )
            );
            assert!(
                q.ends_with("--limit 50 --json"),
                "search limit clamps to 50: {q}"
            );
            assert!(l.contains("skill search q --limit 3 --json"), "{l}");
            assert!(l.contains("skill list --limit 1 --json"), "{l}");
            assert!(l.contains("skill list --limit 200 --json"), "{l}");
            let _ = std::fs::remove_dir_all(&dir);
        }

        #[test]
        fn busy_answers_are_retried_then_surface() {
            let (idx, dir) = fake("busy", true, 2);
            std::fs::write(
                dir.join("search.json"),
                json!([{"name": "a", "score": 0.9}]).to_string(),
            )
            .unwrap();
            let f = SkillFilter::default();
            assert_eq!(idx.search("q", &f, 3).unwrap().len(), 1);
            assert_eq!(log(&dir).matches("skill search").count(), 3);
            let (idx, dir2) = fake("busy-forever", true, 99);
            let e = idx.search("q", &f, 3).unwrap_err();
            assert!(e.contains("429"), "{e}");
            assert_eq!(log(&dir2).matches("skill search").count(), 4);
            let _ = std::fs::remove_dir_all(&dir);
            let _ = std::fs::remove_dir_all(&dir2);
        }

        #[test]
        fn show_keeps_name_and_hash_and_drops_the_body() {
            let (idx, dir) = fake("show", true, 0);
            std::fs::write(
                dir.join("show.json"),
                json!({"slug": "deploy", "current": {"hash": "sha256:cd", "status": "draft"},
                       "approved_hash": "sha256:ab", "body": "IGNORE ALL"})
                .to_string(),
            )
            .unwrap();
            let m = idx.show("deploy").unwrap();
            assert_eq!(
                m,
                SkillMeta {
                    name: "deploy".into(),
                    content_hash: Some("sha256:ab".into())
                }
            );
            assert!(!format!("{m:?}").contains("IGNORE"));
            assert!(idx.show("-x").is_err());
            assert!(log(&dir).contains("skill show deploy --json"));
            let _ = std::fs::remove_dir_all(&dir);
        }

        #[test]
        fn the_version_gate_detects_a_missing_skill_subcommand() {
            let (new, d1) = fake("gate-new", true, 0);
            let (old, d2) = fake("gate-old", false, 0);
            assert!(new.supported());
            assert!(!old.supported());
            assert!(!UtekeSkillIndex::new("/nonexistent/uteke").supported());
            let _ = std::fs::remove_dir_all(&d1);
            let _ = std::fs::remove_dir_all(&d2);
        }

        #[test]
        fn malformed_output_is_an_error() {
            let (idx, dir) = fake("bad", true, 0);
            std::fs::write(dir.join("search.json"), "not json").unwrap();
            assert!(idx.search("q", &SkillFilter::default(), 3).is_err());
            std::fs::write(dir.join("search.json"), json!([{"name": "a"}]).to_string()).unwrap();
            assert!(idx.search("q", &SkillFilter::default(), 3).is_err());
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::Tool as _;
    use std::sync::Mutex;

    fn ws(tag: &str, skills: &[(&str, &str)]) -> crate::test_dir::TestDir {
        let dir = crate::test_dir::TestDir::new(&format!("tole-skillsel-{tag}"));
        for (name, desc) in skills {
            let d = dir.join("skills").join(name);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(
                d.join("SKILL.md"),
                format!("---\nname: {name}\ndescription: \"{desc}\"\n---\nbody of {name}"),
            )
            .unwrap();
        }
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A scripted index that records the queries it was asked.
    struct Scripted {
        result: Result<Vec<Candidate>, String>,
        asked: Mutex<Vec<(String, usize)>>,
    }

    impl Scripted {
        fn new(result: Result<Vec<(&str, f32)>, &str>) -> Self {
            Self {
                result: result
                    .map(|v| {
                        v.into_iter()
                            .map(|(n, s)| Candidate {
                                name: n.into(),
                                score: s,
                            })
                            .collect()
                    })
                    .map_err(str::to_string),
                asked: Mutex::new(Vec::new()),
            }
        }
    }

    impl SkillIndex for Scripted {
        fn label(&self) -> &'static str {
            "scripted"
        }
        fn search(&self, q: &str, _: &SkillFilter, k: usize) -> Result<Vec<Candidate>, String> {
            self.asked.lock().unwrap().push((q.to_string(), k));
            self.result.clone()
        }
        fn upsert(&self, _: &SkillEntry) -> Result<(), String> {
            Ok(())
        }
        fn retire(&self, _: &str) -> Result<(), String> {
            Ok(())
        }
    }

    fn names(skills: &[Skill]) -> Vec<&str> {
        skills.iter().map(|s| s.name.as_str()).collect()
    }

    #[test]
    fn select_is_pinned_core_plus_top_k_from_local_files() {
        let dir = ws(
            "sel",
            &[("a", "A"), ("b", "B"), ("c", "C"), ("d", "D"), ("e", "E")],
        );
        let idx = Scripted::new(Ok(vec![
            ("b", 0.9),     // pinned already: not counted twice
            ("ghost", 0.8), // no local skill: dropped
            ("c", 0.7),
            ("d", 0.6),
            ("e", 0.5),
        ]));
        let q = RecallQuery::new("fix the build", Some("coder")).unwrap();
        let sel = select(
            &idx,
            Some(&dir),
            &["a".into(), "b".into(), "missing".into()],
            Some(&q),
            &SkillFilter::default(),
            2,
            &Gate::Open,
        );
        assert_eq!(names(&sel.pinned), ["a", "b"]);
        let recalled: Vec<&str> = sel.recalled.iter().map(|(s, _)| s.name.as_str()).collect();
        assert_eq!(recalled, ["c", "d"], "top K after pinned and missing names");
        assert_eq!(sel.recalled[0].1, 0.7);
        assert!(
            sel.notes.iter().any(|n| n.contains("missing")),
            "{:?}",
            sel.notes
        );
        // The index is asked once, with the brief + role, for K + pinned.
        let asked = idx.asked.lock().unwrap().clone();
        assert_eq!(asked, [("coder: fix the build".to_string(), 4)]);
        let block = selection_block(&sel);
        assert!(
            block.contains("- a — A\n- b — B\n- c — C\n- d — D\n"),
            "{block}"
        );
        assert!(!block.contains("- e"), "{block}");
        assert!(block.contains("skill_search"), "{block}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn select_bounds_pins_and_k_and_degrades_on_index_errors() {
        let dir = ws(
            "bounds",
            &[
                ("a", "A"),
                ("b", "B"),
                ("c", "C"),
                ("d", "D"),
                ("e", "E"),
                ("f", "F"),
            ],
        );
        let pins: Vec<String> = ["a", "b", "c", "d", "e", "f"].map(String::from).to_vec();
        let idx = Scripted::new(Err("uteke down"));
        let q = RecallQuery::new("task", None).unwrap();
        let sel = select(
            &idx,
            Some(&dir),
            &pins,
            Some(&q),
            &SkillFilter::default(),
            50,
            &Gate::Open,
        );
        assert_eq!(sel.pinned.len(), MAX_PINNED);
        assert!(sel.recalled.is_empty());
        assert!(
            sel.notes.iter().any(|n| n.contains("only the first 5")),
            "{:?}",
            sel.notes
        );
        assert!(
            sel.notes.iter().any(|n| n.contains("uteke down")),
            "{:?}",
            sel.notes
        );
        // K is capped at MAX_RECALL_K when asking the index.
        assert_eq!(idx.asked.lock().unwrap()[0].1, MAX_RECALL_K + MAX_PINNED);
        // No brief, or K = 0: the index is not asked at all.
        let idx = Scripted::new(Ok(vec![("a", 0.9)]));
        let sel = select(
            &idx,
            Some(&dir),
            &[],
            None,
            &SkillFilter::default(),
            5,
            &Gate::Open,
        );
        assert!(sel.recalled.is_empty());
        let sel = select(
            &idx,
            Some(&dir),
            &[],
            Some(&q),
            &SkillFilter::default(),
            0,
            &Gate::Open,
        );
        assert!(sel.recalled.is_empty());
        assert!(idx.asked.lock().unwrap().is_empty());
        // Nothing selected: the block still points at skill_search.
        assert!(selection_block(&sel).contains("No skill was preselected"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn recall_query_is_brief_and_role_only_and_never_flag_shaped() {
        assert_eq!(RecallQuery::new("  ", Some("coder")), None);
        assert_eq!(
            RecallQuery::new("do x", Some(" ")).unwrap().as_str(),
            "do x"
        );
        assert_eq!(
            RecallQuery::new("do x", Some("coder")).unwrap().as_str(),
            "coder: do x"
        );
        assert_eq!(
            RecallQuery::new("--rm all", None).unwrap().as_str(),
            "task --rm all"
        );
        let long = "x".repeat(5000);
        assert_eq!(
            RecallQuery::new(&long, None)
                .unwrap()
                .as_str()
                .chars()
                .count(),
            MAX_QUERY_CHARS
        );
        assert!(clean_query("-x").is_err());
        assert!(clean_query(" ").is_err());
    }

    #[test]
    fn local_index_matches_words_of_name_and_description() {
        let dir = ws(
            "local",
            &[
                ("deploy", "Ship a release to staging"),
                ("lint", "Run the linters"),
                ("docs", "Write docs"),
            ],
        );
        let idx = LocalIndex::new(Some(dir.to_path_buf()));
        let got = idx
            .search("release to staging", &SkillFilter::default(), 5)
            .unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].name, "deploy");
        assert_eq!(got[0].score, 1.0);
        assert!(idx
            .search("nothing matches", &SkillFilter::default(), 5)
            .unwrap()
            .is_empty());
        assert!(idx
            .upsert(&SkillEntry::from_local(
                &load_skill(Some(&dir), "lint").unwrap(),
                b"x"
            ))
            .is_ok());
        assert!(idx.retire("lint").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn skill_search_tool_returns_local_descriptions_only() {
        let dir = ws("tool", &[("deploy", "Local description")]);
        let idx: Arc<dyn SkillIndex> =
            Arc::new(Scripted::new(Ok(vec![("deploy", 0.9), ("ghost", 0.8)])));
        let tool = SkillSearchTool::new(
            idx,
            Some(dir.to_path_buf()),
            SkillFilter::default(),
            Gate::Open,
        );
        assert_eq!(tool.risk(), crate::tool::Risk::ReadOnly);
        let out = tool.execute(json!({"query": "ship", "limit": 99})).unwrap();
        assert_eq!(out["index"], json!("scripted"));
        assert_eq!(
            out["skills"],
            json!([{"name": "deploy", "description": "Local description", "score": 0.9f32}])
        );
        assert!(tool.execute(json!({"query": "-x"})).is_err());
        assert!(tool.execute(json!({})).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// #591: under an approval gate, a pinned, recalled or searched skill
    /// that is not approved is named as unavailable (no description, no
    /// body) and does not use up the K.
    #[test]
    fn the_gate_names_unapproved_skills_as_unavailable() {
        use crate::skill_approval::{read_package, Approvals};
        let dir = ws(
            "gate",
            &[("a", "A"), ("b", "INJECTED"), ("c", "C"), ("d", "D")],
        );
        let mut ok = Approvals::default();
        for n in ["a", "c", "d"] {
            ok.approve(&read_package(Some(&dir), n).unwrap(), 1);
        }
        let gate = Gate::Approved(Arc::new(ok));
        let idx = Scripted::new(Ok(vec![("b", 0.9), ("c", 0.8), ("d", 0.7)]));
        let q = RecallQuery::new("task", None).unwrap();
        let sel = select(
            &idx,
            Some(&dir),
            &["a".into(), "b".into()],
            Some(&q),
            &SkillFilter::default(),
            2,
            &gate,
        );
        assert_eq!(names(&sel.pinned), ["a"]);
        let recalled: Vec<&str> = sel.recalled.iter().map(|(s, _)| s.name.as_str()).collect();
        assert_eq!(
            recalled,
            ["c", "d"],
            "a refused skill does not use up the K"
        );
        assert_eq!(sel.unavailable, ["b"]);
        let block = selection_block(&sel);
        assert!(block.contains("cannot be loaded: b\n"), "{block}");
        assert!(!block.contains("INJECTED"), "{block}");
        // Recalled (not pinned) and refused: also named.
        let idx2 = Scripted::new(Ok(vec![("b", 0.9)]));
        let sel = select(
            &idx2,
            Some(&dir),
            &[],
            Some(&q),
            &SkillFilter::default(),
            2,
            &gate,
        );
        assert_eq!(sel.unavailable, ["b"]);
        assert!(selection_block(&sel).contains("No skill was preselected"));
        assert!(selection_block(&sel).contains("cannot be loaded: b"));
        // skill_search: the refused hit has no description.
        let idx3: Arc<dyn SkillIndex> = Arc::new(Scripted::new(Ok(vec![("b", 0.9), ("c", 0.8)])));
        let tool =
            SkillSearchTool::new(idx3, Some(dir.to_path_buf()), SkillFilter::default(), gate);
        let out = tool.execute(json!({"query": "x"})).unwrap();
        assert_eq!(out["skills"][0]["name"], json!("b"));
        assert_eq!(out["skills"][0]["available"], json!(false));
        assert!(out["skills"][0].get("description").is_none());
        assert_eq!(out["skills"][1]["description"], json!("C"));
        assert!(!out.to_string().contains("INJECTED"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn content_hash_is_sha256_hex() {
        assert_eq!(
            content_hash(b"abc"),
            "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
