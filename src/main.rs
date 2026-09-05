//! Engram CLI: host/operator administration plus the thirteen-word agent surface.

use std::{
    env, fs,
    io::{self, BufReader, BufWriter, Read},
    path::{Path, PathBuf},
    process::ExitCode,
    str::FromStr,
};

use anyhow::{Context, Result, bail};
use clap::{ArgAction, Parser, Subcommand, ValueEnum};
use engram::domain::AssuranceLevel;
use engram::{
    ActorContext, AddInput, AgentVerbs, BuiltinObligationRuleRef, BuiltinObligationTrigger,
    ClaimInput, ControlAssurance, DevelopmentNoopRedactor, DoneInput, ForgetInput, GateInput,
    HandoffAction, HandoffInput, HostControlServer, HostPathPolicy, LocalWorkService, LsInput,
    McpServer, MemoriesInput, NextInput, NoteInput, ObjectHash, ObligationRuleDefinition,
    ObligationRuleSet, ProjectId, RememberInput, SessionId, SqliteStore, StoreError, UpdateAction,
    UpdateInput, VerificationKind, VerificationRequirement, WaiveWorkObligationRequest,
    WorkAttributionDefaults, WorkAvailability, WorkCompleteInput, WorkCompleteResult,
    WorkHandoffInput, WorkItemKind, WorkLifecycle, WorkNextQuery, WorkNextSection,
    WorkObligationId, WorkProposeInput, WorkUpdateInput, looks_like_work_ref, parse_defer_date,
    parse_host_path_policy, probe_host_path_policy, project_database_path, store_error_value,
};
use rmcp::{ServiceExt, transport::stdio};

mod bin_support;

use bin_support::{
    attribution::resolve_shell_work_attribution,
    doctor::doctor,
    graph::run_graph_from_cli,
    store_lifecycle::{backup, initialize, restore},
};

#[derive(Debug, Parser)]
#[command(name = "engram", version, about)]
struct Cli {
    /// Stable project-id file shared by every worktree.
    #[arg(long, default_value = ".engram-project")]
    project_file: PathBuf,
    /// Host-local Engram data directory (or set `ENGRAM_HOME`).
    #[arg(long)]
    home: Option<PathBuf>,
    /// Filesystem identity of the project root. Omit to probe the root's real
    /// filesystem; supply it when probing is impossible or the host knows
    /// better. Unresolved identity refuses path leases instead of guessing.
    #[arg(long, env = "ENGRAM_HOST_PATH_POLICY", value_enum)]
    host_path_policy: Option<HostPathPolicyArg>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum HostPathPolicyArg {
    #[value(name = "case_fold")]
    CaseFold,
    #[value(name = "case_sensitive")]
    CaseSensitive,
}

/// Resolved project-root filesystem identity: host-supplied, probed, or
/// unresolved (with the reason already printed to stderr).
fn resolve_host_path_identity(
    root: &Path,
    supplied: Option<HostPathPolicyArg>,
) -> Option<HostPathPolicy> {
    if let Some(supplied) = supplied {
        return parse_host_path_policy(match supplied {
            HostPathPolicyArg::CaseFold => "case_fold",
            HostPathPolicyArg::CaseSensitive => "case_sensitive",
        });
    }
    match probe_host_path_policy(root) {
        Ok(policy) => Some(policy),
        Err(error) => {
            eprintln!(
                "WARNING: {error}; path leases are refused until --host-path-policy case_fold|case_sensitive (or ENGRAM_HOST_PATH_POLICY) is supplied"
            );
            None
        }
    }
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Create or verify the local Engram database.
    Init {
        /// Minimum host-control assurance required by the bootstrap policy.
        ///
        /// A fresh store defaults to `turn_gated` when this flag is omitted.
        /// Plain `engram init` preserves an existing store's active policy.
        /// Changing this on an existing store requires `control-policy
        /// set-required-assurance`, which records attribution and bumps the
        /// policy epoch.
        #[arg(
            long,
            value_enum,
            requires_all = ["authorized_by", "reason"]
        )]
        required_assurance: Option<ControlAssuranceArg>,
        /// Host/operator actor id attributed to an explicit bootstrap choice.
        #[arg(long, requires = "required_assurance")]
        authorized_by: Option<String>,
        /// Auditable reason for an explicit bootstrap policy choice.
        #[arg(long, requires = "required_assurance")]
        reason: Option<String>,
    },
    /// Verify every immutable object in the local database.
    Doctor {
        /// Emit the machine-readable host diagnostics contract on stdout.
        #[arg(long)]
        json: bool,
        /// Inspect a refused control-policy store without opening or mutating it.
        #[arg(long, conflicts_with = "repair_projections")]
        recover_policy: bool,
        /// Explicitly rebuild indexes, triggers, and full-text projections.
        #[arg(long, conflicts_with = "recover_policy")]
        repair_projections: bool,
    },
    /// Write a verified copy of the store into the host-local backup directory.
    ///
    /// A backup is a complete store, grants and private scratch included; keep
    /// it where the store itself may be kept.
    Backup {
        /// Backup file to write; defaults to `<home>/backups/<project>/engram-<utc>.db`.
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Bring a verified backup back as this project's store.
    Restore {
        /// Backup file written by `engram backup`.
        #[arg(long)]
        from: PathBuf,
        /// Replace an existing store instead of refusing.
        #[arg(long)]
        replace: bool,
    },
    /// Save or load the deterministic project work graph.
    Graph {
        /// Actor identity asserted by the invoking host or operator wrapper.
        #[arg(long, env = "ENGRAM_ACTOR_ID", global = true)]
        actor_id: Option<String>,
        /// Session identity retained only as asserted audit attribution.
        #[arg(long, env = "ENGRAM_SESSION_ID", global = true)]
        session_id: Option<String>,
        /// Optional free-form execution context attributed to this actor.
        #[arg(long, env = "ENGRAM_ACTOR_CONTEXT", global = true)]
        actor_context: Option<String>,
        /// Skill instruction that supplied this actor context, when available.
        #[arg(long, env = "ENGRAM_SOURCE_SKILL")]
        source_skill: Option<String>,
        #[command(subcommand)]
        operation: GraphCommand,
    },
    /// Serve the coding-agent local-work tools over MCP stdio.
    Mcp {
        /// Actor identity asserted by the host integration.
        #[arg(long)]
        actor_id: String,
        /// Durable runtime session identity used for task binding and privacy.
        #[arg(long)]
        session_id: String,
        /// Optional free-form execution context attributed to this actor.
        #[arg(long, env = "ENGRAM_ACTOR_CONTEXT")]
        actor_context: Option<String>,
        /// Skill instruction that supplied this actor context, when available.
        #[arg(long)]
        source_skill: Option<String>,
    },
    /// Serve the host-private behavioral-control protocol as JSON Lines.
    Control {
        /// Actor identity asserted by the host integration.
        #[arg(long)]
        actor_id: String,
        /// Durable runtime session identity fixed for this connection.
        #[arg(long)]
        session_id: String,
        /// Skill instruction that supplied this actor context, when available.
        #[arg(long)]
        source_skill: Option<String>,
    },
    /// Track work with thirteen words: next, ls, show, add, claim, update,
    /// gate, note, done, handoff, remember, memories, forget.
    ///
    /// The host fixes actor and session through the environment
    /// so an agent types only the word and its arguments.
    Work {
        /// Actor identity asserted by the invoking host or operator wrapper;
        /// defaults from the conventional OS-user environment, with a
        /// synthetic process fallback.
        #[arg(long, env = "ENGRAM_ACTOR_ID", global = true)]
        actor_id: Option<String>,
        /// Durable session identity used for ambient focus and cursors;
        /// defaults to one stable id for this process.
        #[arg(long, env = "ENGRAM_SESSION_ID", global = true)]
        session_id: Option<String>,
        /// Optional free-form execution context attributed to this actor;
        /// this never changes assignment, claim, or handoff identity.
        #[arg(long, env = "ENGRAM_ACTOR_CONTEXT", global = true)]
        actor_context: Option<String>,
        /// Skill instruction that supplied this actor context, when available.
        #[arg(long, env = "ENGRAM_SOURCE_SKILL")]
        source_skill: Option<String>,
        /// Print the structured receipt instead of text. Successful mutations whose
        /// session was defaulted also include top-level `effective_session_id`.
        #[arg(long, global = true)]
        json: bool,
        #[command(subcommand)]
        operation: Box<WorkCommand>,
    },
    /// Host-operator audited lifecycle exceptions; never exposed through MCP.
    Authority {
        #[command(subcommand)]
        operation: AuthorityCommand,
    },
    /// Host-operator behavioral-control policy administration.
    ControlPolicy {
        #[command(subcommand)]
        operation: ControlPolicyCommand,
    },
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum ControlAssuranceArg {
    #[value(name = "advisory")]
    Advisory,
    #[value(name = "turn_gated")]
    TurnGated,
    #[value(name = "action_gated")]
    ActionGated,
}

impl From<ControlAssuranceArg> for ControlAssurance {
    fn from(value: ControlAssuranceArg) -> Self {
        match value {
            ControlAssuranceArg::Advisory => Self::Advisory,
            ControlAssuranceArg::TurnGated => Self::TurnGated,
            ControlAssuranceArg::ActionGated => Self::ActionGated,
        }
    }
}

fn control_assurance_name(value: ControlAssurance) -> &'static str {
    match value {
        ControlAssurance::Advisory => "advisory",
        ControlAssurance::TurnGated => "turn_gated",
        ControlAssurance::ActionGated => "action_gated",
    }
}

