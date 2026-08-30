//! Engram CLI: host/operator administration plus the eight-word agent surface.

use std::{
    env, fs,
    io::{self, BufReader, BufWriter, Read},
    path::{Path, PathBuf},
    process::ExitCode,
    str::FromStr,
};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use engram::domain::{AssuranceLevel, SCHEMA_VERSION};
use engram::{
    ActorContext, AddInput, AgentVerbs, BuiltinObligationRuleRef, BuiltinObligationTrigger,
    ClaimInput, ControlAssurance, ControlPolicy, DevelopmentNoopRedactor, DoneInput, HandoffAction,
    HandoffInput, HostControlServer, HostPathPolicy, LocalWorkService, LsInput, McpServer,
    NextInput, NoteInput, ObjectHash, ObligationRuleDefinition, ObligationRuleSet, ProjectId,
    ProjectPolicyAuthorityDecision, SessionId, SqliteStore, UpdateAction, UpdateInput,
    VerificationKind, VerificationRequirement, WaiveWorkObligationRequest, WorkAuthorityGrant,
    WorkAuthorityGrantStatus, WorkAuthorityOperation, WorkAuthorityScope, WorkAvailability,
    WorkCompleteInput, WorkHandoffInput, WorkItemKind, WorkLifecycle, WorkNextQuery,
    WorkNextSection, WorkObligationId, WorkPlanningBudget, WorkProposeInput, WorkUpdateInput,
    describe_host_path_policy, looks_like_work_ref, parse_defer_date, parse_host_path_policy,
    probe_host_path_policy, project_database_path,
};
use rmcp::{ServiceExt, transport::stdio};

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
    /// Create or migrate the local Engram database.
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
    /// Serve the coding-agent memory tools over MCP stdio.
    Mcp {
        /// Actor identity asserted by the host integration.
        #[arg(long)]
        actor_id: String,
        /// Durable runtime session identity used for task binding and privacy.
        #[arg(long)]
        session_id: String,
        /// Skill instruction that supplied this actor context, when available.
        #[arg(long)]
        source_skill: Option<String>,
        /// Host-selected immutable authority grant for work mutations.
        ///
        /// Prefer the environment source when argv may be observable to peer
        /// processes. An explicit flag takes precedence over the environment.
        #[arg(long, env = "ENGRAM_WORK_AUTHORITY_GRANT", hide_env_values = true)]
        work_authority_grant: Option<String>,
        /// Also register the legacy `task_*`, `memory_*`, `context_explain`,
        /// and `work_*` tools beside the eight agent words.
        #[arg(long)]
        legacy_tools: bool,
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
    /// Track work with eight words: next, ls, show, add, claim, update, note, done.
    ///
    /// The host fixes actor, session, and authority through the environment
    /// so an agent types only the word and its arguments.
    Work {
        /// Actor identity asserted by the invoking host or operator wrapper.
        #[arg(long, env = "ENGRAM_ACTOR_ID")]
        actor_id: String,
        /// Durable session identity used for ambient focus and cursors.
        #[arg(long, env = "ENGRAM_SESSION_ID")]
        session_id: String,
        /// Skill instruction that supplied this actor context, when available.
        #[arg(long, env = "ENGRAM_SOURCE_SKILL")]
        source_skill: Option<String>,
        /// Host-selected immutable authority grant for mutations.
        ///
        /// Prefer the environment source when argv may be observable to peer
        /// processes. An explicit flag takes precedence over the environment.
        #[arg(long, env = "ENGRAM_WORK_AUTHORITY_GRANT", hide_env_values = true)]
        authority_grant: Option<String>,
        /// Print the exact structured receipt instead of text.
        #[arg(long, global = true)]
        json: bool,
        #[command(subcommand)]
        operation: WorkCommand,
    },
    /// Host-operator work-authority administration; never exposed through MCP.
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
enum WorkCommand {
    /// What is ready, what you hold, and what changed since your last call.
    Next {
        #[arg(long, default_value_t = 20)]
        limit: u32,
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
    },
    /// One item: outcome, acceptance, holder, blockers, reminders.
    Show {
        /// Short work ref or full UUID; later words default to it.
        work_ref: String,
    },
    /// Create work from a title; outcome and acceptance criteria are welcome.
    Add {
        title: String,
        /// Defaults to the title.
        #[arg(long)]
        outcome: Option<String>,
        /// Acceptance criterion `done` is checked against; repeatable.
        /// Defaults to one criterion "<title> is done".
        #[arg(long = "accept", value_name = "CRITERION")]
        acceptance: Vec<String>,
        /// Add as a required child of this item instead of a root.
        #[arg(long, value_name = "REF")]
        under: Option<String>,
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
        /// Attributed reason for recovering a lapsed prior claim.
        #[arg(long, value_name = "REASON")]
        recover: Option<String>,
    },
    /// Exactly one action: --release, --blocked, --unblock, --cancel, or field changes.
    Update {
        /// Item to act on; defaults to the focus.
        work_ref: Option<String>,
        /// Release your claim.
        #[arg(long)]
        release: bool,
        /// Reason recorded with --release.
        #[arg(long, requires = "release")]
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
        /// Cancel the item and say why.
        #[arg(long, value_name = "REASON")]
        cancel: Option<String>,
    },
    /// Record one finding, decision, or evidence pointer on the item you hold.
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
        operation: CoreWorkCommand,
    },
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
        /// Comma-separated response sections: focus,ready,catalog,changes.
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
    /// Show the public installation and revocation status of one grant hash.
    Show {
        /// Immutable grant hash to query.
        grant: String,
        /// Print the structured status object instead of line-oriented text.
        #[arg(long)]
        json: bool,
    },
    /// Install a bounded standard local-agent grant and print its immutable hash.
    Grant {
        /// Exact actor id that may consume this grant.
        #[arg(long)]
        subject_actor_id: String,
        /// Host/operator actor id attributed as the grant issuer.
        #[arg(long)]
        issued_by: String,
        /// Work policy reference roots and children must match.
        #[arg(long, default_value = "project-default")]
        policy_ref: String,
        /// Grant lifetime in seconds (1..86400).
        #[arg(long, default_value_t = 3_600)]
        valid_seconds: i64,
        /// Maximum decomposition depth.
        #[arg(long, default_value_t = 4)]
        max_depth: u32,
        /// Maximum open descendants beneath one root.
        #[arg(long, default_value_t = 256)]
        max_open_descendants: u32,
        /// Maximum children admitted in one decomposition.
        #[arg(long, default_value_t = 16)]
        max_children_per_decomposition: u32,
        /// Attributed reason for this host-local delegation.
        #[arg(long, default_value = "operator enabled bounded local work execution")]
        reason: String,
        /// Admit reopening completed work (operator-sensitive).
        #[arg(long)]
        allow_reopen: bool,
        /// Admit recovery that waives an unaccounted prior claimant.
        #[arg(long)]
        allow_claim_recovery: bool,
        /// Admit release/cancellation waivers for missing contributions.
        #[arg(long)]
        allow_completion_waiver: bool,
        /// Admit host/operator waiver of an exact open execution obligation.
        #[arg(long)]
        allow_obligation_waiver: bool,
    },
    /// Irreversibly revoke an installed grant and print the revocation hash.
    Revoke {
        /// Immutable grant hash to revoke.
        grant: String,
        /// Host/operator actor id attributed to the revocation.
        #[arg(long)]
        revoked_by: String,
        /// Attributed reason for revocation.
        #[arg(long)]
        reason: String,
    },
    /// Waive one exact open execution obligation under dedicated authority.
    WaiveObligation {
        #[arg(long)]
        obligation_id: String,
        #[arg(long)]
        expected_definition: String,
        #[arg(long)]
        authority_grant: String,
        #[arg(long)]
        waived_by: String,
        #[arg(long)]
        reason: String,
        #[arg(long)]
        idempotency_key: String,
    },
}

