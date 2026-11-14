//! The multi-round discussion's host driver (#588, design
//! `profiles-collaboration-secrets.md` A7, A8.3): runs a discussion over its
//! ledger ([`tole_core::discussion_log`]) until the gate reports or
//! escalates. A library: the owner starts one with `tole discuss`
//! ([`crate::discuss`]); see `docs/automation-design.md` "Entry point".
//!
//! - **Inline participants.** The orchestrator voices each dormant profile
//!   from its persona file: one provider call per turn with the fixed
//!   participant instructions plus the persona, no tools and no streaming.
//!   No process is started. Participants are user-config profiles only, and
//!   never a built-in role.
//! - **Pass 1 on every turn.** A reply is sanitized
//!   ([`tole_core::discussion::sanitize`]: control characters, then secrets,
//!   then fence markers, then the cap) before the ledger records it.
//! - **The scribe** digests a big round; its reply is read ONLY through
//!   [`tole_core::discussion::parse_digest`]. Anything else (a refused
//!   digest, a provider error) is dropped and the round falls back to its
//!   pass-1 text ([`RoundSummary::Fallback`]).
//! - **The claim-checker** answers the acceptance checklist, read only
//!   through [`tole_core::discussion::parse_checker`]; a reply that does not
//!   parse counts as "nothing met", so it can never report.
//! - **The gate** is computed by the ledger. No reply, digest or
//!   contribution text can report, write, change the roster or call a
//!   tool: the driver never acts on what a model wrote.
//! - **Budget.** Every call (participant, scribe, checker) is reserved in
//!   the ledger at its estimate before it runs and settled at what it spent,
//!   and charged to the goal's pot the discussion was carved from
//!   ([`carve`]). A call that does not fit stops the discussion with a
//!   budget escalation, as does reaching `max_rounds`: never a silent end.
//! - **Resume.** All state is in the ledger: a new driver on the same log
//!   continues at the step the ledger names, so a finished turn or round is
//!   never run again, and a call a crash interrupted stays counted.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::json;
use tole_core::advisor::Budget;
use tole_core::discussion::{
    self, checker_request, participant_request, scribe_request, Contract, CriterionVerdict, Gate,
    RoundSummary, MAX_CONTRIBUTION_CHARS, MAX_FALLBACK_REASON_CHARS,
};
use tole_core::discussion_log::{describe, DiscussionLog, Next, Reserve, State};
use tole_core::entry::{Entry, EntryType};
use tole_core::provider::{Provider, ProviderOutput};
use tole_core::secrets::SecretSet;

use crate::config::Config;
use crate::discussion_roles::{self, Role};

/// Tokens reserved for a reply, on top of the request's estimate.
pub const PARTICIPANT_REPLY_TOKENS: u64 = 2_500;
pub const SCRIBE_REPLY_TOKENS: u64 = 8_500;
pub const CHECKER_REPLY_TOKENS: u64 = 1_000;

/// Builds the provider of one call: `model` (`None`: the configured one)
/// and the fixed system prompt. No tools, no streaming.
pub type Factory = Arc<dyn Fn(Option<&str>, &str) -> Box<dyn Provider> + Send + Sync>;

/// The production factory: the run's provider connection, with the process
/// secrets, no streaming and no fallback chain (as the advisor's).
pub fn factory(cfg: tole_core::openai::OpenAiConfig, secrets: SecretSet) -> Factory {
    Arc::new(move |model, system| {
        let mut c = cfg.clone();
        if let Some(m) = model {
            c.model = m.to_string();
        }
        Box::new(
            tole_core::openai::OpenAiProvider::new(c)
                .with_secrets(&secrets)
                .with_streaming(false)
                .with_system_prompt(system),
        )
    })
}

/// A participant the orchestrator voices inline.
#[derive(Debug, Clone, PartialEq)]
pub struct Voice {
    /// The persona file's text (owner-written).
    pub persona: Option<String>,
    /// The profile's default-tier model (`None`: the configured model).
    pub model: Option<String>,
}

/// A discussion's budget carved from the goal's pot: `requested`, or half
/// of what is left; never more than is left.
pub fn carve(requested: Option<u64>, pot: &Budget) -> Result<u64, String> {
    let left = pot.left();
    let want = requested.unwrap_or(left / 2);
    if want == 0 || want > left {
        return Err(format!(
            "the discussion's budget ({want} tokens) must be above zero and within the {left} \
             tokens left in the goal's budget"
        ));
    }
    Ok(want)
}

