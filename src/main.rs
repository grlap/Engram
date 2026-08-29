//! Minimal Engram operator CLI for initializing and checking the local store.

use std::{
    env, fs,
    io::{self, BufReader, BufWriter},
    path::{Path, PathBuf},
    str::FromStr,
};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use engram::domain::{AssuranceLevel, SCHEMA_VERSION};
use engram::{
    ActorContext, ControlAssurance, ControlPolicy, DevelopmentNoopRedactor, HostControlServer,
    LocalWorkService, McpServer, ObjectHash, ProjectId, ProjectPolicyAuthorityDecision, SessionId,
    SqliteStore, WaiveWorkObligationRequest, WorkAuthorityGrant, WorkAuthorityOperation,
    WorkAuthorityScope, WorkAvailability, WorkCompleteInput, WorkHandoffInput, WorkLifecycle,
    WorkNextQuery, WorkNextSection, WorkObligationId, WorkPlanningBudget, WorkProposeInput,
    WorkUpdateInput, project_database_path,
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
    #[command(subcommand)]
    command: Command,
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
    Doctor,
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
    /// Use the six-operation local work protocol from a shell.
    Work {
        /// Actor identity asserted by the invoking host or operator wrapper.
        #[arg(long)]
        actor_id: String,
        /// Durable session identity used for ambient focus and cursors.
        #[arg(long)]
        session_id: String,
        /// Skill instruction that supplied this actor context, when available.
        #[arg(long)]
        source_skill: Option<String>,
        /// Host-selected immutable authority grant for mutations.
        #[arg(long)]
        authority_grant: Option<String>,
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
            "CONTROL WARNING: no current V1 host can bind at action_gated; recover with `engram control-policy set-required-assurance turn_gated --authorized-by <actor> --reason <reason>`"
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
        /// Optional compare-and-swap guard from `engram doctor`.
        #[arg(long)]
        expected_policy_hash: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
enum WorkCommand {
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
        /// JSON object or @path to a JSON file.
        #[arg(long)]
        input: String,
    },
    /// Apply a typed update to ambient work.
    Update {
        /// JSON object or @path to a JSON file.
        #[arg(long)]
        input: String,
    },
    /// Complete ambient work with evidence and acceptance results.
    Complete {
        /// JSON object or @path to a JSON file.
        #[arg(long)]
        input: String,
    },
    /// Offer, accept, or cancel an ambient claim handoff.
    Handoff {
        /// JSON object or @path to a JSON file.
        #[arg(long)]
        input: String,
    },
}

#[derive(Debug, Subcommand)]
enum AuthorityCommand {
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
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let (project_id, database) = resolve_project(&cli.project_file, cli.home)?;
    match cli.command {
        Command::Init {
            required_assurance,
            authorized_by,
            reason,
        } => initialize(
            &database,
            required_assurance.map(Into::into),
            authorized_by,
            reason,
        ),
        Command::Doctor => doctor(&database),
        Command::Mcp {
            actor_id,
            session_id,
            source_skill,
            work_authority_grant,
        } => {
            let grant = parse_optional_hash(work_authority_grant)?;
            serve_mcp(
                database,
                project_id,
                actor_id,
                session_id,
                source_skill,
                grant,
            )
            .await
        }
        Command::Control {
            actor_id,
            session_id,
            source_skill,
        } => serve_control(database, project_id, actor_id, session_id, source_skill),
        Command::Work {
            actor_id,
            session_id,
            source_skill,
            authority_grant,
            operation,
        } => run_work(
            database,
            project_id,
            actor_id,
            session_id,
            source_skill,
            parse_optional_hash(authority_grant)?,
            operation,
        ),
        Command::Authority { operation } => run_authority(&database, project_id, operation),
        Command::ControlPolicy { operation } => {
            run_control_policy(&database, project_id, operation)
        }
    }
}

fn run_control_policy(
    database: &Path,
    _project_id: ProjectId,
    operation: ControlPolicyCommand,
) -> Result<()> {
    let mut store = SqliteStore::open(database)
        .with_context(|| format!("failed to open {}", database.display()))?;
    let value = match operation {
        ControlPolicyCommand::SetRequiredAssurance {
            level,
            authorized_by,
            reason,
            expected_policy_hash,
        } => {
            let level = ControlAssurance::from(level);
            let expected_policy = expected_policy_hash
                .map(|value| {
                    ObjectHash::from_str(&value).map_err(|message| {
                        anyhow::anyhow!("invalid expected policy hash: {message}")
                    })
                })
                .transpose()?;
            let receipt = store.set_required_control_assurance(
                level,
                &ActorContext {
                    actor_id: authorized_by,
                    actor_kind: "host_operator".into(),
                    assurance: AssuranceLevel::Asserted,
                    run_id: None,
                    session_id: None,
                    source_tool: Some("cli:control_policy".into()),
                    source_skill: None,
                    provenance_chain: Vec::new(),
                    reason: "authorize a project-scoped behavioral-control policy change".into(),
                },
                &reason,
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
    };
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}

#[allow(
    clippy::too_many_lines,
    reason = "authority administration keeps grant and irreversible revocation behavior together"
)]
fn run_authority(
    database: &Path,
    project_id: ProjectId,
    operation: AuthorityCommand,
) -> Result<()> {
    let mut store = SqliteStore::open(database)
        .with_context(|| format!("failed to open {}", database.display()))?;
    let now = chrono::Utc::now();
    let value = match operation {
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

fn run_work(
    database: PathBuf,
    project_id: ProjectId,
    actor_id: String,
    session_id: String,
    source_skill: Option<String>,
    authority_grant: Option<ObjectHash>,
    operation: WorkCommand,
) -> Result<()> {
    let service = LocalWorkService::new(
        database,
        project_id,
        actor_id,
        SessionId(session_id),
        source_skill,
        authority_grant,
    );
    let now = chrono::Utc::now();
    let value = match operation {
        WorkCommand::Next {
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
        WorkCommand::Focus { work_ref } => {
            serde_json::to_value(service.work_focus(&work_ref, now)?)?
        }
        WorkCommand::Propose { input } => serde_json::to_value(
            service.work_propose(parse_json_input::<WorkProposeInput>(&input)?, now)?,
        )?,
        WorkCommand::Update { input } => serde_json::to_value(
            service.work_update(parse_json_input::<WorkUpdateInput>(&input)?, now)?,
        )?,
        WorkCommand::Complete { input } => serde_json::to_value(
            service.work_complete(parse_json_input::<WorkCompleteInput>(&input)?, now)?,
        )?,
        WorkCommand::Handoff { input } => serde_json::to_value(
            service.work_handoff(parse_json_input::<WorkHandoffInput>(&input)?, now)?,
        )?,
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

fn serve_control(
    database: PathBuf,
    project_id: ProjectId,
    actor_id: String,
    session_id: String,
    source_skill: Option<String>,
) -> Result<()> {
    let mut server = HostControlServer::open(
        database,
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

fn resolve_project(project_file: &Path, home: Option<PathBuf>) -> Result<(ProjectId, PathBuf)> {
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
    Ok((project_id, database))
}

fn initialize(
    database: &Path,
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
        (None, None, None) => SqliteStore::open(database),
        (Some(required_assurance), Some(authorized_by), Some(reason)) => {
            SqliteStore::open_with_initial_control_assurance(
                database,
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

fn doctor(database: &Path) -> Result<()> {
    let store = SqliteStore::open(database)
        .with_context(|| format!("failed to open {}", database.display()))?;
    let report = store.verify_all()?;
    if !report.is_healthy() {
        bail!(
            "integrity check failed for {} object(s), {} control record(s), and {} work record(s)",
            report.invalid_objects.len(),
            report.invalid_control_records.len(),
            report.invalid_work_records.len()
        );
    }
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
    let control = store.control_diagnostics()?;
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
    eprintln!("WARNING: development no-op redactor is active; no secret or PII protection");
    Ok(())
}

async fn serve_mcp(
    database: PathBuf,
    project_id: ProjectId,
    actor_id: String,
    session_id: String,
    source_skill: Option<String>,
    work_authority_grant: Option<ObjectHash>,
) -> Result<()> {
    let server = McpServer::new(
        database,
        project_id,
        actor_id,
        SessionId(session_id),
        source_skill,
    )
    .with_work_authority_grant(work_authority_grant)
    .serve(stdio())
    .await
    .context("failed to start Engram MCP stdio server")?;
    server
        .waiting()
        .await
        .context("Engram MCP stdio server stopped with an error")?;
    Ok(())
}