#[tokio::main]
async fn main() -> Result<ExitCode> {
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
        Command::Doctor { json } => doctor(&database, identity, &project_id, json)?,
        Command::Backup { out } => backup(&database, out)?,
        Command::Restore { from, replace } => restore(&database, &from, replace)?,
        Command::Mcp {
            actor_id,
            session_id,
            source_skill,
            work_authority_grant,
            legacy_tools,
        } => {
            let grant = parse_optional_hash(work_authority_grant)?;
            serve_mcp(
                McpServer::new(
                    database,
                    project_id,
                    actor_id,
                    SessionId(session_id),
                    source_skill,
                )
                .with_work_authority_grant(grant)
                .with_legacy_tools(legacy_tools),
            )
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
            source_skill,
            authority_grant,
            json,
            operation,
        } => {
            let context = WorkContext {
                database,
                project_id,
                actor_id,
                session_id: SessionId(session_id),
                source_skill,
                authority_grant: parse_optional_hash(authority_grant)?,
            };
            return match operation {
                WorkCommand::Core { operation } => {
                    run_core_work(context, operation).map(|()| ExitCode::SUCCESS)
                }
                operation => run_work(context, json, operation),
            };
        }
        Command::Authority { operation } => {
            run_authority(&database, identity, project_id, operation)?;
        }
        Command::ControlPolicy { operation } => {
            run_control_policy(&database, identity, project_id, operation)?;
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// Host-fixed identity and authority for one shell work invocation.
struct WorkContext {
    database: PathBuf,
    project_id: ProjectId,
    actor_id: String,
    session_id: SessionId,
    source_skill: Option<String>,
    authority_grant: Option<ObjectHash>,
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

#[allow(
    clippy::too_many_lines,
    reason = "authority administration keeps grant and irreversible revocation behavior together"
)]
fn run_authority(
    database: &Path,
    identity: Option<HostPathPolicy>,
    project_id: ProjectId,
    operation: AuthorityCommand,
) -> Result<()> {
    let mut store = SqliteStore::open_with_host_path_identity(database, identity)
        .with_context(|| format!("failed to open {}", database.display()))?;
    let now = chrono::Utc::now();
    let value = match operation {
        AuthorityCommand::Show { grant, json } => {
            let grant = ObjectHash::from_str(&grant)
                .map_err(|message| anyhow::anyhow!("invalid grant hash: {message}"))?;
            let status = store.work_authority_grant_status(&grant)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&status)?);
            } else {
                print_authority_grant_status(&status)?;
            }
            return Ok(());
        }
        AuthorityCommand::Grant {
            subject_actor_id,
            issued_by,
            policy_ref,
            valid_seconds,
            max_depth,
            max_open_descendants,
            max_children_per_decomposition,
            reason,
            allow_reopen,
            allow_claim_recovery,
            allow_completion_waiver,
            allow_obligation_waiver,
        } => {
            if !(1..=86_400).contains(&valid_seconds) {
                bail!("--valid-seconds must be from 1 through 86400");
            }
            eprintln!(
                "WARNING: authority issuer and subject identities are asserted host context, not authenticated identities"
            );
            let mut operations = vec![
                WorkAuthorityOperation::RootCreate,
                WorkAuthorityOperation::Plan,
                WorkAuthorityOperation::Claim,
                WorkAuthorityOperation::Dispose,
                WorkAuthorityOperation::RootComplete,
                WorkAuthorityOperation::CompletionDrain,
            ];
            if allow_reopen {
                operations.push(WorkAuthorityOperation::Reopen);
            }
            if allow_claim_recovery {
                operations.push(WorkAuthorityOperation::ClaimRecovery);
            }
            if allow_completion_waiver {
                operations.push(WorkAuthorityOperation::CompletionWaiver);
            }
            if allow_obligation_waiver {
                operations.push(WorkAuthorityOperation::ObligationWaiver);
            }
            let hash = store.install_work_authority_grant(
                WorkAuthorityGrant {
                    schema_version: SCHEMA_VERSION,
                    project_id,
                    policy_ref,
                    subject_actor_id,
                    issued_by: ActorContext {
                        actor_id: issued_by,
                        actor_kind: "host_operator".into(),
                        assurance: AssuranceLevel::Asserted,
                        run_id: None,
                        session_id: None,
                        source_tool: Some("cli:authority_grant".into()),
                        source_skill: None,
                        provenance_chain: Vec::new(),
                        reason: "issue a bounded host-local work authority grant".into(),
                    },
                    assurance: AssuranceLevel::Asserted,
                    operations,
                    scope: WorkAuthorityScope::Project,
                    planning_budget: Some(WorkPlanningBudget {
                        max_depth,
                        max_open_descendants,
                        max_children_per_decomposition,
                    }),
                    issued_at: now,
                    valid_until: now + chrono::Duration::seconds(valid_seconds),
                    reason,
                },
                &DevelopmentNoopRedactor,
            )?;
            serde_json::json!({ "grant": hash })
        }
        AuthorityCommand::Revoke {
            grant,
            revoked_by,
            reason,
        } => {
            eprintln!(
                "WARNING: authority revoker identity is asserted host context, not an authenticated identity"
            );
            let grant = ObjectHash::from_str(&grant)
                .map_err(|message| anyhow::anyhow!("invalid grant hash: {message}"))?;
            let revocation = store.revoke_work_authority_grant(
                &grant,
                &ActorContext {
                    actor_id: revoked_by,
                    actor_kind: "host_operator".into(),
                    assurance: AssuranceLevel::Asserted,
                    run_id: None,
                    session_id: None,
                    source_tool: Some("cli:authority_revoke".into()),
                    source_skill: None,
                    provenance_chain: Vec::new(),
                    reason: "revoke a host-local work authority grant".into(),
                },
                &reason,
                now,
                &DevelopmentNoopRedactor,
            )?;
            serde_json::json!({ "grant": grant, "revocation": revocation })
        }
        AuthorityCommand::WaiveObligation {
            obligation_id,
            expected_definition,
            authority_grant,
            waived_by,
            reason,
            idempotency_key,
        } => {
            eprintln!(
                "WARNING: obligation waiver identity is asserted host context, not an authenticated identity"
            );
            let obligation_id = WorkObligationId(
                uuid::Uuid::parse_str(&obligation_id).context("invalid work obligation id")?,
            );
            let expected_definition = ObjectHash::from_str(&expected_definition)
                .map_err(|message| anyhow::anyhow!("invalid definition hash: {message}"))?;
            let authority_grant = ObjectHash::from_str(&authority_grant)
                .map_err(|message| anyhow::anyhow!("invalid authority grant hash: {message}"))?;
            serde_json::to_value(store.waive_work_obligation(
                &WaiveWorkObligationRequest {
                    obligation_id,
                    expected_definition,
                    waived_by: waived_by.clone(),
                    reason,
                    authority: engram::LifecycleAuthorityDecision {
                        grant: authority_grant,
                    },
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

fn print_authority_grant_status(status: &WorkAuthorityGrantStatus) -> Result<()> {
    println!("installed: {}", status.installed);
    println!(
        "subject_actor_id: {}",
        status.subject_actor_id.as_deref().unwrap_or("null")
    );
    println!(
        "issued_by: {}",
        status.issued_by.as_deref().unwrap_or("null")
    );
    println!(
        "valid_from: {}",
        status
            .valid_from
            .as_ref()
            .map(chrono::DateTime::to_rfc3339)
            .as_deref()
            .unwrap_or("null")
    );
    println!(
        "valid_until: {}",
        status
            .valid_until
            .as_ref()
            .map(chrono::DateTime::to_rfc3339)
            .as_deref()
            .unwrap_or("null")
    );
    println!(
        "revoked_at: {}",
        status
            .revoked_at
            .as_ref()
            .map(chrono::DateTime::to_rfc3339)
            .as_deref()
            .unwrap_or("null")
    );
    println!("operations: {}", serde_json::to_string(&status.operations)?);
    println!("scope: {}", serde_json::to_string(&status.scope)?);
    Ok(())
}

#[allow(
    clippy::too_many_lines,
    reason = "each word's flag translation stays beside the others so the eight-word surface is reviewable in one place"
)]
fn run_work(context: WorkContext, json: bool, operation: WorkCommand) -> Result<ExitCode> {
    let verbs = AgentVerbs::new(
        context.database,
        context.project_id,
        context.actor_id,
        context.session_id,
        context.source_skill,
        context.authority_grant,
    );
    let now = chrono::Utc::now();
    let outcome = match operation {
        WorkCommand::Next { limit } => verbs.next(&NextInput { limit: Some(limit) }, now),
        WorkCommand::Ls {
            search,
            blocked,
            mine,
            all,
            label,
            limit,
        } => verbs.ls(
            &LsInput {
                search,
                blocked,
                mine,
                all,
                label,
                limit: Some(limit),
            },
            now,
        ),
        WorkCommand::Show { work_ref } => verbs.show(&work_ref, now),
        WorkCommand::Add {
            title,
            outcome,
            acceptance,
            under,
            priority,
            labels,
            assignee,
            kind,
        } => verbs.add(
            AddInput {
                title,
                outcome,
                acceptance,
                under,
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
        WorkCommand::Update {
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
            cancel,
        } => {
            let revise = assignee.is_some()
                || priority.is_some()
                || defer.is_some()
                || title.is_some()
                || outcome.is_some();
            let selected = usize::from(release)
                + usize::from(blocked.is_some())
                + usize::from(unblock)
                + usize::from(cancel.is_some())
                + usize::from(revise);
            if selected != 1 {
                bail!(
                    "update needs exactly one action: --release, --blocked WHY, --unblock, --cancel REASON, or field changes (--title, --outcome, --assignee, --priority, --defer)"
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
            } else {
                let defer = defer
                    .as_deref()
                    .map(parse_defer_date)
                    .transpose()
                    .map_err(|message| anyhow::anyhow!("invalid --defer: {message}"))?;
                UpdateAction::Revise {
                    title,
                    outcome,
                    assignee,
                    priority,
                    defer,
                }
            };
            verbs.update(UpdateInput { work_ref, action }, now)
        }
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
            if json {
                println!("{}", serde_json::to_string_pretty(&receipt.value)?);
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
                    serde_json::to_string_pretty(&serde_json::json!({
                        "error": {
                            "message": error.to_string(),
                            "reminders": guidance.reminders,
                            "next": guidance.next,
                        }
                    }))?
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

fn run_core_work(context: WorkContext, operation: CoreWorkCommand) -> Result<()> {
    let service = LocalWorkService::new(
        context.database,
        context.project_id,
        context.actor_id,
        context.session_id,
        context.source_skill,
        context.authority_grant,
    );
    let now = chrono::Utc::now();
    let value = match operation {
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
        } => serde_json::to_value(service.work_next_with_delivery_token(
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
            },
            now,
        )?)?,
        CoreWorkCommand::Focus { work_ref } => {
            serde_json::to_value(service.work_focus(&work_ref, now)?)?
        }
        CoreWorkCommand::Propose { work_ref, input } => {
            serde_json::to_value(service.work_propose_on(
                work_ref.as_deref(),
                parse_json_input::<WorkProposeInput>(&input)?,
                now,
            )?)?
        }
        CoreWorkCommand::Update { work_ref, input } => {
            serde_json::to_value(service.work_update_on(
                work_ref.as_deref(),
                parse_json_input::<WorkUpdateInput>(&input)?,
                now,
            )?)?
        }
        CoreWorkCommand::Complete { work_ref, input } => {
            serde_json::to_value(service.work_complete_on(
                work_ref.as_deref(),
                parse_json_input::<WorkCompleteInput>(&input)?,
                now,
            )?)?
        }
        CoreWorkCommand::Handoff { work_ref, input } => {
            serde_json::to_value(service.work_handoff_on(
                work_ref.as_deref(),
                parse_json_input::<WorkHandoffInput>(&input)?,
                now,
            )?)?
        }
    };
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}

fn parse_optional_hash(value: Option<String>) -> Result<Option<ObjectHash>> {
    let Some(value) = value else {
        return Ok(None);
    };
    let value = value.trim();
    if value.is_empty() {
        return Ok(None);
    }
    ObjectHash::from_str(value)
        .map(Some)
        .map_err(|message| anyhow::anyhow!("invalid work-authority grant: {message}"))
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

fn initialize(
    database: &Path,
    identity: Option<HostPathPolicy>,
    required_assurance: Option<ControlAssurance>,
    authorized_by: Option<String>,
    reason: Option<String>,
) -> Result<()> {
    if let Some(parent) = database.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let requested_attribution = authorized_by
        .as_deref()
        .zip(reason.as_deref())
        .map(|(actor_id, reason)| (actor_id.trim().to_owned(), reason.trim().to_owned()));
    let store = match (required_assurance, authorized_by, reason) {
        (None, None, None) => SqliteStore::open_with_host_path_identity(database, identity),
        (Some(required_assurance), Some(authorized_by), Some(reason)) => {
            SqliteStore::open_with_initial_control_assurance(
                database,
                identity,
                required_assurance,
                &ActorContext {
                    actor_id: authorized_by,
                    actor_kind: "host_operator".into(),
                    assurance: AssuranceLevel::Asserted,
                    run_id: None,
                    session_id: None,
                    source_tool: Some("cli:init".into()),
                    source_skill: None,
                    provenance_chain: Vec::new(),
                    reason: "authorize an explicit project bootstrap control policy".into(),
                },
                &reason,
                &DevelopmentNoopRedactor,
            )
        }
        _ => bail!("--required-assurance, --authorized-by, and --reason must be supplied together"),
    }
    .with_context(|| format!("failed to initialize {}", database.display()))?;
    let control = store.control_diagnostics()?;
    let bootstrap_attribution_recorded = if let Some((actor_id, reason)) = requested_attribution {
        let policy: ControlPolicy = store
            .get(&control.active_policy)?
            .context("active bootstrap policy object is missing")?;
        let authority: ProjectPolicyAuthorityDecision = store
            .get(&policy.authority)?
            .context("active bootstrap policy authority object is missing")?;
        authority.authorized_by.actor_id == actor_id && authority.reason == reason
    } else {
        false
    };
    if bootstrap_attribution_recorded {
        eprintln!(
            "WARNING: bootstrap policy administrator identity is asserted host context, not an authenticated identity"
        );
    }
    warn_if_action_gated(control.required_assurance);
    println!(
        "Initialized local Engram store at {} with policy {} (epoch {}, required {}, obligation rules {})",
        database.display(),
        control.active_policy,
        control.policy_epoch.0,
        control_assurance_name(control.required_assurance),
        control.obligation_rule_set,
    );
    Ok(())
}

fn backup(database: &Path, out: Option<PathBuf>) -> Result<()> {
    let store = SqliteStore::open_unresolved(database)
        .with_context(|| format!("failed to open {}", database.display()))?;
    let out = if let Some(out) = out {
        out
    } else {
        {
            // <home>/projects/<digest>/engram.db → <home>/backups/<digest>/engram-<utc>.db
            let project_dir = database
                .parent()
                .context("store path has no project directory")?;
            let digest = project_dir
                .file_name()
                .context("store path has no project digest")?;
            let home = project_dir
                .parent()
                .and_then(Path::parent)
                .context("store path is not below an Engram home")?;
            home.join("backups").join(digest).join(format!(
                "engram-{}.db",
                chrono::Utc::now().format("%Y%m%dT%H%M%SZ")
            ))
        }
    };
    let manifest = store
        .backup_to(&out)
        .with_context(|| format!("failed to back up {}", database.display()))?;
    println!("{}", serde_json::to_string_pretty(&manifest)?);
    eprintln!(
        "WARNING: a backup is a complete store, host grants and private scratch included; keep it where the store may be kept"
    );
    Ok(())
}

/// Restores a verified backup as the project store. The backup is verified
/// read-only before anything is touched, staged beside the store, verified
/// again, and only then installed. Replacing an existing store first
/// checkpoints and truncates its write-ahead log under the writer lock so no
/// stale log can be applied to the restored file; no other Engram process
/// may use the store while it is replaced.
fn restore(database: &Path, from: &Path, replace: bool) -> Result<()> {
    // A backup from an older schema is still usable: it is migrated on the
    // staged copy, never on the backup itself.
    let manifest = match SqliteStore::verify_backup(from) {
        Ok(manifest) => Some(manifest),
        Err(engram::storage::StoreError::BackupNeedsMigration { .. }) => None,
        Err(error) => {
            return Err(error).with_context(|| format!("backup {} is not usable", from.display()));
        }
    };
    if let (Ok(source), Ok(target)) = (fs::canonicalize(from), fs::canonicalize(database))
        && source == target
    {
        bail!("backup {} is the store itself", from.display());
    }
    let exists = database.exists();
    if exists && !replace {
        bail!(
            "store {} already exists; pass --replace to overwrite it with the backup",
            database.display()
        );
    }
    if let Some(parent) = database.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let staged = PathBuf::from(format!(
        "{}.restore-{}.tmp",
        database.display(),
        std::process::id()
    ));
    let _ = fs::remove_file(&staged);
    fs::copy(from, &staged)
        .with_context(|| format!("failed to stage {} beside the store", from.display()))?;
    let mut migrated_from = None;
    let install = (|| -> Result<()> {
        let (staged_manifest, from_version) = SqliteStore::prepare_restore_copy(&staged)
            .with_context(|| format!("staged copy {} failed verification", staged.display()))?;
        migrated_from = from_version;
        if let Some(manifest) = &manifest
            && staged_manifest.file_sha256 != manifest.file_sha256
        {
            bail!("staged copy bytes differ from the backup");
        }
        if exists {
            // Fold the old log into the old file and truncate it under an
            // exclusive lock; a stale log must never meet the restored file.
            // A checkpoint that could not complete means another process still
            // holds the store, and replacement stops there.
            let old = rusqlite::Connection::open(database).with_context(|| {
                format!("failed to open {} for replacement", database.display())
            })?;
            old.execute_batch("PRAGMA locking_mode = EXCLUSIVE; BEGIN IMMEDIATE; COMMIT;")
                .context(
                    "failed to take the store being replaced; is another Engram process using it?",
                )?;
            let (busy, log_frames, checkpointed): (i64, i64, i64) = old
                .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                })
                .context("failed to checkpoint the store being replaced")?;
            if busy != 0 || log_frames != checkpointed {
                bail!(
                    "another process still uses {}; stop it and retry (checkpoint busy={busy}, log={log_frames}, checkpointed={checkpointed})",
                    database.display()
                );
            }
            drop(old);
            for suffix in ["-wal", "-shm", "-journal"] {
                let sidecar = PathBuf::from(format!("{}{suffix}", database.display()));
                if sidecar.exists() {
                    fs::remove_file(&sidecar)
                        .with_context(|| format!("failed to remove {}", sidecar.display()))?;
                }
            }
            fs::rename(&staged, database)
                .with_context(|| format!("failed to install {} as the store", staged.display()))?;
        } else {
            // Nothing may be replaced without --replace, not even a store that
            // appeared while the copy was being staged.
            engram::install_store_copy_without_replacing(&staged, database)
                .with_context(|| format!("failed to install {} as the store", staged.display()))?;
        }
        Ok(())
    })();
    if let Err(error) = install {
        let _ = fs::remove_file(&staged);
        return Err(error);
    }
    let restored = SqliteStore::verify_backup(database)
        .with_context(|| format!("restored store {} failed verification", database.display()))?;
    if let Some(manifest) = &manifest
        && restored.file_sha256 != manifest.file_sha256
    {
        bail!("restored store bytes differ from the backup");
    }
    if let Some(from_version) = migrated_from {
        eprintln!(
            "NOTE: the backup was written under work schema {from_version}; the restored store was migrated to the current schema before installation"
        );
    }
    println!("{}", serde_json::to_string_pretty(&restored)?);
    Ok(())
}

fn doctor(
    database: &Path,
    identity: Option<HostPathPolicy>,
    project_id: &ProjectId,
    json: bool,
) -> Result<()> {
    let store = SqliteStore::open_with_host_path_identity(database, identity)
        .with_context(|| format!("failed to open {}", database.display()))?;
    let report = store.verify_all()?;
    if json {
        let json_report = build_doctor_json_report(&store, database, project_id, &report)?;
        println!("{}", serde_json::to_string_pretty(&json_report.value)?);
        match &json_report.control {
            Some(control) => emit_control_limitations(control),
            None => emit_unavailable_control_diagnostics_limitations(),
        }
        if let Some(failure) = json_report.failure {
            bail!("{failure}");
        }
        return Ok(());
    }
    if !report.is_healthy() {
        bail!(
            "integrity check failed for {} object(s), {} control record(s), and {} work record(s)",
            report.invalid_objects.len(),
            report.invalid_control_records.len(),
            report.invalid_work_records.len()
        );
    }
    let control = store.control_diagnostics_at(chrono::Utc::now())?;
    let stored_path_policy = store.stored_host_path_policy()?;
    println!(
        "Engram store is healthy ({} immutable object(s), {} control record(s), {} work record(s) checked)",
        report.checked_objects, report.checked_control_records, report.checked_work_records
    );
    if !report.legacy_work_records.is_empty() {
        println!(
            "Legacy work records retained without reinterpretation: {}",
            report.legacy_work_records.join(", ")
        );
    }
    println!(
        "Control policy schema={} id={} epoch={} required={} obligation_rules={} supported={:?}; sessions={} issued={} begun={}",
        control.control_schema_version,
        control.active_policy,
        control.policy_epoch.0,
        control_assurance_name(control.required_assurance),
        control.obligation_rule_set,
        control.supported_effects,
        control.active_sessions,
        control.issued_turns,
        control.begun_turns,
    );
    match (stored_path_policy, identity) {
        (Some(stored), Some(_)) => println!(
            "Host path policy: {} (persisted; this opener resolved the same)",
            describe_host_path_policy(stored)
        ),
        (Some(stored), None) => println!(
            "Host path policy: {} (persisted; this opener could not resolve the project root, so path leases would be refused)",
            describe_host_path_policy(stored)
        ),
        (None, Some(resolved)) => println!(
            "Host path policy: {} (resolved now, persisted by this open)",
            describe_host_path_policy(resolved)
        ),
        (None, None) => println!(
            "Host path policy: unresolved; path leases are refused until --host-path-policy is supplied"
        ),
    }
    emit_control_limitations(&control);
    Ok(())
}

struct DoctorJsonReport {
    value: serde_json::Value,
    control: Option<engram::storage::ControlDiagnostics>,
    failure: Option<String>,
}

fn build_doctor_json_report(
    store: &SqliteStore,
    database: &Path,
    project_id: &ProjectId,
    report: &engram::storage::IntegrityReport,
) -> Result<DoctorJsonReport> {
    let control = store.control_diagnostics_at(chrono::Utc::now());
    let stored_path_policy = store.stored_host_path_policy()?;
    let canonical_database = canonical_database_path(database)?;
    let (control, control_value, control_error) = match control {
        Ok(control) => {
            let value = serde_json::json!({
                "schema_version": control.control_schema_version,
                "policy": control.active_policy,
                "epoch": control.policy_epoch.0,
                "required_assurance": control.required_assurance,
                "obligation_rules": control.obligation_rule_set,
                "supported_effects": control.supported_effects,
                "sessions": control.active_sessions,
                "issued": control.issued_turns,
                "begun": control.begun_turns,
            });
            (Some(control), value, None)
        }
        Err(error) => (None, serde_json::Value::Null, Some(error.to_string())),
    };
    let mut value = serde_json::json!({
        "healthy": report.is_healthy(),
        "project_id": project_id,
        "database": canonical_database,
        "work_schema_version": store.work_schema_version(),
        "checked": {
            "objects": report.checked_objects,
            "control_records": report.checked_control_records,
            "work_records": report.checked_work_records,
        },
        "invalid": {
            "objects": report.invalid_objects,
            "control_records": report.invalid_control_records,
            "work_records": report.invalid_work_records,
        },
        "host_path_policy": stored_path_policy.map(describe_host_path_policy),
        "control": control_value,
    });
    if let Some(error) = &control_error {
        let serde_json::Value::Object(object) = &mut value else {
            bail!("doctor JSON report is not an object");
        };
        object.insert(
            "control_error".into(),
            serde_json::Value::String(error.clone()),
        );
    }
    let failure = if report.is_healthy() {
        control_error.map(|error| format!("control diagnostics failed: {error}"))
    } else {
        Some(format!(
            "integrity check failed for {} object(s), {} control record(s), and {} work record(s)",
            report.invalid_objects.len(),
            report.invalid_control_records.len(),
            report.invalid_work_records.len()
        ))
    };
    Ok(DoctorJsonReport {
        value,
        control,
        failure,
    })
}

fn canonical_database_path(database: &Path) -> Result<String> {
    let canonical = fs::canonicalize(database)
        .with_context(|| format!("failed to canonicalize {}", database.display()))?;
    Ok(path_without_windows_verbatim_prefix(&canonical))
}

#[cfg(windows)]
fn path_without_windows_verbatim_prefix(path: &Path) -> String {
    let path = path.to_string_lossy();
    if let Some(rest) = path.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = path.strip_prefix(r"\\?\") {
        rest.to_owned()
    } else {
        path.into_owned()
    }
}

#[cfg(not(windows))]
fn path_without_windows_verbatim_prefix(path: &Path) -> String {
    path.display().to_string()
}

fn emit_control_limitations(control: &engram::storage::ControlDiagnostics) {
    warn_if_action_gated(control.required_assurance);
    if !control.action_gating_available {
        eprintln!(
            "CONTROL LIMITATION: action gating is unavailable; unsupported effects fail closed: {:?}",
            control.unenforced_effects
        );
    }
    if !control.authority_mediation_available {
        eprintln!(
            "CONTROL LIMITATION: organizational authority mediation is not wired; the built-in policy admits only effects that require no external authority decision"
        );
    }
    if !control.action_outcome_tracking_available {
        eprintln!(
            "CONTROL LIMITATION: action-outcome reconciliation is not wired; effects requiring action gating remain unsupported"
        );
    }
    emit_redactor_limitation();
}

fn emit_unavailable_control_diagnostics_limitations() {
    eprintln!(
        "CONTROL LIMITATION: control diagnostics are unavailable; this integrity report provides no enforcement assurance"
    );
    emit_redactor_limitation();
}

fn emit_redactor_limitation() {
    eprintln!("WARNING: development no-op redactor is active; no secret or PII protection");
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unhealthy_doctor_json_survives_a_corrupt_policy_projection() {
        let directory = tempfile::tempdir().expect("temporary doctor store");
        let database = directory.path().join("engram.db");
        let store = SqliteStore::open(&database).expect("healthy store");
        let corruptor = rusqlite::Connection::open(&database).expect("corruption connection");
        corruptor
            .execute(
                "UPDATE control_policy_versions SET policy_json = X'7B7D'
                 WHERE policy_hash = (
                     SELECT policy_hash FROM control_policy_state WHERE singleton = 1
                 )",
                [],
            )
            .expect("corrupt active policy projection after open");
        drop(corruptor);

        let report = store.verify_all().expect("integrity report");
        assert!(!report.is_healthy());
        let json_report = build_doctor_json_report(
            &store,
            &database,
            &ProjectId("doctor-corrupt-policy".into()),
            &report,
        )
        .expect("best-effort doctor JSON");

        assert_eq!(json_report.value["healthy"], false);
        assert_eq!(json_report.value["control"], serde_json::Value::Null);
        assert!(json_report.value["control_error"].is_string());
        assert!(
            json_report.value["invalid"]["control_records"]
                .as_array()
                .is_some_and(|records| records.iter().any(|record| {
                    record
                        .as_str()
                        .is_some_and(|record| record.starts_with("control_policy_"))
                }))
        );
        assert!(json_report.control.is_none());
        assert!(json_report.failure.is_some());
        serde_json::to_string(&json_report.value).expect("printable doctor JSON");
    }
}