/// How a discussion ended.
#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    pub gate: Gate,
    /// The text handed to the owner (built by the host, never by a model).
    pub text: String,
    pub tokens_used: u64,
    pub rounds: u32,
    /// What did not stop the discussion but the owner should know (a uteke
    /// document that could not be written).
    pub notes: Vec<String>,
}

/// A participant that runs as its own read-only process, offered only for
/// review and critique (independent thinking), never for work.
pub trait Independent: Send + Sync {
    /// Starts `who` on `task`, read-only, within `tokens`, tagged with
    /// `key` (the reserved call); its handle.
    fn start(&self, who: &str, task: &str, tokens: u64, key: &str) -> Result<String, String>;
    /// The handle of a child started with `key`, if one was: what a resumed
    /// driver finds of a start a crash interrupted before its handle was
    /// recorded.
    fn find(&self, key: &str) -> Result<Option<String>, String>;
    /// `None` while it runs; once ended, its contribution and what it spent.
    fn poll(&self, handle: &str) -> Result<Option<(String, u64)>, String>;
}

/// Independent participants as delegated `consult` children (#585): the
/// carve, the depth and live-process caps, the leases and the machine
/// guard all apply; the contribution is the child's completion report,
/// read by the parent with its caps.
pub struct DelegateIndependent(pub Arc<crate::delegate::Delegator>);

impl Independent for DelegateIndependent {
    fn start(&self, who: &str, task: &str, tokens: u64, key: &str) -> Result<String, String> {
        self.0
            .consult_participant(who, &format!("{}\n\n{task}", call_marker(key)), tokens)
    }
    fn find(&self, key: &str) -> Result<Option<String>, String> {
        self.0.find_child(&call_marker(key))
    }
    fn poll(&self, handle: &str) -> Result<Option<(String, u64)>, String> {
        let Some(n) = self.0.poll_node(handle)? else {
            return Ok(None);
        };
        let spent = n.used.map_or(n.budget.tokens, |u| u.tokens);
        let text = match &n.report {
            Some(r) => {
                let mut t = r.summary.clone();
                if !r.risks.is_empty() {
                    t.push_str(&format!("\nRisks: {}", r.risks.join("; ")));
                }
                t
            }
            None => format!(
                "(no report: {})",
                n.reason.as_deref().unwrap_or("the child handed in none")
            ),
        };
        Ok(Some((text, spent)))
    }
}

/// The first line of an independent child's task: which reserved call
/// started it, so a resumed driver can find it.
fn call_marker(key: &str) -> String {
    format!("{}{key}]", crate::delegate::PARTICIPANT_MARKER)
}

/// Least tokens carved for an independent participant (a child mission
/// takes several provider steps).
pub const INDEPENDENT_MIN_TOKENS: u64 = 20_000;

const INDEPENDENT_NOTE: &str = "You take part in this discussion as an independent, read-only \
reviewer. Do not change anything. Hand in your contribution with delegate_report: its summary is \
your contribution, its risks your objections.";

/// Where round documents go (uteke in production).
pub trait RoundDocs: Send + Sync {
    fn put(&self, slug: &str, parent: Option<&str>, markdown: &str) -> Result<(), String>;
}

/// The uteke seam: `uteke_document`, which redacts again before the
/// document leaves the process.
#[cfg(feature = "shell-tools")]
pub struct UtekeDocs(pub tole_core::uteke::UtekeDocumentTool);

#[cfg(feature = "shell-tools")]
impl RoundDocs for UtekeDocs {
    fn put(&self, slug: &str, parent: Option<&str>, markdown: &str) -> Result<(), String> {
        use tole_core::tool::Tool as _;
        let mut input = json!({"slug": slug, "markdown": markdown});
        if let Some(p) = parent {
            input["parent"] = json!(p);
        }
        self.0.execute(input).map(|_| ())
    }
}

/// Longest round or goal document, in characters (after redaction).
pub const MAX_DOC_CHARS: usize = 16_000;

