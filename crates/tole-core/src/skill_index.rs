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
//!   fallback without uteke) and `UtekeDocIndex` (`uteke doc`, tag `skill`,
//!   recall by meaning; `shell-tools` builds only).
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

/// Adapter 1: `uteke doc` (tag `skill`, slug `skill-<name>`, metadata
/// name / description / profiles / status / content hash). Shells out to
/// the uteke CLI like the other uteke integrations.
#[cfg(feature = "shell-tools")]
mod uteke_doc {
    use super::*;
    use crate::subprocess::{
        run_with_timeout, run_with_timeout_stdin, scrub_env_for_child, SUBPROCESS_TIMEOUT,
    };
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

        fn run(&self, args: &[&str], stdin: Option<&[u8]>) -> Result<String, String> {
            let mut cmd = Command::new(&self.bin);
            scrub_env_for_child(&mut cmd);
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
        use std::os::unix::fs::PermissionsExt;

        /// A fake uteke: logs argv (one line per call) and stdin, answers
        /// `doc search` with `search` and `doc get <slug>` from `docs`.
        fn fake(tag: &str, search: &str, docs: &[(&str, Value)]) -> (UtekeDocIndex, PathBuf) {
            let dir = std::env::temp_dir().join(format!("tole-skidx-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
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
            std::fs::write(&bin, script).unwrap();
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::Tool as _;
    use std::sync::Mutex;

    fn ws(tag: &str, skills: &[(&str, &str)]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tole-skillsel-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
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
        let idx = LocalIndex::new(Some(dir.clone()));
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
        let tool = SkillSearchTool::new(idx, Some(dir.clone()), SkillFilter::default(), Gate::Open);
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
        let tool = SkillSearchTool::new(idx3, Some(dir.clone()), SkillFilter::default(), gate);
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
