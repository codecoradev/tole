//! The nested clap subcommand enums of `tole delegation`, `tole approvals`,
//! `tole discuss` and `tole config`.

use clap::Subcommand;

#[derive(Subcommand)]
pub(crate) enum DelegationAction {
    /// Approval requests of this workspace's delegated children.
    Approvals {
        #[command(subcommand)]
        action: ApprovalsAction,
    },
}

#[derive(Subcommand)]
pub(crate) enum ApprovalsAction {
    /// The pending requests (all of them with `--all`).
    List {
        #[arg(long)]
        all: bool,
    },
    /// One request in full: who asked, the chain, what it would do.
    Show { id: String },
    /// Allow ONE call of ONE request: shows it, then asks for the id typed
    /// back. The child runs the call once, if it is still waiting.
    Allow {
        /// The full request id (no prefix, no pattern).
        id: String,
        /// Skip typing the id back (the id above is the confirmation).
        #[arg(long)]
        yes: bool,
        /// Pin the content: 12 or more hex characters of the fingerprint
        /// `show` prints.
        #[arg(long, value_name = "HEX")]
        fingerprint: Option<String>,
    },
    /// Deny a request (no answer is a denial too).
    Deny { id: String },
}

/// `tole config <action>` — an enum so later parts can add variants.
#[derive(Subcommand)]
pub(crate) enum DiscussAction {
    /// Start a discussion and run it to its end (the gate reports or
    /// escalates; the budget and `--max-rounds` always stop it).
    Start {
        /// The question to settle.
        #[arg(long)]
        question: String,
        /// An acceptance criterion the claim-checker verifies. Repeat for
        /// more (1 to 12).
        #[arg(long = "criterion", required = true)]
        criteria: Vec<String>,
        /// A participant: a profile name from your user config. Repeat for
        /// more (any number; 8 is recommended, more only adds cost).
        #[arg(long = "participant", required = true)]
        participants: Vec<String>,
        /// Tokens the whole discussion may spend (a hard cap).
        #[arg(long)]
        budget: u64,
        /// Most rounds before an escalation (default 4, at most 10).
        #[arg(long)]
        max_rounds: Option<u32>,
        /// Discussion id (default: d-<unix seconds>).
        #[arg(long)]
        id: Option<String>,
        /// A background file inside the current folder, passed by reference:
        /// inline participants read a capped, sanitized excerpt (fenced as
        /// data), counted in the cost estimate. Repeat for more (at most 4).
        /// A path outside the folder, a symlink or a FIFO is refused. Only
        /// known secret values and token shapes are redacted (a .env-style
        /// file line by line): anything else in the file, such as a password
        /// in prose, goes as written to every inline participant's provider.
        /// Name only files you would paste into the question. A resume
        /// refuses a file that is gone or changed.
        #[arg(long = "file")]
        files: Vec<String>,
        /// A connected (MCP) tool the participants may use, as
        /// <server>.<tool> (e.g. nginjen.search); repeat for more (at most
        /// 8). The server must be in the `mcp_server` list of your config (or
        /// be the cora preset): the helpers are separate processes that do
        /// not get a `--mcp-server` flag. The tool name must need no change
        /// to be registered (letters, digits, `_`, `-`; the name `mcp_<server>_<tool>`
        /// is printed). With any, every participant
        /// runs as an independent read-only child (it cannot write or run
        /// programs) and the FIRST call of each tool in this discussion is
        /// asked of you in the delegation approvals queue (answer it with
        /// `tole delegation approvals list|allow|deny` from the same folder): allow
        /// covers that tool for this discussion only, never another one. It
        /// needs `[delegation] approvals = "owner"` in your user config.
        #[arg(long = "tool")]
        tools: Vec<String>,
        /// Validate the contract and show what it may cost; call no model
        /// and write nothing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Continue a discussion where its ledger stands (after a crash or a
    /// failed call). A finished one prints its outcome again.
    Resume {
        /// The discussion id.
        id: String,
    },
    /// List the discussions.
    List {
        /// Print JSON instead of text.
        #[arg(long)]
        json: bool,
    },
    /// Show one discussion: rounds, gate decisions, cost; never a raw
    /// contribution.
    Show {
        /// The discussion id.
        id: String,
        /// Print JSON instead of text.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
pub(crate) enum ConfigAction {
    /// Validate `<cwd>/.tole/config.toml` (or --config) and print what was
    /// parsed plus the effective value and source of each applied key.
    /// Exits non-zero on any error.
    Check,
    /// Trust `<cwd>/.tole/config.toml` (or --config): shows the full
    /// content (and a diff vs the previously trusted version), then asks
    /// `[y/N]`. The approval is content-bound and stored outside the repo
    /// (`$CODECORA_HOME/tole/trusted-configs.json`).
    Trust {
        /// Skip the question (the content is still printed). Required
        /// when there is no terminal to ask on.
        #[arg(long)]
        yes: bool,
    },
    /// Forget the trust record for the config (no error if absent).
    Untrust,
}