fn warn_if_action_gated(value: ControlAssurance) {
    if value == ControlAssurance::ActionGated {
        eprintln!(
            "CONTROL WARNING: no current V1 host can bind at action_gated; recover with `engram control-policy set-required-assurance turn_gated --authorized-by <actor> --reason <reason> --idempotency-key <key>`"
        );
    }
}

fn warn_if_assurance_weakened(previous: ControlAssurance, current: ControlAssurance) {
    if current < previous {
        eprintln!(
            "CONTROL WARNING: project required assurance was lowered from {} to {}; future sessions may mediate less of the agent execution path",
            control_assurance_name(previous),
            control_assurance_name(current)
        );
    }
}

#[derive(Debug, Subcommand)]
enum ControlPolicyCommand {
    /// Activate a new immutable policy version with a different requirement.
    ///
    /// Activation bumps the project policy epoch. Issued grants then fail
    /// begin with `policy_epoch_changed` and must be re-evaluated; if the new
    /// requirement exceeds the host declaration, fresh evaluation instead
    /// fails `control_assurance_insufficient`. Begun grants remain
    /// checkpointable under their frozen prior basis.
    SetRequiredAssurance {
        #[arg(value_enum)]
        level: ControlAssuranceArg,
        /// Host/operator actor id attributed to the policy decision.
        #[arg(long)]
        authorized_by: String,
        /// Auditable reason for changing the project-wide requirement.
        #[arg(long)]
        reason: String,
        /// Durable caller key for exact receipt replay after an uncertain response.
        #[arg(long)]
        idempotency_key: String,
        /// Optional compare-and-swap guard from `engram doctor`.
        #[arg(long)]
        expected_policy_hash: Option<String>,
    },
    /// Activate a typed immutable obligation rule set for future observations.
    SetObligationRuleSet {
        /// Strict JSON object or @path to a JSON file containing the rule set.
        #[arg(long)]
        input: String,
        /// Host/operator actor id attributed to the policy decision.
        #[arg(long)]
        authorized_by: String,
        /// Auditable reason for selecting this rule set.
        #[arg(long)]
        reason: String,
        /// Durable caller key for exact receipt replay after an uncertain response.
        #[arg(long)]
        idempotency_key: String,
        /// Optional compare-and-swap guard from `engram doctor`.
        #[arg(long)]
        expected_policy_hash: Option<String>,
    },
}