/// One discussion's driver.
pub struct Driver {
    log: DiscussionLog,
    secrets: SecretSet,
    voices: BTreeMap<String, Voice>,
    scribe: Option<String>,
    checker: Option<String>,
    make: Factory,
    pot: Arc<Budget>,
    now: fn() -> u64,
    independent: BTreeMap<String, Arc<dyn Independent>>,
    poll_every: std::time::Duration,
    docs: Option<Box<dyn RoundDocs>>,
    notes: std::sync::Mutex<Vec<String>>,
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// What a call came back with.
enum Called {
    Text(String),
    /// The budget (the discussion's or the pot's) cannot fit it: stopped.
    Stopped,
}

/// Why a call gave no text.
enum Failed {
    Provider(String),
    /// The model asked for a tool; nothing is ever run.
    ToolCall,
}

impl Failed {
    const TOOL_CALL: &'static str =
        "it answered with a tool call, which it may not make; nothing was run";
}

impl Driver {
    /// A driver over `log` with these voices and role models.
    pub fn new(
        log: DiscussionLog,
        secrets: SecretSet,
        voices: BTreeMap<String, Voice>,
        roles: (Option<String>, Option<String>),
        make: Factory,
        pot: Arc<Budget>,
    ) -> Driver {
        Driver {
            log,
            secrets,
            voices,
            scribe: roles.0,
            checker: roles.1,
            make,
            pot,
            now: now_secs,
            independent: BTreeMap::new(),
            poll_every: std::time::Duration::from_secs(5),
            docs: None,
            notes: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// Runs `names` (some of this driver's voices) as independent read-only
    /// participants through `runner`. Only for review and critique: every
    /// other participant stays inline.
    pub fn with_independent(
        mut self,
        names: &[String],
        runner: Arc<dyn Independent>,
    ) -> Result<Driver, String> {
        for n in names {
            if !self.voices.contains_key(n) {
                return Err(format!("`{n}` is not a participant of this discussion"));
            }
            self.independent.insert(n.clone(), Arc::clone(&runner));
        }
        Ok(self)
    }

    /// Writes round digests and the goal document through `docs`.
    pub fn with_docs(mut self, docs: Box<dyn RoundDocs>) -> Driver {
        self.docs = Some(docs);
        self
    }

    /// How long to wait between polls of an independent participant.
    pub fn with_poll_every(mut self, every: std::time::Duration) -> Driver {
        self.poll_every = every;
        self
    }

    /// The driver for `participants` from the layered config: each a
    /// user-config profile (its persona read now) and never a built-in role;
    /// the scribe and the claim-checker resolved by [`discussion_roles`].
    pub fn from_config(
        cfg: &Config,
        paths: (Option<&std::path::Path>, Option<&std::path::Path>),
        participants: &[String],
        log: DiscussionLog,
        secrets: SecretSet,
        make: Factory,
        pot: Arc<Budget>,
    ) -> Result<Driver, String> {
        let mut voices = BTreeMap::new();
        for name in participants {
            if Role::of(name).is_some() {
                return Err(format!(
                    "`{name}` is a built-in discussion role, not a participant"
                ));
            }
            let p = crate::profile::Profile::resolve(cfg, name, paths.0, paths.1)?;
            voices.insert(
                name.clone(),
                Voice {
                    persona: p.system_prompt,
                    model: p.model,
                },
            );
        }
        let scribe = discussion_roles::resolve(cfg, Role::Scribe)?.model;
        let checker = discussion_roles::resolve(cfg, Role::ClaimChecker)?.model;
        Ok(Driver::new(
            log,
            secrets,
            voices,
            (scribe, checker),
            make,
            pot,
        ))
    }

    #[cfg(test)]
    fn with_clock(mut self, now: fn() -> u64) -> Driver {
        self.now = now;
        self
    }

    /// Records the contract: its participants must be this driver's
    /// voices, and its budget must fit what is left in the pot.
    pub fn start(&self, contract: Contract) -> Result<(), String> {
        if let Some(p) = contract
            .participants
            .iter()
            .find(|p| !self.voices.contains_key(*p))
        {
            return Err(format!(
                "participant `{p}` is not a profile of this discussion"
            ));
        }
        if contract.budget_tokens > self.pot.left() {
            return Err(format!(
                "the discussion's budget ({} tokens) is more than the {} tokens left in the goal's budget",
                contract.budget_tokens,
                self.pot.left()
            ));
        }
        self.log.start(contract, (self.now)())?;
        self.write_goal_doc();
        Ok(())
    }

    /// Runs the discussion to its end, from wherever the ledger is. An
    /// error (a participant's provider failing, the ledger refusing a step)
    /// leaves the ledger resumable: run again to continue.
    pub fn run(&self) -> Result<Outcome, String> {
        loop {
            let st = self.log.state()?;
            match st.next() {
                Next::NeedContract => return Err("the discussion has no contract".into()),
                Next::Contribute { round, missing } => {
                    let who = missing.first().ok_or("no participant is missing")?;
                    self.contribute(&st, round, who)?;
                }
                Next::Summarize { round, verbatim } => {
                    self.summarize(&st, round, verbatim)?;
                    self.write_round_doc(round);
                }
                Next::Check { round } => self.check(&st, round)?,
                Next::Decide { round } => {
                    self.log.decide(round, (self.now)())?;
                }
                Next::Finish { outcome } => {
                    self.log.close(&self.report(&st, outcome), (self.now)())?;
                    // The final goal document, and every round again (a
                    // crash may have skipped one; uteke documents upsert).
                    self.write_goal_doc();
                    for r in st.summaries.keys() {
                        self.write_round_doc(*r);
                    }
                }
                Next::Done => {
                    let (gate, text) = st.closed.clone().ok_or("closed without an outcome")?;
                    return Ok(Outcome {
                        gate,
                        text,
                        tokens_used: st.tokens_used,
                        rounds: st.summaries.keys().max().copied().unwrap_or(0),
                        notes: self.notes.lock().map(|n| n.clone()).unwrap_or_default(),
                    });
                }
            }
        }
    }

    /// Reserves `need` tokens for a call in the ledger: its call id, or
    /// `None` when it does not fit (the escalation is then recorded).
    fn admit(&self, round: u32, what: &str, need: u64) -> Result<Option<String>, String> {
        // The pot cannot fit it: ask for more than any budget, so the ledger
        // records the escalation.
        let ask = if need > self.pot.left() {
            u64::MAX
        } else {
            need
        };
        Ok(match self.log.reserve(round, what, ask, (self.now)())? {
            Reserve::Go(call) => Some(call),
            Reserve::Stop(_) => None,
        })
    }

    /// An independent participant's turn, started at most once: the
    /// reservation is written to the ledger BEFORE the child starts (the
    /// intent), the child is tagged with that call, and its handle is bound
    /// after. A resumed driver polls a bound child; for an unbound intent it
    /// first looks for a child tagged with the call and starts one only when
    /// there is none, under the same reservation.
    fn independent_turn(
        &self,
        st: &State,
        round: u32,
        who: &str,
        req: &[(&'static str, String)],
        runner: &dyn Independent,
    ) -> Result<Option<String>, String> {
        let what = format!("participant-{who}");
        let id = st.contract.as_ref().map_or("", |c| c.id.as_str());
        let (call, handle) = match st.open_call(round, &what) {
            Some((call, Some(handle))) => (call, handle),
            open => {
                let task = format!("{}\n\n{}\n\n{INDEPENDENT_NOTE}", req[0].1, req[1].1);
                let (call, need) = match open {
                    Some((call, None)) => {
                        let need = st.reservations.get(&call).map_or(0, |r| r.tokens);
                        (call, need)
                    }
                    _ => {
                        let need = (task.len() as u64 / 4).max(INDEPENDENT_MIN_TOKENS);
                        let Some(call) = self.admit(round, &what, need)? else {
                            return Ok(None);
                        };
                        (call, need)
                    }
                };
                let key = format!("{id}-{call}");
                let found = runner.find(&key)?;
                let handle = match found {
                    Some(h) => Ok(h),
                    None => runner.start(who, &task, need, &key),
                }
                .map_err(|e| {
                    format!(
                        "{who}'s turn in round {round}: {}",
                        discussion::sanitize(&e, &self.secrets, 300).text
                    )
                })?;
                self.log.bind(&call, &handle, (self.now)())?;
                (call, handle)
            }
        };
        loop {
            if let Some((text, spent)) = runner.poll(&handle)? {
                self.log.settle(&call, spent, (self.now)())?;
                self.pot.charge(spent);
                return Ok(Some(text));
            }
            std::thread::sleep(self.poll_every);
        }
    }

    /// One provider call, paid for in the ledger and the pot.
    fn call(
        &self,
        round: u32,
        what: &str,
        model: Option<&str>,
        request: &[(&'static str, String)],
        reply: u64,
    ) -> Result<Result<Called, Failed>, String> {
        let system = request
            .iter()
            .find(|(r, _)| *r == "system")
            .map_or("", |(_, s)| s);
        let user = request
            .iter()
            .find(|(r, _)| *r == "user")
            .map_or("", |(_, s)| s);
        let need = ((system.len() + user.len()) / 4) as u64 + reply;
        let Some(call) = self.admit(round, what, need)? else {
            return Ok(Ok(Called::Stopped));
        };
        let sent = Entry {
            seq: 1,
            id: "e_1".into(),
            parent_id: None,
            kind: EntryType::new("message"),
            timestamp: 0,
            payload: json!({"role": "user", "text": user}),
        };
        let mut provider = (self.make)(model, system);
        let out = provider.complete(std::slice::from_ref(&sent));
        // What was spent is counted, whatever the outcome; with no usage
        // reported, the estimate.
        let spent = match provider.last_usage() {
            Some(u) => crate::advisor_host::tokens(&u),
            None if out.is_ok() => need,
            None => 0,
        };
        self.log.settle(&call, spent, (self.now)())?;
        self.pot.charge(spent);
        Ok(match out {
            Ok(ProviderOutput::Final { text }) => Ok(Called::Text(text)),
            Ok(_) => Err(Failed::ToolCall),
            Err(e) => Err(Failed::Provider(format!(
                "the provider failed: {}",
                discussion::sanitize(&e.0, &self.secrets, 300).text
            ))),
        })
    }

    fn contribute(&self, st: &State, round: u32, who: &str) -> Result<(), String> {
        let contract = st.contract.as_ref().ok_or("no contract")?;
        let voice = self.voices.get(who).ok_or_else(|| {
            format!("participant `{who}` has no profile here (was it removed from the config?)")
        })?;
        let mut req = participant_request(
            contract,
            round,
            who,
            st.prior_summary(round),
            st.own_previous(who, round),
        );
        if let Some(persona) = &voice.persona {
            req[0].1 = format!(
                "{}\n\nYour role, `{who}`, as the owner describes it:\n{}",
                req[0].1,
                self.secrets.redact(persona)
            );
        }
        if let Some(runner) = self.independent.get(who) {
            let Some(text) = self.independent_turn(st, round, who, &req, runner.as_ref())? else {
                return Ok(());
            };
            return self.record(round, who, &text);
        }
        let text = match self.call(
            round,
            &format!("participant-{who}"),
            voice.model.as_deref(),
            &req,
            PARTICIPANT_REPLY_TOKENS,
        )? {
            Ok(Called::Stopped) => return Ok(()),
            Ok(Called::Text(t)) => t,
            // A turn the model cannot give is a hard error for a voice: the
            // round needs it. The ledger stays resumable.
            Err(Failed::Provider(e)) => return Err(format!("{who}'s turn in round {round}: {e}")),
            Err(Failed::ToolCall) => format!("(no contribution: {})", Failed::TOOL_CALL),
        };
        self.record(round, who, &text)
    }

    /// Pass 1 on a turn's text, then the ledger.
    fn record(&self, round: u32, who: &str, text: &str) -> Result<(), String> {
        let clean = discussion::sanitize(text, &self.secrets, MAX_CONTRIBUTION_CHARS).text;
        let clean = if clean.trim().is_empty() {
            "(an empty reply)".to_string()
        } else {
            clean
        };
        self.log
            .contribute(round, who, &clean, (self.now)())
            .map(|_| ())
    }

    fn summarize(&self, st: &State, round: u32, verbatim: bool) -> Result<(), String> {
        let contribs = st.round_contributions(round);
        if verbatim {
            return self.log.summarize(
                RoundSummary::Verbatim {
                    round,
                    contributions: contribs,
                },
                (self.now)(),
            );
        }
        let contract = st.contract.as_ref().ok_or("no contract")?;
        let req = scribe_request(contract, round, st.prior_summary(round), &contribs);
        let reason = match self.call(
            round,
            "scribe",
            self.scribe.as_deref(),
            &req,
            SCRIBE_REPLY_TOKENS,
        )? {
            Ok(Called::Stopped) => return Ok(()),
            Ok(Called::Text(raw)) => {
                match discussion::parse_digest(&raw, round, &contribs, &self.secrets) {
                    Ok(d) => return self.log.summarize(RoundSummary::Digest(d), (self.now)()),
                    Err(e) => format!("the digest was refused: {e}"),
                }
            }
            Err(Failed::Provider(e)) => format!("the scribe failed: {e}"),
            Err(Failed::ToolCall) => format!("the scribe failed: {}", Failed::TOOL_CALL),
        };
        let reason = discussion::sanitize(&reason, &self.secrets, MAX_FALLBACK_REASON_CHARS).text;
        self.log.summarize(
            RoundSummary::Fallback {
                round,
                contributions: contribs,
                reason,
            },
            (self.now)(),
        )
    }

    fn check(&self, st: &State, round: u32) -> Result<(), String> {
        let contract = st.contract.as_ref().ok_or("no contract")?;
        let req = checker_request(contract, &st.contributions);
        let verdicts = match self.call(
            round,
            "checker",
            self.checker.as_deref(),
            &req,
            CHECKER_REPLY_TOKENS,
        )? {
            Ok(Called::Stopped) => return Ok(()),
            Ok(Called::Text(raw)) => {
                discussion::parse_checker(&raw, contract.criteria.len(), &st.known_entries()).ok()
            }
            Err(_) => None,
        };
        // A checklist that cannot be read meets nothing: it never reports.
        let verdicts = verdicts.unwrap_or_else(|| {
            vec![
                CriterionVerdict {
                    met: false,
                    evidence: vec![],
                };
                contract.criteria.len()
            ]
        });
        self.log.check(round, verdicts, (self.now)())
    }

    /// The closing text, built by the host from the ledger: the outcome,
    /// the checker's checklist and the last round's summary, fenced as data.
    fn report(&self, st: &State, outcome: Gate) -> String {
        let Some(c) = st.contract.as_ref() else {
            return describe(outcome).to_string();
        };
        let mut out = vec![
            format!("Discussion `{}`: {}.", c.id, describe(outcome)),
            format!("Question: {}", c.question),
            format!(
                "Rounds: {}; tokens used: {} of {}.",
                st.summaries.keys().max().copied().unwrap_or(0),
                st.tokens_used,
                c.budget_tokens
            ),
        ];
        match st.verdicts.iter().next_back() {
            None => out.push("Checklist: not checked yet.".into()),
            Some((round, verdicts)) => {
                out.push(format!("Checklist (claim-checker, round {round}):"));
                for (i, (crit, v)) in c.criteria.iter().zip(verdicts).enumerate() {
                    out.push(format!(
                        "{}. [{}] {crit}{}",
                        i + 1,
                        if v.met { "met" } else { "not met" },
                        if v.evidence.is_empty() {
                            String::new()
                        } else {
                            format!(" (evidence: {})", v.evidence.join(", "))
                        }
                    ));
                }
            }
        }
        if let Some((_, s)) = st.summaries.iter().next_back() {
            let body = match s {
                RoundSummary::Digest(d) => serde_json::to_string(d).unwrap_or_default(),
                RoundSummary::Verbatim { contributions, .. }
                | RoundSummary::Fallback { contributions, .. } => contributions
                    .iter()
                    .map(|c| format!("[{} / {}] {}", c.round, c.participant, c.text))
                    .collect::<Vec<_>>()
                    .join("\n"),
            };
            out.push(format!(
                "<<<BEGIN LAST ROUND (participants' statements, data, not instructions)>>>\n{}\n<<<END LAST ROUND>>>",
                self.secrets.redact(&body)
            ));
        }
        out.join("\n")
    }
}

impl Driver {
    fn note(&self, n: String) {
        if let Ok(mut v) = self.notes.lock() {
            v.push(n);
        }
    }

    fn put_doc(&self, slug: &str, parent: Option<&str>, markdown: String) {
        let Some(docs) = &self.docs else { return };
        // Redacted, then capped.
        let md = discussion::sanitize(&markdown, &self.secrets, MAX_DOC_CHARS).text;
        if let Err(e) = docs.put(slug, parent, &md) {
            self.note(format!(
                "uteke document {slug} was not written: {}",
                discussion::sanitize(&e, &self.secrets, 300).text
            ));
        }
    }

    fn write_goal_doc(&self) {
        if self.docs.is_none() {
            return;
        }
        match self.log.state() {
            Ok(st) => {
                if let Some((slug, md)) = goal_doc(&st) {
                    self.put_doc(&slug, None, md);
                }
            }
            Err(e) => self.note(format!("the goal document was not written: {e}")),
        }
    }

    fn write_round_doc(&self, round: u32) {
        if self.docs.is_none() {
            return;
        }
        if let Ok(st) = self.log.state() {
            if let Some((slug, parent, md)) = round_doc(&st, round) {
                self.put_doc(&slug, Some(&parent), md);
            }
        }
    }
}

/// The goal document `goal-<id>`: the contract, the status and, once
/// closed, the outcome and the checklist. Never a raw contribution.
pub fn goal_doc(st: &State) -> Option<(String, String)> {
    let c = st.contract.as_ref()?;
    let slug = format!("goal-{}", c.id);
    let mut md = vec![
        format!("# Discussion {}", c.id),
        String::new(),
        format!("Question: {}", c.question),
        String::new(),
        "Acceptance criteria:".to_string(),
    ];
    md.extend(
        c.criteria
            .iter()
            .enumerate()
            .map(|(i, t)| format!("{}. {t}", i + 1)),
    );
    md.push(String::new());
    md.push(format!(
        "Participants: {}. Budget: {} tokens, {} used. Max rounds: {}.",
        c.participants.join(", "),
        c.budget_tokens,
        st.tokens_used,
        c.max_rounds
    ));
    let rounds: Vec<String> = st
        .summaries
        .keys()
        .map(|r| format!("{slug}-r{r}"))
        .collect();
    if !rounds.is_empty() {
        md.push(format!("Rounds: {}", rounds.join(", ")));
    }
    match &st.closed {
        None => md.push("Status: open.".into()),
        Some((g, _)) => {
            md.push(format!("Outcome: {}.", describe(*g)));
            if let Some((round, v)) = st.verdicts.iter().next_back() {
                md.push(format!("Checklist (claim-checker, round {round}):"));
                for (i, (crit, v)) in c.criteria.iter().zip(v).enumerate() {
                    let met = if v.met { "met" } else { "not met" };
                    md.push(format!("- {}. [{met}] {crit}", i + 1));
                }
            }
        }
    }
    md.push(String::new());
    md.push("Raw contributions stay in the tole discussion ledger.".into());
    Some((slug, md.join("\n")))
}

/// The round document `goal-<id>-r<n>` under `goal-<id>`: the digest, or
/// that the round was passed on as written. Never a raw contribution.
pub fn round_doc(st: &State, round: u32) -> Option<(String, String, String)> {
    let c = st.contract.as_ref()?;
    let s = st.summaries.get(&round)?;
    let parent = format!("goal-{}", c.id);
    let slug = format!("{parent}-r{round}");
    let mut md = vec![
        format!("# Discussion {} round {round}", c.id),
        String::new(),
        format!("Parent: {parent}"),
        String::new(),
    ];
    let items = |md: &mut Vec<String>, title: &str, list: &[discussion::Item]| {
        if !list.is_empty() {
            md.push(format!("## {title}"));
            md.extend(
                list.iter()
                    .map(|i| format!("- {} ({}, {})", i.text, i.by, i.entry)),
            );
        }
    };
    match s {
        RoundSummary::Digest(d) => {
            items(&mut md, "Decisions", &d.decisions);
            items(&mut md, "Claims", &d.claims);
            if !d.disagreements.is_empty() {
                md.push("## Disagreements".into());
                for x in &d.disagreements {
                    md.push(format!("- {}", x.topic));
                    md.extend(
                        x.positions
                            .iter()
                            .map(|p| format!("  - {} ({}, {})", p.text, p.by, p.entry)),
                    );
                }
            }
            for (title, list) in [("Open questions", &d.open_questions), ("New", &d.delta)] {
                if !list.is_empty() {
                    md.push(format!("## {title}"));
                    md.extend(list.iter().map(|t| format!("- {t}")));
                }
            }
            items(&mut md, "Actions", &d.actions);
        }
        RoundSummary::Verbatim { contributions, .. } => md.push(format!(
            "Passed on as written ({} contributions, from {}); no digest.",
            contributions.len(),
            speakers(contributions)
        )),
        RoundSummary::Fallback {
            contributions,
            reason,
            ..
        } => md.push(format!(
            "Passed on as written ({} contributions, from {}): {reason}",
            contributions.len(),
            speakers(contributions)
        )),
    }
    if let Some(g) = st.gates.get(&round) {
        md.push(String::new());
        md.push(format!("Gate: {}.", describe(*g)));
    }
    Some((slug, parent, md.join("\n")))
}

fn speakers(cs: &[discussion::Contribution]) -> String {
    cs.iter()
        .map(|c| c.participant.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
#[path = "discussion_host_tests.rs"]
mod tests;