const MAX_CONTROL_POLICY_CLI_INPUT_BYTES: u64 = 64 * 1024;

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct CliObligationRuleSet {
    schema_version: u16,
    rules: Vec<CliObligationRuleDefinition>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct CliObligationRuleDefinition {
    rule: CliBuiltinObligationRuleRef,
    trigger: BuiltinObligationTrigger,
    requirement: CliVerificationRequirement,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct CliBuiltinObligationRuleRef {
    rule_id: String,
    rule_version: u16,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct CliVerificationRequirement {
    check_kind: VerificationKind,
    #[serde(default)]
    check_fingerprint: Option<ObjectHash>,
    #[serde(default)]
    required_environment: Option<ObjectHash>,
}

impl From<CliObligationRuleSet> for ObligationRuleSet {
    fn from(value: CliObligationRuleSet) -> Self {
        Self {
            schema_version: value.schema_version,
            rules: value.rules.into_iter().map(Into::into).collect(),
        }
    }
}

impl From<CliObligationRuleDefinition> for ObligationRuleDefinition {
    fn from(value: CliObligationRuleDefinition) -> Self {
        Self {
            rule: BuiltinObligationRuleRef {
                rule_id: value.rule.rule_id,
                rule_version: value.rule.rule_version,
            },
            trigger: value.trigger,
            requirement: VerificationRequirement {
                check_kind: value.requirement.check_kind,
                check_fingerprint: value.requirement.check_fingerprint,
                required_environment: value.requirement.required_environment,
            },
        }
    }
}

#[derive(Debug, Subcommand)]
enum GraphCommand {
    /// Save planning state and inert history without execution authority.
    Save {
        /// Write to this file instead of the project-digest snapshot path.
        #[arg(long, conflicts_with = "stdout")]
        out: Option<PathBuf>,
        /// Write the disclosure artifact to stdout.
        #[arg(long, conflicts_with = "out")]
        stdout: bool,
        /// Include restricted memory bodies; secret references never widen.
        #[arg(long, requires = "reason")]
        include_restricted: bool,
        /// Audit reason required when restricted memory bodies are included.
        #[arg(long, requires = "include_restricted")]
        reason: Option<String>,
    },
    /// Recreate planning state and inert history in an empty project store.
    Load {
        /// Snapshot file written by `engram graph save`.
        file: PathBuf,
        /// Validate and report the exact landing plan without writing.
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Debug, Subcommand)]
enum WorkCommand {
    /// What is ready, what you hold, and what changed since your last call.
    Next {
        #[arg(long, default_value_t = 20)]
        limit: u32,
        /// Return the full structured projection instead of compact rows.
        #[arg(long)]
        verbose: bool,
        /// Asserted host/client context generation; a new value may reannounce project memories.
        #[arg(long)]
        context_generation: Option<String>,
    },
    /// List open work.
    Ls {
        /// Case-insensitive text over refs, titles, outcomes, and labels.
        #[arg(long)]
        search: Option<String>,
        /// Only items with an active blocker or incomplete prerequisite.
        #[arg(long)]
        blocked: bool,
        /// Only items assigned to you or held by this session.
        #[arg(long)]
        mine: bool,
        /// Include completed, cancelled, and superseded items.
        #[arg(long)]
        all: bool,
        /// Exact case-insensitive label.
        #[arg(long)]
        label: Option<String>,
        #[arg(long, default_value_t = 20)]
        limit: u32,
        /// Return the full structured projection instead of compact rows.
        #[arg(long)]
        verbose: bool,
    },
    /// One item: outcome, acceptance, holder, blockers, reminders.
    Show {
        /// Short work ref or full UUID; later words default to it.
        work_ref: String,
        /// Full notes, oldest first, with an exact omitted count if byte-bounded.
        #[arg(long)]
        notes: bool,
    },
    /// Create work from a title; outcome and acceptance criteria are welcome.
    Add {
        /// Initial attributed note; repeatable and atomic with creation.
        #[arg(long = "note", value_name = "TEXT")]
        notes: Vec<String>,
        title: String,
        /// Defaults to the title.
        #[arg(long)]
        outcome: Option<String>,
        /// Acceptance criterion `done` is checked against; repeatable.
        /// Defaults to one criterion "<title> is done".
        #[arg(long = "accept", value_name = "CRITERION")]
        acceptance: Vec<String>,
        /// Add as a child of this item instead of a root.
        #[arg(long, value_name = "REF")]
        under: Option<String>,
        /// Make the child optional for parent completion; requires --under.
        #[arg(long, requires = "under")]
        optional: bool,
        /// 0 (highest) through 4.
        #[arg(long)]
        priority: Option<i32>,
        /// Label; repeatable.
        #[arg(long = "label", value_name = "LABEL")]
        labels: Vec<String>,
        #[arg(long)]
        assignee: Option<String>,
        #[arg(long, value_enum)]
        kind: Option<WorkKindArg>,
    },
    /// Hold an item before changing anything; later words default to it.
    Claim {
        work_ref: String,
        /// Claim lifetime in seconds (default one hour).
        #[arg(long, value_name = "SECONDS")]
        ttl: Option<i64>,
        /// Attributed reason for taking over a different holder's lapsed claim.
        #[arg(long, value_name = "REASON")]
        recover: Option<String>,
    },
    /// Exactly one action: lifecycle change or one audited field revision.
    Update(Box<WorkUpdateArgs>),
    /// Record a gate on held open work or a late finding on completed work.
    Gate {
        /// Item to record the gate on; defaults to the focus.
        #[arg(long, value_name = "REF")]
        work_ref: Option<String>,
        /// Stable gate name, normalized case-insensitively.
        name: String,
        /// Failure label; repeatable. Omit only when the gate passed.
        #[arg(
            long = "failed",
            value_name = "FAILURE",
            action = ArgAction::Append,
            num_args = 1
        )]
        failed: Vec<String>,
        /// Opaque evidence reference; a path or URL by convention, never ingested.
        #[arg(long = "ref", value_name = "OPAQUE_REFERENCE")]
        evidence_ref: Option<String>,
    },
    /// Store one attributed project memory.
    Remember {
        text: String,
        /// Safe permanent project-memory key.
        #[arg(long)]
        key: Option<String>,
    },
    /// List/search project memories, or read one key in full.
    Memories {
        /// Query text, or the exact key when --full is present.
        query: Option<String>,
        /// Continue an unfiltered key-ordered listing.
        #[arg(long, conflicts_with = "full")]
        after: Option<String>,
        /// Return the dedicated full body for the positional key.
        #[arg(long)]
        full: bool,
    },
    /// Permanently retire one project-memory key.
    Forget { key: String },
    /// Record a note on open work (an observation without a claim), or a late finding on completed work.
    Note {
        /// An optional item ref, then the note text.
        #[arg(required = true, num_args = 1..=2, value_name = "[REF] TEXT")]
        args: Vec<String>,
        /// Evidence pointer such as a path or URL; repeatable.
        #[arg(long = "ref", value_name = "PATH_OR_URL")]
        refs: Vec<String>,
    },
    /// Complete the item you hold.
    Done {
        /// An optional item ref and what was delivered.
        #[arg(num_args = 0..=2, value_name = "[REF] [SUMMARY]")]
        args: Vec<String>,
        /// Acceptance note recorded against every criterion.
        #[arg(long)]
        note: Option<String>,
    },
    /// Offer the item you hold to another session, accept an offer, or cancel yours.
    Handoff {
        /// Item to hand off; defaults to the focus.
        work_ref: Option<String>,
        /// Session that receives the item.
        #[arg(long, value_name = "ACTOR")]
        to: Option<String>,
        /// Checkpoint summary recorded with the offer.
        #[arg(long, requires = "to")]
        summary: Option<String>,
        /// Accept the offer made to this session.
        #[arg(long, conflicts_with_all = ["to", "cancel"])]
        accept: bool,
        /// Cancel your outstanding offer and say why.
        #[arg(long, value_name = "REASON", conflicts_with = "to")]
        cancel: Option<String>,
    },
    /// Six-operation JSON protocol for hosts and operators.
    Core {
        #[command(subcommand)]
        operation: Box<CoreWorkCommand>,
    },
}

impl WorkCommand {
    /// Only successful mutation receipts need an in-band handle for a
    /// process-defaulted session. Read receipts and every explicitly bound
    /// surface retain their existing structured shape.
    const fn returns_mutation_receipt(&self) -> bool {
        match self {
            Self::Add { .. }
            | Self::Claim { .. }
            | Self::Update { .. }
            | Self::Gate { .. }
            | Self::Remember { .. }
            | Self::Forget { .. }
            | Self::Note { .. }
            | Self::Done { .. }
            | Self::Handoff { .. } => true,
            Self::Next { .. }
            | Self::Ls { .. }
            | Self::Show { .. }
            | Self::Memories { .. }
            | Self::Core { .. } => false,
        }
    }
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum WorkKindArg {
    Task,
    Bug,
    Feature,
    Epic,
    Chore,
    Research,
}

impl From<WorkKindArg> for WorkItemKind {
    fn from(value: WorkKindArg) -> Self {
        match value {
            WorkKindArg::Task => Self::Task,
            WorkKindArg::Bug => Self::Bug,
            WorkKindArg::Feature => Self::Feature,
            WorkKindArg::Epic => Self::Epic,
            WorkKindArg::Chore => Self::Chore,
            WorkKindArg::Research => Self::Research,
        }
    }
}

#[derive(Debug, Subcommand)]
enum CoreWorkCommand {
    /// Return ambient focus, ready candidates, and new project-feed entries.
    Next {
        #[arg(long, default_value_t = 20)]
        limit: u32,
        /// Acknowledge the exact `delivered_through` cursor from the prior page.
        #[arg(long)]
        acknowledge_through: Option<i64>,
        /// Opaque `delivery_token` returned with that same prior page.
        #[arg(long)]
        acknowledge_token: Option<String>,
        /// Comma-separated response sections: focus,ready,catalog,changes,memories.
        /// Omit for all sections.
        #[arg(long, value_delimiter = ',')]
        sections: Vec<String>,
        #[arg(long)]
        search: Option<String>,
        #[arg(long, value_delimiter = ',')]
        lifecycles: Vec<String>,
        #[arg(long, value_delimiter = ',')]
        availabilities: Vec<String>,
        #[arg(long)]
        blocked_only: bool,
        #[arg(long)]
        assigned_to: Option<String>,
        #[arg(long)]
        label: Option<String>,
        #[arg(long)]
        catalog_after: Option<String>,
        #[arg(long)]
        context_generation: Option<String>,
    },
    /// Select and inspect ambient work without claiming it.
    Focus {
        /// Short work ref or full UUID.
        work_ref: String,
    },
    /// Create a root or atomically decompose ambient work.
    Propose {
        /// Short ref or UUID of the parent to decompose; selects focus first.
        #[arg(long)]
        work_ref: Option<String>,
        /// JSON object or @path to a JSON file.
        #[arg(long)]
        input: String,
    },
    /// Apply a typed update to ambient work.
    Update {
        /// Short ref or UUID to act on; selects focus first.
        #[arg(long)]
        work_ref: Option<String>,
        /// JSON object or @path to a JSON file.
        #[arg(long)]
        input: String,
    },
    /// Complete ambient work with evidence and acceptance results.
    Complete {
        /// Short ref or UUID to complete; selects focus first.
        #[arg(long)]
        work_ref: Option<String>,
        /// JSON object or @path to a JSON file.
        #[arg(long)]
        input: String,
    },
    /// Offer, accept, or cancel an ambient claim handoff.
    Handoff {
        /// Short ref or UUID to hand off; selects focus first.
        #[arg(long)]
        work_ref: Option<String>,
        /// JSON object or @path to a JSON file.
        #[arg(long)]
        input: String,
    },
}

#[derive(Debug, Subcommand)]
enum AuthorityCommand {
    /// Waive one exact open execution obligation with attributed reason.
    WaiveObligation {
        #[arg(long)]
        obligation_id: String,
        #[arg(long)]
        expected_definition: String,
        #[arg(long)]
        waived_by: String,
        #[arg(long)]
        reason: String,
        #[arg(long)]
        idempotency_key: String,
    },
}

const CLI_STACK_BYTES: usize = 8 * 1024 * 1024;

fn main() -> Result<ExitCode> {
    // The combined clap command graph is parsed and driven on this named
    // thread because Windows' default main-thread stack is too small for the
    // full CLI enum. Tokio worker futures are not affected; only parse and
    // `block_on` stay on the enlarged stack.
    match std::thread::Builder::new()
        .name("engram-cli".into())
        .stack_size(CLI_STACK_BYTES)
        .spawn(run_cli)?
        .join()
    {
        Ok(result) => result,
        Err(payload) => std::panic::resume_unwind(payload),
    }
}

#[tokio::main]
#[allow(
    clippy::too_many_lines,
    reason = "top-level CLI dispatch keeps every operator and agent surface exhaustive in one match"
)]
async fn run_cli() -> Result<ExitCode> {
    let cli = Cli::parse();
    let (project_id, database, root) = resolve_project(&cli.project_file, cli.home)?;
    let identity = resolve_host_path_identity(&root, cli.host_path_policy);
    match cli.command {
        Command::Init {
            required_assurance,
            authorized_by,
            reason,
        } => initialize(
            &database,
            identity,
            required_assurance.map(Into::into),
            authorized_by,
            reason,
        )?,
        Command::Doctor {
            json,
            recover_policy,
            repair_projections,
        } => doctor(
            &database,
            identity,
            &project_id,
            json,
            recover_policy,
            repair_projections,
        )?,
        Command::Backup { out } => backup(&database, out)?,
        Command::Restore { from, replace } => restore(&database, &from, replace)?,
        Command::Graph {
            actor_id,
            session_id,
            actor_context,
            source_skill,
            operation,
        } => run_graph_from_cli(
            database,
            project_id,
            actor_id,
            session_id,
            actor_context,
            source_skill,
            operation,
        )?,
        Command::Mcp {
            actor_id,
            session_id,
            actor_context,
            source_skill,
        } => {
            serve_mcp(McpServer::new_with_actor_context(
                database,
                project_id,
                actor_id,
                SessionId(session_id),
                source_skill,
                actor_context,
            ))
            .await?;
        }
        Command::Control {
            actor_id,
            session_id,
            source_skill,
        } => serve_control(
            database,
            identity,
            project_id,
            actor_id,
            session_id,
            source_skill,
        )?,
        Command::Work {
            actor_id,
            session_id,
            actor_context,
            source_skill,
            json,
            operation,
        } => {
            let attribution = resolve_shell_work_attribution(actor_id, session_id);
            attribution.print_notices();
            let context = WorkContext {
                database,
                project_id,
                actor_id: attribution.actor_id,
                session_id: SessionId(attribution.session_id),
                actor_context,
                attribution_defaults: attribution.defaults,
                source_skill,
            };
            return match *operation {
                WorkCommand::Core { operation } => run_core_work(context, *operation),
                operation => run_work(context, json, operation),
            };
        }
        Command::Authority { operation } => {
            run_authority(&database, identity, operation)?;
        }
        Command::ControlPolicy { operation } => {
            run_control_policy(&database, identity, project_id, operation)?;
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// Host-fixed project, actor, and session for one shell work invocation.
struct WorkContext {
    database: PathBuf,
    project_id: ProjectId,
    actor_id: String,
    session_id: SessionId,
    actor_context: Option<String>,
    attribution_defaults: WorkAttributionDefaults,
    source_skill: Option<String>,
}

fn run_control_policy(
    database: &Path,
    identity: Option<HostPathPolicy>,
    _project_id: ProjectId,
    operation: ControlPolicyCommand,
) -> Result<()> {
    let mut store = SqliteStore::open_with_host_path_identity(database, identity)
        .with_context(|| format!("failed to open {}", database.display()))?;
    let value = match operation {
        ControlPolicyCommand::SetRequiredAssurance {
            level,
            authorized_by,
            reason,
            idempotency_key,
            expected_policy_hash,
        } => {
            let level = ControlAssurance::from(level);
            let expected_policy = parse_expected_policy_hash(expected_policy_hash)?;
            let receipt = store.set_required_control_assurance(
                level,
                &control_policy_actor(authorized_by),
                &reason,
                &idempotency_key,
                expected_policy.as_ref(),
                chrono::Utc::now(),
                &DevelopmentNoopRedactor,
            )?;
            if receipt.changed {
                eprintln!(
                    "WARNING: policy administrator identity is asserted host context, not an authenticated identity"
                );
            }
            warn_if_assurance_weakened(
                receipt.previous_required_assurance,
                receipt.required_assurance,
            );
            warn_if_action_gated(receipt.required_assurance);
            serde_json::to_value(receipt)?
        }
        ControlPolicyCommand::SetObligationRuleSet {
            input,
            authorized_by,
            reason,
            idempotency_key,
            expected_policy_hash,
        } => {
            let input: CliObligationRuleSet = parse_bounded_json_input(
                &input,
                "obligation rule-set",
                MAX_CONTROL_POLICY_CLI_INPUT_BYTES,
            )?;
            let rule_set = ObligationRuleSet::from(input);
            let expected_policy = parse_expected_policy_hash(expected_policy_hash)?;
            let receipt = store.set_obligation_rule_set(
                &rule_set,
                &control_policy_actor(authorized_by),
                &reason,
                &idempotency_key,
                expected_policy.as_ref(),
                chrono::Utc::now(),
                &DevelopmentNoopRedactor,
            )?;
            if receipt.changed {
                eprintln!(
                    "WARNING: policy administrator identity is asserted host context, not an authenticated identity"
                );
            }
            serde_json::to_value(receipt)?
        }
    };
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}

fn control_policy_actor(actor_id: String) -> ActorContext {
    ActorContext {
        actor_id,
        actor_kind: "host_operator".into(),
        assurance: AssuranceLevel::Asserted,
        run_id: None,
        session_id: None,
        source_tool: Some("cli:control_policy".into()),
        source_skill: None,
        provenance_chain: Vec::new(),
        reason: "authorize a project-scoped behavioral-control policy change".into(),
    }
}

fn parse_expected_policy_hash(value: Option<String>) -> Result<Option<ObjectHash>> {
    value
        .map(|value| {
            ObjectHash::from_str(&value)
                .map_err(|message| anyhow::anyhow!("invalid expected policy hash: {message}"))
        })
        .transpose()
}

fn run_authority(
    database: &Path,
    identity: Option<HostPathPolicy>,
    operation: AuthorityCommand,
) -> Result<()> {
    let mut store = SqliteStore::open_with_host_path_identity(database, identity)
        .with_context(|| format!("failed to open {}", database.display()))?;
    let now = chrono::Utc::now();
    let value = match operation {
        AuthorityCommand::WaiveObligation {
            obligation_id,
            expected_definition,
            waived_by,
            reason,
            idempotency_key,
        } => {
            eprintln!(
                "WARNING: obligation waiver identity is asserted context; this shell path is neither authenticated nor bound to a live control session/run"
            );
            let obligation_id = WorkObligationId(
                uuid::Uuid::parse_str(&obligation_id).context("invalid work obligation id")?,
            );
            let expected_definition = ObjectHash::from_str(&expected_definition)
                .map_err(|message| anyhow::anyhow!("invalid definition hash: {message}"))?;
            serde_json::to_value(store.waive_work_obligation(
                &WaiveWorkObligationRequest {
                    obligation_id,
                    expected_definition,
                    waived_by: waived_by.clone(),
                    reason,
                    actor: ActorContext {
                        actor_id: waived_by,
                        actor_kind: "host_operator".into(),
                        assurance: AssuranceLevel::Asserted,
                        run_id: None,
                        session_id: None,
                        source_tool: Some("cli:authority_waive_obligation".into()),
                        source_skill: None,
                        provenance_chain: Vec::new(),
                        reason: "waive an exact host-observed work obligation".into(),
                    },
                    idempotency_key,
                    waived_at: now,
                },
                &DevelopmentNoopRedactor,
            )?)?
        }
    };
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}

#[allow(
    clippy::too_many_lines,
    reason = "each word's flag translation stays beside the others so the thirteen-word surface is reviewable in one place"
)]
fn run_work(context: WorkContext, json: bool, operation: WorkCommand) -> Result<ExitCode> {
    let effective_session_id =
        (json && context.attribution_defaults.session && operation.returns_mutation_receipt())
            .then(|| context.session_id.clone());
    let verbs = AgentVerbs::new_with_attribution(
        context.database,
        context.project_id,
        context.actor_id,
        context.session_id,
        context.source_skill,
        context.actor_context,
        context.attribution_defaults,
    );
    let now = chrono::Utc::now();
    let outcome = match operation {
        WorkCommand::Next {
            limit,
            verbose,
            context_generation,
        } => verbs.next(
            &NextInput {
                limit: Some(limit),
                verbose,
                context_generation,
            },
            now,
        ),
        WorkCommand::Ls {
            search,
            blocked,
            mine,
            all,
            label,
            limit,
            verbose,
        } => verbs.ls(
            &LsInput {
                search,
                blocked,
                mine,
                all,
                label,
                limit: Some(limit),
                verbose,
            },
            now,
        ),
        WorkCommand::Show { work_ref, notes } => verbs.show_with_notes(&work_ref, notes, now),
        WorkCommand::Add {
            notes,
            title,
            outcome,
            acceptance,
            under,
            optional,
            priority,
            labels,
            assignee,
            kind,
        } => verbs.add(
            AddInput {
                notes,
                title,
                outcome,
                acceptance,
                under,
                optional,
                priority,
                labels,
                assignee,
                kind: kind.map(Into::into),
            },
            now,
        ),
        WorkCommand::Claim {
            work_ref,
            ttl,
            recover,
        } => verbs.claim(
            ClaimInput {
                work_ref,
                ttl_seconds: ttl,
                recover,
            },
            now,
        ),
        WorkCommand::Update(args) => {
            let WorkUpdateArgs {
                work_ref,
                release,
                reason,
                blocked,
                unblock,
                assignee,
                priority,
                defer,
                title,
                outcome,
                kind,
                acceptance,
                labels,
                unlabels,
                cancel,
                after,
                drop_after,
                waive,
                supersede_with,
            } = *args;
            if reason.is_some() && !release && supersede_with.is_none() && waive.is_none() {
                bail!("--reason is only valid with --release, --waive, or --supersede-with");
            }
            let revise = assignee.is_some()
                || priority.is_some()
                || defer.is_some()
                || title.is_some()
                || outcome.is_some()
                || acceptance.is_some()
                || kind.is_some()
                || !labels.is_empty()
                || !unlabels.is_empty();
            let selected = usize::from(release)
                + usize::from(blocked.is_some())
                + usize::from(unblock)
                + usize::from(cancel.is_some())
                + usize::from(after.is_some())
                + usize::from(drop_after.is_some())
                + usize::from(waive.is_some())
                + usize::from(supersede_with.is_some())
                + usize::from(revise);
            if selected != 1 {
                bail!(
                    "update needs exactly one action: --release, --blocked WHY, --unblock, --cancel REASON, --after REF, --drop-after REF, --waive REF --reason WHY, --supersede-with REF --reason WHY, or field changes (--title, --outcome, --accept, --assignee, --priority, --defer, --kind, --label, --unlabel)"
                );
            }
            let action = if release {
                UpdateAction::Release { reason }
            } else if let Some(detail) = blocked {
                UpdateAction::Blocked { detail }
            } else if unblock {
                UpdateAction::Unblock
            } else if let Some(reason) = cancel {
                UpdateAction::Cancel { reason }
            } else if let Some(prerequisite) = after {
                UpdateAction::After { prerequisite }
            } else if let Some(prerequisite) = drop_after {
                UpdateAction::DropAfter { prerequisite }
            } else if let Some(child) = waive {
                let reason = reason
                    .filter(|value| !value.trim().is_empty())
                    .ok_or_else(|| anyhow::anyhow!("--waive requires --reason WHY"))?;
                UpdateAction::WaiveRequiredChild { child, reason }
            } else if let Some(replacement) = supersede_with {
                let reason = reason
                    .filter(|value| !value.trim().is_empty())
                    .ok_or_else(|| anyhow::anyhow!("--supersede-with requires --reason WHY"))?;
                UpdateAction::Supersede {
                    replacement,
                    reason,
                }
            } else {
                let defer = defer
                    .as_deref()
                    .map(parse_defer_date)
                    .transpose()
                    .map_err(|message| anyhow::anyhow!("invalid --defer: {message}"))?;
                UpdateAction::Revise {
                    title,
                    outcome,
                    acceptance,
                    assignee,
                    priority,
                    defer,
                    kind: kind.map(Into::into),
                    labels,
                    unlabels,
                }
            };
            verbs.update(UpdateInput { work_ref, action }, now)
        }
        WorkCommand::Gate {
            work_ref,
            name,
            failed,
            evidence_ref,
        } => verbs.gate(
            GateInput {
                work_ref,
                name,
                failed,
                evidence_ref,
            },
            now,
        ),
        WorkCommand::Remember { text, key } => verbs.remember(RememberInput { text, key }, now),
        WorkCommand::Memories { query, after, full } => {
            verbs.memories(&MemoriesInput { query, after, full }, now)
        }
        WorkCommand::Forget { key } => verbs.forget(ForgetInput { key }, now),
        WorkCommand::Note { mut args, refs } => {
            let (work_ref, text) = if args.len() >= 2 {
                let text = args.remove(1);
                (Some(args.remove(0)), text)
            } else {
                let text = args.pop().unwrap_or_default();
                if looks_like_work_ref(&text) {
                    bail!("note needs text after the item ref");
                }
                (None, text)
            };
            verbs.note(
                &NoteInput {
                    work_ref,
                    text,
                    refs,
                },
                now,
            )
        }
        WorkCommand::Done { mut args, note } => {
            let (work_ref, summary) = match args.len() {
                0 => (None, None),
                1 => {
                    let value = args.remove(0);
                    if looks_like_work_ref(&value) {
                        (Some(value), None)
                    } else {
                        (None, Some(value))
                    }
                }
                _ => {
                    let summary = args.remove(1);
                    (Some(args.remove(0)), Some(summary))
                }
            };
            verbs.done(
                DoneInput {
                    work_ref,
                    summary,
                    note,
                },
                now,
            )
        }
        WorkCommand::Handoff {
            work_ref,
            to,
            summary,
            accept,
            cancel,
        } => {
            let action = match (to, accept, cancel) {
                (Some(to), false, None) => HandoffAction::Offer {
                    to,
                    summary,
                    ttl_seconds: None,
                },
                (None, true, None) => HandoffAction::Accept,
                (None, false, Some(reason)) => HandoffAction::Cancel { reason },
                _ => bail!("handoff needs exactly one of --to ACTOR, --accept, or --cancel REASON"),
            };
            verbs.handoff(HandoffInput { work_ref, action }, now)
        }
        WorkCommand::Core { .. } => bail!("core operations are dispatched separately"),
    };
    match outcome {
        Ok(receipt) => {
            let receipt = if let Some(effective_session_id) = &effective_session_id {
                receipt.with_effective_session_id(effective_session_id)
            } else {
                receipt
            };
            if json {
                println!("{}", serialize_agent_receipt(&receipt.value)?);
            } else {
                println!("{}", receipt.text());
            }
            Ok(if receipt.owed {
                ExitCode::from(2)
            } else {
                ExitCode::SUCCESS
            })
        }
        Err(error) => {
            let guidance = error.guidance();
            if json {
                eprintln!(
                    "{}",
                    serde_json::to_string_pretty(&{
                        let mut value = store_error_value(&error.error);
                        value["error"]["reminders"] = serde_json::json!(guidance.reminders);
                        value["error"]["next"] = serde_json::json!(guidance.next);
                        value
                    })?
                );
            } else {
                eprintln!("error: {error}");
                for reminder in &guidance.reminders {
                    eprintln!("  - {reminder}");
                }
                if !guidance.next.is_empty() {
                    eprintln!("next:");
                    for command in &guidance.next {
                        eprintln!("  {command}");
                    }
                }
            }
            Ok(ExitCode::FAILURE)
        }
    }
}

fn serialize_agent_receipt(value: &serde_json::Value) -> serde_json::Result<String> {
    // Agent response budgets are measured against compact JSON. Emitting that
    // exact representation keeps the CLI transport inside the same hard bound.
    serde_json::to_string(value)
}

fn run_core_work(context: WorkContext, operation: CoreWorkCommand) -> Result<ExitCode> {
    let service = LocalWorkService::new_with_attribution(
        context.database,
        context.project_id,
        context.actor_id,
        context.session_id,
        context.source_skill,
        context.actor_context,
        context.attribution_defaults,
    );
    let now = chrono::Utc::now();
    let mut completion_refused = false;
    let result: Result<serde_json::Value, StoreError> = match operation {
        CoreWorkCommand::Next {
            limit,
            acknowledge_through,
            acknowledge_token,
            sections,
            search,
            lifecycles,
            availabilities,
            blocked_only,
            assigned_to,
            label,
            catalog_after,
            context_generation,
        } => service
            .work_next_with_delivery_token(
                limit,
                acknowledge_through,
                acknowledge_token.as_deref(),
                WorkNextQuery {
                    sections: parse_enum_values::<WorkNextSection>(&sections, "--sections")?,
                    search,
                    lifecycles: parse_enum_values::<WorkLifecycle>(&lifecycles, "--lifecycles")?,
                    availabilities: parse_enum_values::<WorkAvailability>(
                        &availabilities,
                        "--availabilities",
                    )?,
                    blocked_only,
                    assigned_to,
                    label,
                    after: catalog_after,
                    context_generation,
                },
                now,
            )
            .and_then(|value| serde_json::to_value(value).map_err(StoreError::from)),
        CoreWorkCommand::Focus { work_ref } => service
            .work_focus(&work_ref, now)
            .and_then(|value| serde_json::to_value(value).map_err(StoreError::from)),
        CoreWorkCommand::Propose { work_ref, input } => {
            let input = parse_json_input::<WorkProposeInput>(&input)?;
            service
                .work_propose_on(work_ref.as_deref(), input, now)
                .and_then(|value| serde_json::to_value(value).map_err(StoreError::from))
        }
        CoreWorkCommand::Update { work_ref, input } => {
            let input = parse_json_input::<WorkUpdateInput>(&input)?;
            service
                .work_update_on(work_ref.as_deref(), input, now)
                .and_then(|value| serde_json::to_value(value).map_err(StoreError::from))
        }
        CoreWorkCommand::Complete { work_ref, input } => {
            let input = parse_json_input::<WorkCompleteInput>(&input)?;
            service
                .work_complete_on(work_ref.as_deref(), input, now)
                .and_then(|value| {
                    completion_refused = matches!(&value, WorkCompleteResult::Refused(_));
                    serde_json::to_value(value).map_err(StoreError::from)
                })
        }
        CoreWorkCommand::Handoff { work_ref, input } => {
            let input = parse_json_input::<WorkHandoffInput>(&input)?;
            service
                .work_handoff_on(work_ref.as_deref(), input, now)
                .and_then(|value| serde_json::to_value(value).map_err(StoreError::from))
        }
    };
    match result {
        Ok(value) => {
            println!("{}", serde_json::to_string_pretty(&value)?);
            Ok(if completion_refused {
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            })
        }
        Err(error) => {
            eprintln!(
                "{}",
                serde_json::to_string_pretty(&store_error_value(&error))?
            );
            Ok(ExitCode::FAILURE)
        }
    }
}

fn parse_enum_values<T>(values: &[String], field: &str) -> Result<Vec<T>>
where
    T: serde::de::DeserializeOwned,
{
    values
        .iter()
        .map(|value| {
            serde_json::from_value(serde_json::Value::String(value.trim().to_lowercase()))
                .with_context(|| format!("invalid value {value:?} for {field}"))
        })
        .collect()
}

fn parse_json_input<T: serde::de::DeserializeOwned>(input: &str) -> Result<T> {
    let json = if let Some(path) = input.strip_prefix('@') {
        fs::read_to_string(path).with_context(|| format!("failed to read JSON input {path}"))?
    } else {
        input.to_owned()
    };
    serde_json::from_str(&json).context("invalid work operation JSON")
}

fn parse_bounded_json_input<T: serde::de::DeserializeOwned>(
    input: &str,
    label: &str,
    max_bytes: u64,
) -> Result<T> {
    let json = if let Some(path) = input.strip_prefix('@') {
        let file = fs::File::open(path)
            .with_context(|| format!("failed to open {label} JSON input {path}"))?;
        let mut bytes = Vec::new();
        file.take(max_bytes + 1)
            .read_to_end(&mut bytes)
            .with_context(|| format!("failed to read {label} JSON input {path}"))?;
        if bytes.len() as u64 > max_bytes {
            bail!("{label} JSON input exceeds the {max_bytes}-byte limit");
        }
        String::from_utf8(bytes)
            .with_context(|| format!("{label} JSON input {path} is not UTF-8"))?
    } else {
        if input.len() as u64 > max_bytes {
            bail!("{label} JSON input exceeds the {max_bytes}-byte limit");
        }
        input.to_owned()
    };
    serde_json::from_str(&json).with_context(|| format!("invalid {label} JSON"))
}

fn serve_control(
    database: PathBuf,
    identity: Option<HostPathPolicy>,
    project_id: ProjectId,
    actor_id: String,
    session_id: String,
    source_skill: Option<String>,
) -> Result<()> {
    let mut server = HostControlServer::open_with_host_path_identity(
        database,
        identity,
        project_id,
        actor_id,
        SessionId(session_id),
        source_skill,
    )
    .context("failed to start Engram host-control service")?;
    server
        .serve(
            BufReader::new(io::stdin().lock()),
            BufWriter::new(io::stdout().lock()),
        )
        .context("Engram host-control stdio service stopped with an error")
}

/// Resolves the stable project id, its host-local database, and the project
/// root (the directory holding the project file).
fn resolve_project(
    project_file: &Path,
    home: Option<PathBuf>,
) -> Result<(ProjectId, PathBuf, PathBuf)> {
    let project_id = fs::read_to_string(project_file)
        .with_context(|| format!("failed to read {}", project_file.display()))?;
    let project_id = project_id.trim();
    if project_id.is_empty() {
        bail!("project id in {} is empty", project_file.display());
    }
    let home = home.or_else(|| env::var_os("ENGRAM_HOME").map(PathBuf::from));
    let home = home.context("pass --home or set ENGRAM_HOME")?;
    let project_id = ProjectId(project_id.to_owned());
    let database = project_database_path(&home, &project_id);
    let root = project_file
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
    Ok((project_id, database, root))
}

async fn serve_mcp(server: McpServer) -> Result<()> {
    let server = server
        .serve(stdio())
        .await
        .context("failed to start Engram MCP stdio server")?;
    server
        .waiting()
        .await
        .context("Engram MCP stdio server stopped with an error")?;
    Ok(())
}

/// Flat flags for one planning/lifecycle update, boxed in the command enum.
#[derive(clap::Args, Debug)]
struct WorkUpdateArgs {
    /// Item to act on; defaults to the focus.
    work_ref: Option<String>,
    /// Release your claim.
    #[arg(long)]
    release: bool,
    /// Reason recorded with --release or required by --waive and --supersede-with.
    #[arg(long)]
    reason: Option<String>,
    /// Mark the item blocked and say why.
    #[arg(long, value_name = "WHY")]
    blocked: Option<String>,
    /// Clear the item's single active blocker.
    #[arg(long)]
    unblock: bool,
    #[arg(long)]
    assignee: Option<String>,
    /// 0 (highest) through 4.
    #[arg(long)]
    priority: Option<i32>,
    /// Defer until DATE (RFC 3339, YYYY-MM-DD, or YYYY-MM-DDTHH:MM:SS UTC).
    #[arg(long, value_name = "DATE")]
    defer: Option<String>,
    #[arg(long)]
    title: Option<String>,
    #[arg(long)]
    outcome: Option<String>,
    /// Replace the work kind.
    #[arg(long, value_enum)]
    kind: Option<WorkKindArg>,
    /// Replace the whole acceptance list; repeat for multiple criteria.
    #[arg(long = "accept", value_name = "CRITERION", action = ArgAction::Append, num_args = 1)]
    acceptance: Option<Vec<String>>,
    /// Add a label; repeatable.
    #[arg(long = "label", value_name = "LABEL")]
    labels: Vec<String>,
    /// Remove a label; repeatable.
    #[arg(long = "unlabel", value_name = "LABEL")]
    unlabels: Vec<String>,
    /// Cancel the item and say why.
    #[arg(long, value_name = "REASON")]
    cancel: Option<String>,
    /// Make this item wait for another open item.
    #[arg(long, value_name = "REF")]
    after: Option<String>,
    /// Remove one prerequisite edge from this item.
    #[arg(long, value_name = "REF")]
    drop_after: Option<String>,
    /// Waive one cancelled or superseded required child; requires --reason.
    #[arg(long, value_name = "REF")]
    waive: Option<String>,
    /// Supersede this item with another item; requires --reason.
    #[arg(long, value_name = "REF")]
    supersede_with: Option<String>,
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::*;

    #[test]
    fn complete_cli_command_graph_fits_the_configured_parse_stack() {
        std::thread::Builder::new()
            .stack_size(CLI_STACK_BYTES)
            .spawn(|| Cli::command().debug_assert())
            .expect("spawn CLI command-graph test")
            .join()
            .expect("CLI command graph remains valid");
    }

    #[test]
    fn graph_save_is_operator_only_and_has_exclusive_destinations() {
        let parsed = Cli::try_parse_from([
            "engram",
            "graph",
            "--actor-id",
            "operator",
            "--session-id",
            "operator-session",
            "save",
            "--stdout",
            "--include-restricted",
            "--reason",
            "incident recovery",
        ])
        .expect("parse graph save");
        let Command::Graph { operation, .. } = parsed.command else {
            panic!("graph save did not parse as the operator graph command");
        };
        assert!(matches!(
            operation,
            GraphCommand::Save {
                stdout: true,
                include_restricted: true,
                reason: Some(ref reason),
                out: None,
            } if reason == "incident recovery"
        ));
        assert!(
            Cli::try_parse_from([
                "engram",
                "graph",
                "save",
                "--stdout",
                "--out",
                "snapshot.json",
            ])
            .is_err()
        );
        assert!(Cli::try_parse_from(["engram", "work", "graph", "save", "--stdout"]).is_err());
        assert!(
            Cli::try_parse_from([
                "engram",
                "graph",
                "save",
                "--stdout",
                "--include-restricted",
            ])
            .is_err()
        );
        assert!(
            Cli::try_parse_from([
                "engram",
                "graph",
                "save",
                "--stdout",
                "--reason",
                "not widening",
            ])
            .is_err()
        );
    }

    #[test]
    fn agent_json_emission_uses_the_response_budget_representation() {
        let limit = engram::work_service::MAX_AGENT_WORK_RESPONSE_BYTES;
        let value = serde_json::json!({ "a": "x".repeat(limit - 8) });
        let emitted = serialize_agent_receipt(&value).expect("serialize agent receipt");
        assert_eq!(emitted.len(), limit);
        assert!(
            serde_json::to_string_pretty(&value)
                .expect("serialize pretty comparison")
                .len()
                > limit
        );
        assert!(!emitted.contains('\n'));
    }

    #[test]
    fn agent_work_cli_has_no_grant_surface() {
        assert!(
            Cli::try_parse_from([
                "engram",
                "work",
                "--actor-id",
                "agent",
                "--session-id",
                "session",
                "--authority-grant",
                "not-a-token",
                "next",
            ])
            .is_err()
        );
        assert!(
            Cli::try_parse_from([
                "engram",
                "mcp",
                "--actor-id",
                "agent",
                "--session-id",
                "session",
                "--work-authority-grant",
                "not-a-token",
            ])
            .is_err()
        );
        let command = Cli::command();
        let authority = command
            .find_subcommand("authority")
            .expect("authority command remains for obligation waiver");
        assert_eq!(
            authority
                .get_subcommands()
                .map(|subcommand| subcommand.get_name().to_owned())
                .collect::<Vec<_>>(),
            vec!["waive-obligation"]
        );
    }

    #[test]
    fn every_agent_word_classifies_its_structured_receipt_exhaustively() {
        let cases: &[(&[&str], bool)] = &[
            (&["next"], false),
            (&["ls"], false),
            (&["show", "w-000000000001"], false),
            (&["add", "new work"], true),
            (&["claim", "w-000000000001"], true),
            (&["update", "--release"], true),
            (&["gate", "cargo-check"], true),
            (&["remember", "project observation"], true),
            (&["memories"], false),
            (&["forget", "project-observation"], true),
            (&["note", "progress"], true),
            (&["done"], true),
            (&["handoff", "--to", "peer-session"], true),
        ];

        let command = Cli::command();
        let work = command
            .find_subcommand("work")
            .expect("work command exists");
        let agent_word_count = work
            .get_subcommands()
            .filter(|subcommand| subcommand.get_name() != "core")
            .count();
        assert_eq!(cases.len(), agent_word_count);

        for &(args, expected) in cases {
            let mut command = vec!["engram", "work"];
            command.extend_from_slice(args);
            let parsed = Cli::try_parse_from(command).expect("parse agent word");
            let Command::Work { operation, .. } = parsed.command else {
                panic!("agent word did not parse as work");
            };
            assert_eq!(
                operation.returns_mutation_receipt(),
                expected,
                "unexpected receipt classification for {args:?}"
            );
        }
    }

    #[test]
    fn work_cli_parses_cut_a_update_flags_and_gate() {
        let prerequisite = Cli::try_parse_from([
            "engram",
            "work",
            "--actor-id",
            "agent",
            "--session-id",
            "session",
            "update",
            "w-000000000001",
            "--after",
            "w-000000000002",
        ])
        .expect("parse prerequisite flag");
        assert!(matches!(
            prerequisite.command,
            Command::Work { operation, .. }
                if matches!(*operation, WorkCommand::Update(ref args) if args.after.is_some())
        ));

        let supersede = Cli::try_parse_from([
            "engram",
            "work",
            "--actor-id",
            "agent",
            "--session-id",
            "session",
            "update",
            "w-000000000001",
            "--supersede-with",
            "w-000000000002",
            "--reason",
            "duplicate",
        ])
        .expect("parse supersession flag");
        assert!(matches!(
            supersede.command,
            Command::Work { operation, .. }
                if matches!(*operation, WorkCommand::Update(ref args) if args.supersede_with.is_some() && args.reason.is_some())
        ));

        let gate = Cli::try_parse_from([
            "engram",
            "work",
            "--actor-id",
            "agent",
            "--session-id",
            "session",
            "gate",
            "--work-ref",
            "w-000000000001",
            "cargo-test",
            "--failed",
            "one::test",
            "--ref",
            "target/test.log",
        ])
        .expect("parse gate word");
        assert!(matches!(
            gate.command,
            Command::Work { operation, .. }
                if matches!(&*operation, WorkCommand::Gate { work_ref: Some(work_ref), failed, evidence_ref: Some(_), .. } if work_ref == "w-000000000001" && failed == &["one::test"])
        ));

        let failures_before_name = Cli::try_parse_from([
            "engram",
            "work",
            "--actor-id",
            "agent",
            "--session-id",
            "session",
            "gate",
            "--failed",
            "cargo fmt --check",
            "--failed",
            "doc-links",
            "quality-gates",
        ])
        .expect("repeatable failure labels do not consume the gate name");
        assert!(matches!(
            failures_before_name.command,
            Command::Work { operation, .. }
                if matches!(
                    &*operation,
                    WorkCommand::Gate { name, failed, .. }
                        if name == "quality-gates"
                            && failed == &["cargo fmt --check", "doc-links"]
                )
        ));
    }

    #[test]
    fn phoenix_note_cli_accepts_positional_target_and_describes_observations() {
        let parsed = Cli::try_parse_from([
            "engram",
            "work",
            "note",
            "w-000000000001",
            "review finding",
            "--ref",
            "review:detail",
        ])
        .unwrap();
        assert!(matches!(parsed.command, Command::Work { operation, .. }
            if matches!(&*operation, WorkCommand::Note { args, refs }
                if args == &["w-000000000001", "review finding"] && refs == &["review:detail"])));
        let help = Cli::try_parse_from(["engram", "work", "note", "--help"]).unwrap_err();
        assert_eq!(help.kind(), clap::error::ErrorKind::DisplayHelp);
        let text = help.to_string();
        assert!(text.contains("observation without a claim"));
        assert!(text.contains("[REF] TEXT"));
        assert!(!text.contains("--work-ref"));
    }

    #[test]
    fn work_cli_parses_required_child_waiver() {
        let waive = Cli::try_parse_from([
            "engram",
            "work",
            "--actor-id",
            "agent",
            "--session-id",
            "session",
            "update",
            "w-000000000001",
            "--waive",
            "w-000000000002",
            "--reason",
            "explicitly accepted omission",
        ])
        .expect("parse required-child waiver flag");
        assert!(matches!(
            waive.command,
            Command::Work { operation, .. }
                if matches!(*operation, WorkCommand::Update(ref args) if args.waive.is_some() && args.reason.is_some())
        ));
    }
}
