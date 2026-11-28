//! Durable, isolated ACP runtime verification.
//!
//! This module deliberately sits beside the ACP process implementation. It
//! can therefore reuse the same spawn and JSON-RPC lifecycle as a live
//! session while keeping all probe-owned state under a disposable root.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::time::{MissedTickBehavior, interval, timeout};
use url::Url;
use vibex_config_switch::{
    AgentProviderProjectionEngine, ProviderConfigService, secrets::resolve_provider_secret,
};
use vibex_core::{
    AgentProviderProjectionPreview, AgentProviderProjectionRegistry, AgentRuntimeProbeCapability,
    AgentRuntimeProbeFact, AgentRuntimeProbeFactStatus, AgentRuntimeProbeId,
    AgentRuntimeProbeListRequest, AgentRuntimeProbeRecord, AgentRuntimeProbeRequest,
    AgentRuntimeProbeStage, AgentRuntimeProbeStartRequest, AgentRuntimeProbeStatus,
    AgentRuntimeProfile, ProjectionDescriptorMatch, ProviderSwitchBehavior, VibexError,
    VibexResult, unix_timestamp_ms,
};
use vibex_db::{
    AgentModelProviderBindingRepository, AgentRuntimeProbeRepository,
    AgentRuntimeProfileRepository, apply_migrations, open_database,
};

use crate::AcpProcessInstanceId;
use crate::protocol::{
    self, AcpOperation, build_initialize_params, build_session_load_params,
    build_session_new_params, build_session_resume_params,
};
use crate::registry::{ModelCatalogSemantics, ModelSelectionStrategy};
use crate::runtime::{
    ACP_PROBE_TIMEOUT, AcpProcess, AcpProcessLaunch, AcpProcessPurpose, AcpRuntimeClient,
    append_projection_process_args, apply_startup_model_to_config, effective_acp_process_args,
    extract_current_model_id, extract_model_ids, validate_restore_response,
};

const PROBE_SOURCE_REVISION: &str = "runtime-probe-v5";
const MIN_DIAGNOSTIC_LEN: usize = 1;
const PROBE_ENV_KEYS: &[&str] = &[
    "HOME",
    "USERPROFILE",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_STATE_HOME",
    "XDG_CACHE_HOME",
    "CODEX_HOME",
];

/// Management-facing coordinator. Request/list/cancel operations are
/// synchronous durable DB operations; `spawn` starts the bounded async worker.
#[derive(Clone)]
pub struct AgentRuntimeProbeService {
    client: Arc<AcpRuntimeClient>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AgentRuntimeProbeReconcileReport {
    pub recovered: usize,
    pub cancelled: usize,
}

impl AgentRuntimeProbeService {
    pub fn new(client: Arc<AcpRuntimeClient>) -> Self {
        Self { client }
    }

    pub fn request(
        &self,
        request: AgentRuntimeProbeStartRequest,
    ) -> VibexResult<AgentRuntimeProbeRecord> {
        request.validate()?;
        let record = self.client.create_probe_record(request)?;
        Ok(record)
    }

    /// Start a worker for a previously persisted request. A separate method
    /// keeps request durability independent from executor availability.
    pub fn spawn(&self, probe_id: AgentRuntimeProbeId) -> VibexResult<()> {
        let client = self.client.clone();
        tokio::runtime::Handle::try_current()
            .map_err(|_| {
                VibexError::process(
                    "agent_runtime_probe_executor_unavailable",
                    "runtime probe requires an active async executor",
                )
            })?
            .spawn(async move {
                if let Err(error) = client.execute_probe(probe_id).await {
                    tracing::warn!(
                        target: "vibex_agent_runtime_probe",
                        error_code = %error.code,
                        "runtime probe worker failed"
                    );
                }
            });
        Ok(())
    }

    pub async fn run(&self, probe_id: AgentRuntimeProbeId) -> VibexResult<AgentRuntimeProbeRecord> {
        self.client.execute_probe(probe_id).await
    }

    pub fn reconcile_on_startup(&self) -> VibexResult<AgentRuntimeProbeReconcileReport> {
        let conn = self.client.open_probe_connection()?;
        let pending = AgentRuntimeProbeRepository::list_non_terminal(&conn)?;
        let mut report = AgentRuntimeProbeReconcileReport::default();
        for record in pending {
            self.client.cleanup_probe_root(&record.id)?;
            let recovered = AgentRuntimeProbeRepository::reset_for_startup(
                &conn,
                &record.id,
                record.revision,
                unix_timestamp_ms(),
            )?;
            if recovered.status == AgentRuntimeProbeStatus::Requested {
                self.spawn(recovered.id)?;
                report.recovered = report.recovered.saturating_add(1);
            } else if recovered.status == AgentRuntimeProbeStatus::Cancelled {
                report.cancelled = report.cancelled.saturating_add(1);
            }
        }
        Ok(report)
    }

    pub fn get(
        &self,
        probe_id: &AgentRuntimeProbeId,
    ) -> VibexResult<Option<AgentRuntimeProbeRecord>> {
        let conn = self.client.open_probe_connection()?;
        AgentRuntimeProbeRepository::get(&conn, probe_id)
    }

    pub fn list(
        &self,
        request: AgentRuntimeProbeListRequest,
    ) -> VibexResult<Vec<AgentRuntimeProbeRecord>> {
        let conn = self.client.open_probe_connection()?;
        AgentRuntimeProbeRepository::list(
            &conn,
            request.runtime_profile_id.as_ref(),
            request.limit.unwrap_or(100),
        )
    }

    pub fn cancel(
        &self,
        probe_id: &AgentRuntimeProbeId,
        expected_revision: i64,
    ) -> VibexResult<AgentRuntimeProbeRecord> {
        let conn = self.client.open_probe_connection()?;
        AgentRuntimeProbeRepository::request_cancel(
            &conn,
            probe_id,
            expected_revision,
            unix_timestamp_ms(),
        )
    }
}

#[derive(Debug)]
struct ProbeExecutionResult {
    facts: Vec<AgentRuntimeProbeFact>,
    agent_version: Option<String>,
    adapter_version: Option<String>,
    descriptor_match: ProjectionDescriptorMatch,
    projection_fingerprint: Option<String>,
    switch_behavior: ProviderSwitchBehavior,
    source_survived_prepare_failure: bool,
}

struct ProbeProjectionContext {
    provider_profile_id: Option<vibex_core::ProviderProfileId>,
    descriptor_match: ProjectionDescriptorMatch,
    projection: Option<vibex_config_switch::ResolvedAgentProviderProjection>,
    expected_provider_identities: Vec<String>,
}

struct RunningProbeContext<'a> {
    runtime: &'a AgentRuntimeProfile,
    profile_id: &'a vibex_core::ProviderProfileId,
    descriptor_match: ProjectionDescriptorMatch,
    projection: Option<&'a vibex_config_switch::ResolvedAgentProviderProjection>,
    expected_provider_identities: &'a [String],
    cwd: &'a Path,
    process: Arc<AcpProcess>,
    deadline: Instant,
}

struct ProbeRootGuard {
    path: PathBuf,
}

impl ProbeRootGuard {
    fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

impl Drop for ProbeRootGuard {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_dir_all(&self.path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(
                target: "vibex_agent_runtime_probe",
                error_kind = ?error.kind(),
                "failed to remove isolated runtime probe root"
            );
        }
    }
}

impl AcpRuntimeClient {
    pub fn runtime_probe_service(self: &Arc<Self>) -> AgentRuntimeProbeService {
        AgentRuntimeProbeService::new(self.clone())
    }

    fn open_probe_connection(&self) -> VibexResult<vibex_db::DbConnection> {
        let mut conn = open_database(self.provider_config_service().database_path())?;
        apply_migrations(&mut conn)?;
        Ok(conn)
    }

    fn create_probe_record(
        &self,
        request: AgentRuntimeProbeStartRequest,
    ) -> VibexResult<AgentRuntimeProbeRecord> {
        let conn = self.open_probe_connection()?;
        let runtime = AgentRuntimeProfileRepository::get(&conn, &request.runtime_profile_id)?
            .ok_or_else(|| {
                VibexError::validation(
                    "agent_runtime_profile_not_found",
                    "Agent runtime profile was not found for the probe",
                )
            })?;
        let resolution =
            AgentProviderProjectionRegistry::builtin()?.resolve(&runtime.version_identity)?;
        let binding = request
            .binding_id
            .as_ref()
            .map(|id| AgentModelProviderBindingRepository::get(&conn, id))
            .transpose()?
            .flatten();
        if let Some(binding) = &binding
            && binding.runtime_profile_id != runtime.id
        {
            return Err(VibexError::validation(
                "agent_runtime_probe_binding_mismatch",
                "probe binding does not belong to the selected runtime profile",
            ));
        }
        let record = AgentRuntimeProbeRecord::requested(
            AgentRuntimeProbeId::new(),
            AgentRuntimeProbeRequest {
                runtime_profile_id: request.runtime_profile_id,
                binding_id: request.binding_id,
                workspace_key: request.workspace_key,
                timeout_ms: request.timeout_ms,
                minimal_prompt: request.minimal_prompt,
            },
            runtime.version_identity.route.agent_id.clone(),
            runtime.version_identity.route.adapter_id.clone(),
            resolution.descriptor.id,
            resolution.descriptor.descriptor_version,
            unix_timestamp_ms(),
        )?;
        AgentRuntimeProbeRepository::insert(&conn, &record)?;
        Ok(record)
    }

    async fn execute_probe(
        &self,
        probe_id: AgentRuntimeProbeId,
    ) -> VibexResult<AgentRuntimeProbeRecord> {
        let mut record = {
            let conn = self.open_probe_connection()?;
            let record = AgentRuntimeProbeRepository::claim_requested(
                &conn,
                &probe_id,
                unix_timestamp_ms(),
            )?
            .ok_or_else(|| {
                VibexError::validation(
                    "agent_runtime_probe_not_found",
                    "Agent runtime probe was not found",
                )
            })?;
            if record.status != AgentRuntimeProbeStatus::Running {
                return Ok(record);
            }
            record
        };

        let outcome = self.execute_probe_stages(&mut record).await;
        let conn = self.open_probe_connection()?;
        let mut latest = AgentRuntimeProbeRepository::get(&conn, &probe_id)?.ok_or_else(|| {
            VibexError::validation(
                "agent_runtime_probe_not_found",
                "Agent runtime probe disappeared before completion",
            )
        })?;
        if latest.is_terminal() {
            return Ok(latest);
        }

        match outcome {
            Ok(result) => {
                let evidence = self.build_evidence(&record, result);
                latest.status = aggregate_probe_status(&evidence);
                latest.diagnostic_code = aggregate_probe_diagnostic(&evidence, latest.status);
                latest.facts = evidence.facts.clone();
                latest.evidence = Some(evidence);
            }
            Err(error) if error.code == "agent_runtime_probe_cancelled" => {
                latest.status = AgentRuntimeProbeStatus::Cancelled;
                latest.diagnostic_code = Some("probe_cancelled".to_string());
                latest.facts = record.facts;
            }
            Err(error) if error.code == "agent_runtime_probe_timeout" => {
                latest.status = AgentRuntimeProbeStatus::TimedOut;
                latest.diagnostic_code = Some("probe_timeout".to_string());
                latest.facts = record.facts;
            }
            Err(error) => {
                latest.status = if is_blocking_probe_error(&error.code) {
                    AgentRuntimeProbeStatus::Blocked
                } else {
                    AgentRuntimeProbeStatus::Failed
                };
                latest.diagnostic_code = Some(safe_code(&error.code));
                latest.facts = record.facts;
            }
        }
        latest.stage = AgentRuntimeProbeStage::Completed;
        let now_ms = unix_timestamp_ms();
        latest.finished_at_ms = Some(now_ms);
        latest.updated_at_ms = now_ms.max(latest.updated_at_ms);
        let expected_revision = latest.revision;
        latest.revision = latest.revision.saturating_add(1).max(1);
        AgentRuntimeProbeRepository::update(&conn, &latest, expected_revision)
    }

    async fn execute_probe_stages(
        &self,
        record: &mut AgentRuntimeProbeRecord,
    ) -> VibexResult<ProbeExecutionResult> {
        let deadline = Instant::now() + Duration::from_millis(record.request.timeout_ms);
        self.transition_probe_stage(record, AgentRuntimeProbeStage::ResolvingIdentity)?;
        let conn = self.open_probe_connection()?;
        let runtime =
            AgentRuntimeProfileRepository::get(&conn, &record.request.runtime_profile_id)?
                .ok_or_else(|| {
                    VibexError::validation(
                        "agent_runtime_probe_prerequisite_missing",
                        "runtime profile disappeared before probe execution",
                    )
                })?;
        let resolution =
            AgentProviderProjectionRegistry::builtin()?.resolve(&runtime.version_identity)?;
        if resolution.descriptor.id != record.descriptor_id
            || resolution.descriptor.descriptor_version != record.descriptor_version
        {
            return Err(VibexError::conflict(
                "agent_runtime_probe_descriptor_stale",
                "runtime identity no longer matches the descriptor reserved for this probe",
            ));
        }
        self.ensure_probe_not_cancelled(record)?;

        let root = self.probe_root(&record.id)?;
        let _root_guard = ProbeRootGuard::new(root.clone());
        self.transition_probe_stage(record, AgentRuntimeProbeStage::PlanningProjection)?;
        if Instant::now() >= deadline {
            return Err(VibexError::process(
                "agent_runtime_probe_timeout",
                "runtime probe exceeded its deadline while planning projection",
            ));
        }
        let projection_context =
            self.plan_probe_projection(&conn, record, &runtime, resolution.match_kind, &root)?;
        if Instant::now() >= deadline {
            return Err(VibexError::process(
                "agent_runtime_probe_timeout",
                "runtime probe exceeded its deadline while planning projection",
            ));
        }
        drop(conn);
        self.ensure_probe_not_cancelled(record)?;

        self.transition_probe_stage(record, AgentRuntimeProbeStage::StartingProcess)?;
        let workspace = root.join("workspace");
        let home = root.join("home");
        let config_home = root.join("config");
        let data_home = root.join("data");
        let state_home = root.join("state");
        let cache_home = root.join("cache");
        for path in [
            &workspace,
            &home,
            &config_home,
            &data_home,
            &state_home,
            &cache_home,
        ] {
            fs::create_dir_all(path).map_err(|error| {
                VibexError::storage(
                    "agent_runtime_probe_root_create_failed",
                    "failed to create the isolated runtime probe root",
                )
                .with_diagnostic("errorKind", format!("{:?}", error.kind()))
            })?;
        }

        let profile_id = projection_context.provider_profile_id.ok_or_else(|| {
            VibexError::validation(
                "agent_runtime_probe_prerequisite_missing",
                "no ACP provider profile is available for this runtime probe",
            )
        })?;
        let profile = self
            .provider_config_service()
            .get_profile(&profile_id)?
            .ok_or_else(|| {
                VibexError::validation(
                    "agent_runtime_probe_prerequisite_missing",
                    "ACP provider profile is unavailable for this runtime probe",
                )
            })?;
        let mut config = self.profile_config(&profile_id)?;
        let target_model = projection_context
            .projection
            .as_ref()
            .and_then(|projection| projection.effective_model.as_deref());
        apply_startup_model_to_config(&mut config, profile.agent_id.as_str(), target_model);
        let process_args = materialized_probe_process_args(
            profile.agent_id.as_str(),
            &config,
            projection_context.projection.as_ref(),
        );
        let mut env = self.resolve_probe_config_env(&profile, &config)?;
        if let Some(projection) = &projection_context.projection {
            env.extend(projection.child_environment());
        }
        // This probe materializes its own launch environment, which bypasses
        // the runtime's Codex startup model override, so apply it here too.
        if projection_context.projection.is_some()
            && let Some(model) = crate::runtime::codex_launch_model(&profile.agent_id, &config)
        {
            crate::codex::upsert_codex_model_override(&mut env, model);
        }
        let codex_home = probe_codex_home(
            profile.agent_id.as_str(),
            &root,
            &home,
            projection_context
                .projection
                .as_ref()
                .map(|projection| projection.overlay_root.as_path()),
        )?;
        add_isolation_environment(
            &mut env,
            &home,
            &config_home,
            &data_home,
            &state_home,
            &cache_home,
            &codex_home,
        );
        let cwd = workspace;
        let (strategy, fallback) = self.pool_decision(&profile_id, &config)?;
        let runtime_resources = vibex_agent::ProviderRuntimeResources::default();
        let auth_source = vibex_core::RuntimeAuthSource::provider_profile(profile_id.clone());
        let env_unsets = Vec::new();
        let process = tokio::select! {
            result = timeout(
                deadline.saturating_duration_since(Instant::now()),
                self.spawn_process(
                    AcpProcessInstanceId::new(),
                    AcpProcessLaunch {
                        auth_source: &auth_source,
                        auth_source_revision: profile.launch_revision(),
                        agent_id: &profile.agent_id,
                        config: &config,
                        cwd: &cwd,
                        runtime_resources: &runtime_resources,
                        env_unsets: &env_unsets,
                        purpose: AcpProcessPurpose::Probe,
                        process_strategy_effective: strategy,
                        pool_fallback_reason: fallback,
                        session_id: None,
                    },
                    Some(process_args),
                    Some(env),
                    None,
                ),
            ) => match result {
                Ok(result) => result?,
                Err(_) => return Err(VibexError::process(
                    "agent_runtime_probe_timeout",
                    "runtime probe exceeded its deadline while starting process",
                )),
            },
            cancellation = self.wait_for_probe_cancel(&record.id) => {
                cancellation?;
                return Err(VibexError::conflict(
                    "agent_runtime_probe_cancelled",
                    "runtime probe cancellation was requested",
                ));
            }
        };

        let probe_result = self
            .run_process_probe(
                record,
                RunningProbeContext {
                    runtime: &runtime,
                    profile_id: &profile_id,
                    descriptor_match: projection_context.descriptor_match,
                    projection: projection_context.projection.as_ref(),
                    expected_provider_identities: &projection_context.expected_provider_identities,
                    cwd: &cwd,
                    process: process.clone(),
                    deadline,
                },
            )
            .await;
        process.shutdown().await;
        probe_result
    }

    async fn run_process_probe(
        &self,
        record: &mut AgentRuntimeProbeRecord,
        context: RunningProbeContext<'_>,
    ) -> VibexResult<ProbeExecutionResult> {
        let RunningProbeContext {
            runtime,
            profile_id,
            descriptor_match,
            projection,
            expected_provider_identities,
            cwd,
            process,
            deadline,
        } = context;
        let timeout_duration = deadline.saturating_duration_since(Instant::now());
        if timeout_duration.is_zero() {
            return Err(VibexError::process(
                "agent_runtime_probe_timeout",
                "runtime probe exceeded its deadline before ACP execution",
            ));
        }
        let probe_id = record.id.clone();
        let claude_session_settings = process.claude_session_settings();
        let operation = async {
            self.transition_probe_stage(record, AgentRuntimeProbeStage::InitializingAcp)?;
            let initialize = process
                .request(
                    AcpOperation::Initialize.method(),
                    build_initialize_params(false, false, false, false, false, false),
                    timeout_duration.min(ACP_PROBE_TIMEOUT),
                )
                .await?;
            let identity = &runtime.version_identity;
            let agent_id = identity.route.agent_id.as_str();
            let agent_version = extract_reported_agent_version(agent_id, &initialize)
                .and_then(safe_version_value)
                .map(ToString::to_string);
            let adapter_version = if agent_id == "codex" {
                extract_reported_adapter_version(&initialize)
                    .and_then(safe_version_value)
                    .map(ToString::to_string)
            } else {
                identity.adapter_version.clone()
            };
            let mut facts = vec![
                binary_identity_fact(
                    identity,
                    agent_version.as_deref(),
                    adapter_version.as_deref(),
                ),
                AgentRuntimeProbeFact::passed(AgentRuntimeProbeCapability::AcpHandshake),
            ];
            record.facts = facts.clone();

            self.transition_probe_stage(record, AgentRuntimeProbeStage::Authenticating)?;
            facts.push(auth_fact(
                &initialize,
                profile_id,
                self.provider_config_service(),
            )?);
            record.facts = facts.clone();

            self.transition_probe_stage(record, AgentRuntimeProbeStage::CreatingOrLoadingSession)?;
            let session = process
                .request(
                    AcpOperation::SessionNew.method(),
                    claude_session_request(
                        build_session_new_params(cwd, json!([])),
                        claude_session_settings,
                    ),
                    timeout_duration.min(ACP_PROBE_TIMEOUT),
                )
                .await?;
            let native_session_id = session
                .get("sessionId")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| {
                    VibexError::provider(
                        "agent_runtime_probe_session_id_missing",
                        "ACP session/new did not return a session identity",
                    )
                })?;
            // Keep the native id in this stack frame only. It is never copied
            // into the durable record or evidence.
            let _native_session_id_guard = native_session_id;
            facts.push(AgentRuntimeProbeFact::passed(
                AgentRuntimeProbeCapability::Session,
            ));
            record.facts = facts.clone();

            self.transition_probe_stage(record, AgentRuntimeProbeStage::DiscoveringModels)?;
            let models = extract_model_ids(&session);
            if models.is_empty() {
                facts.push(AgentRuntimeProbeFact {
                    capability: AgentRuntimeProbeCapability::ModelCatalog,
                    status: AgentRuntimeProbeFactStatus::Unsupported,
                    diagnostic_code: Some("model_catalog_unavailable".to_string()),
                });
            } else {
                facts.push(AgentRuntimeProbeFact::passed(
                    AgentRuntimeProbeCapability::ModelCatalog,
                ));
            }
            record.facts = facts.clone();

            self.transition_probe_stage(record, AgentRuntimeProbeStage::ApplyingModelAndConfig)?;
            let target_model = projection.and_then(|value| value.effective_model.as_deref());
            let startup_model = process.startup_model();
            let is_copilot = runtime.version_identity.route.agent_id.as_str() == "copilot";
            let compatibility = self
                .compatibility_registry
                .for_agent(&runtime.version_identity.route.agent_id);
            let catalog_semantics = compatibility
                .map(|descriptor| descriptor.model_catalog_semantics)
                .unwrap_or(ModelCatalogSemantics::ProbeRequired);
            let selection_strategy = compatibility
                .map(|descriptor| descriptor.model_selection_strategy)
                .unwrap_or(ModelSelectionStrategy::RuntimeNegotiated);
            let mut model_apply_response = None;
            let model_fact = if let Some(model) = target_model {
                if catalog_semantics == ModelCatalogSemantics::Closed
                    && !models.is_empty()
                    && !models.iter().any(|candidate| candidate == model)
                {
                    AgentRuntimeProbeFact {
                        capability: AgentRuntimeProbeCapability::ModelSelection,
                        status: AgentRuntimeProbeFactStatus::Failed,
                        diagnostic_code: Some(
                            "target_model_not_advertised_by_closed_catalog".to_string(),
                        ),
                    }
                } else if selection_strategy == ModelSelectionStrategy::StartupProjection
                    || is_copilot
                {
                    classify_startup_model(
                        model,
                        startup_model,
                        probe_current_model_id(Some(identity), &initialize, &session).as_deref(),
                    )
                } else {
                    let result = process
                        .request(
                            AcpOperation::SessionSetModel.method(),
                            protocol::build_session_set_model_params(
                                _native_session_id_guard,
                                model,
                            ),
                            timeout_duration.min(ACP_PROBE_TIMEOUT),
                        )
                        .await;
                    match result {
                        Ok(response) => {
                            model_apply_response = Some(response);
                            AgentRuntimeProbeFact::passed(
                                AgentRuntimeProbeCapability::ModelSelection,
                            )
                        }
                        Err(error) if is_method_unsupported(&error) => AgentRuntimeProbeFact {
                            capability: AgentRuntimeProbeCapability::ModelSelection,
                            status: AgentRuntimeProbeFactStatus::Unsupported,
                            diagnostic_code: Some("session_model_mutation_unsupported".to_string()),
                        },
                        Err(_) => AgentRuntimeProbeFact {
                            capability: AgentRuntimeProbeCapability::ModelSelection,
                            status: AgentRuntimeProbeFactStatus::Failed,
                            diagnostic_code: Some("model_selection_failed".to_string()),
                        },
                    }
                }
            } else if (is_copilot && startup_model.is_some())
                || extract_current_model_id(&session).is_some()
            {
                AgentRuntimeProbeFact::passed(AgentRuntimeProbeCapability::ModelSelection)
            } else {
                AgentRuntimeProbeFact {
                    capability: AgentRuntimeProbeCapability::ModelSelection,
                    status: AgentRuntimeProbeFactStatus::Unsupported,
                    diagnostic_code: Some("model_selection_unavailable".to_string()),
                }
            };
            facts.push(model_fact);
            record.facts = facts.clone();

            if record.request.minimal_prompt {
                self.transition_probe_stage(record, AgentRuntimeProbeStage::OptionalMinimalPrompt)?;
                let _ = process
                    .request(
                        AcpOperation::SessionPrompt.method(),
                        protocol::build_session_prompt_params(
                            _native_session_id_guard,
                            vec![json!({"type":"text","text":"VIBEX_RUNTIME_PROBE"})],
                        ),
                        timeout_duration.min(ACP_PROBE_TIMEOUT),
                    )
                    .await?;
            }

            self.transition_probe_stage(
                record,
                AgentRuntimeProbeStage::ConfirmingEffectiveProvider,
            )?;
            let provider_fact = effective_provider_fact(
                Some(identity),
                &initialize,
                &session,
                model_apply_response.as_ref(),
                startup_model,
                projection,
                expected_provider_identities,
            );
            facts.push(provider_fact);
            let resume_fact = probe_session_resume(
                &initialize,
                &process,
                _native_session_id_guard,
                cwd,
                timeout_duration.min(ACP_PROBE_TIMEOUT),
                claude_session_settings,
            )
            .await;
            facts.push(resume_fact);
            facts.push(AgentRuntimeProbeFact::blocked(
                AgentRuntimeProbeCapability::SwitchCompatibility,
                "switch_compatibility_not_confirmed",
            ));
            facts.push(AgentRuntimeProbeFact::passed(
                AgentRuntimeProbeCapability::Redaction,
            ));
            record.facts = facts.clone();
            self.transition_probe_stage(record, AgentRuntimeProbeStage::CleaningUp)?;
            if Instant::now() >= deadline {
                return Err(VibexError::process(
                    "agent_runtime_probe_timeout",
                    "runtime probe exceeded its deadline",
                ));
            }
            Ok(ProbeExecutionResult {
                facts,
                agent_version,
                adapter_version,
                descriptor_match,
                projection_fingerprint: projection.map(|value| value.fingerprint.clone()),
                switch_behavior: projection
                    .map(|value| value.switch_behavior)
                    .unwrap_or(ProviderSwitchBehavior::RestartAndResume),
                source_survived_prepare_failure: false,
            })
        };
        tokio::select! {
            result = timeout(timeout_duration, operation) => match result {
                Ok(result) => result,
                Err(_) => Err(VibexError::process(
                    "agent_runtime_probe_timeout",
                    "runtime probe exceeded its deadline",
                )),
            },
            cancellation = self.wait_for_probe_cancel(&probe_id) => {
                cancellation?;
                Err(VibexError::conflict(
                    "agent_runtime_probe_cancelled",
                    "runtime probe cancellation was requested",
                ))
            }
        }
    }

    fn transition_probe_stage(
        &self,
        record: &mut AgentRuntimeProbeRecord,
        stage: AgentRuntimeProbeStage,
    ) -> VibexResult<()> {
        self.ensure_probe_not_cancelled(record)?;
        let expected_revision = record.revision;
        record.stage = stage;
        record.updated_at_ms = unix_timestamp_ms().max(record.updated_at_ms);
        record.revision = record.revision.saturating_add(1).max(1);
        let conn = self.open_probe_connection()?;
        match AgentRuntimeProbeRepository::update(&conn, record, expected_revision) {
            Ok(updated) => {
                *record = updated;
                Ok(())
            }
            Err(error) if error.code == "agent_runtime_probe_revision_conflict" => {
                let current = AgentRuntimeProbeRepository::get(&conn, &record.id)?;
                if current.as_ref().is_some_and(|value| {
                    value.cancel_requested || value.status == AgentRuntimeProbeStatus::Cancelled
                }) {
                    Err(VibexError::conflict(
                        "agent_runtime_probe_cancelled",
                        "runtime probe cancellation was requested",
                    ))
                } else {
                    Err(error)
                }
            }
            Err(error) => Err(error),
        }
    }

    fn ensure_probe_not_cancelled(&self, record: &AgentRuntimeProbeRecord) -> VibexResult<()> {
        let conn = self.open_probe_connection()?;
        let current = AgentRuntimeProbeRepository::get(&conn, &record.id)?;
        if record.cancel_requested
            || current.as_ref().is_some_and(|value| {
                value.cancel_requested || value.status == AgentRuntimeProbeStatus::Cancelled
            })
        {
            Err(VibexError::conflict(
                "agent_runtime_probe_cancelled",
                "runtime probe cancellation was requested",
            ))
        } else {
            Ok(())
        }
    }

    async fn wait_for_probe_cancel(&self, probe_id: &AgentRuntimeProbeId) -> VibexResult<()> {
        let mut ticker = interval(Duration::from_millis(100));
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            let conn = self.open_probe_connection()?;
            let record = AgentRuntimeProbeRepository::get(&conn, probe_id)?.ok_or_else(|| {
                VibexError::validation(
                    "agent_runtime_probe_not_found",
                    "Agent runtime probe disappeared during execution",
                )
            })?;
            if record.cancel_requested || record.status == AgentRuntimeProbeStatus::Cancelled {
                return Ok(());
            }
        }
    }

    fn plan_probe_projection(
        &self,
        conn: &vibex_db::DbConnection,
        record: &AgentRuntimeProbeRecord,
        runtime: &AgentRuntimeProfile,
        descriptor_match: ProjectionDescriptorMatch,
        probe_root: &Path,
    ) -> VibexResult<ProbeProjectionContext> {
        let provider_profile_id = runtime.legacy_provider_profile_id.clone().or_else(|| {
            vibex_db::ProviderProfileRepository::list_by_agent(
                conn,
                &runtime.version_identity.route.agent_id,
                true,
            )
            .ok()
            .and_then(|profiles| {
                profiles
                    .into_iter()
                    .find(|profile| profile.status == vibex_core::ProviderProfileStatus::Enabled)
                    .map(|profile| profile.id)
            })
        });
        let Some(binding_id) = record.request.binding_id.as_ref() else {
            return Ok(ProbeProjectionContext {
                provider_profile_id,
                descriptor_match,
                projection: None,
                expected_provider_identities: Vec::new(),
            });
        };
        let binding =
            AgentModelProviderBindingRepository::get(conn, binding_id)?.ok_or_else(|| {
                VibexError::validation(
                    "agent_runtime_probe_binding_missing",
                    "runtime probe binding was not found",
                )
            })?;
        let plan = self
            .provider_config_service()
            .plan_agent_provider_projection(&vibex_core::AgentProviderProjectionPreviewRequest {
                binding_id: binding.id,
                workspace_key: record.request.workspace_key.clone(),
            })?;
        let expected_provider_identities = safe_projection_provider_identities(&plan.preview);
        let resolved = AgentProviderProjectionEngine::resolve_and_materialize(
            &plan,
            probe_root,
            &record.request.workspace_key,
        )?;
        Ok(ProbeProjectionContext {
            provider_profile_id,
            descriptor_match,
            projection: Some(resolved),
            expected_provider_identities,
        })
    }

    fn resolve_probe_config_env(
        &self,
        profile: &vibex_core::ProviderProfile,
        config: &vibex_core::AcpProviderConfig,
    ) -> VibexResult<Vec<(String, String)>> {
        let mut values = Vec::new();
        for reference in &config.env {
            let key = reference.key.trim();
            if key.is_empty() || is_probe_environment_key(key) {
                continue;
            }
            let value = match reference.source {
                vibex_core::AcpProviderEnvSource::Literal => reference.value.clone(),
                vibex_core::AcpProviderEnvSource::ProcessEnvironment => std::env::var(key).ok(),
                vibex_core::AcpProviderEnvSource::SecretReference => reference
                    .secret_lookup_key
                    .as_deref()
                    .and_then(|lookup_key| {
                        profile
                            .secrets
                            .iter()
                            .find(|secret| secret.lookup_key == lookup_key)
                            .and_then(|secret| resolve_provider_secret(secret).ok().flatten())
                    }),
            };
            if let Some(value) = value.filter(|value| !value.trim().is_empty()) {
                values.push((key.to_string(), value));
            }
        }
        Ok(values)
    }

    fn probe_root(&self, probe_id: &AgentRuntimeProbeId) -> VibexResult<PathBuf> {
        let root = self.probe_root_path(probe_id)?;
        fs::create_dir_all(&root).map_err(|error| {
            VibexError::storage(
                "agent_runtime_probe_root_create_failed",
                "failed to create isolated runtime probe root",
            )
            .with_diagnostic("errorKind", format!("{:?}", error.kind()))
        })?;
        Ok(root)
    }

    fn probe_root_path(&self, probe_id: &AgentRuntimeProbeId) -> VibexResult<PathBuf> {
        let parent = self
            .provider_config_service()
            .database_path()
            .parent()
            .ok_or_else(|| {
                VibexError::storage(
                    "agent_runtime_probe_parent_missing",
                    "database path has no parent for runtime probe isolation",
                )
            })?;
        let root = parent
            .join("runtime")
            .join("probes")
            .join(probe_id.as_str());
        Ok(root)
    }

    fn cleanup_probe_root(&self, probe_id: &AgentRuntimeProbeId) -> VibexResult<()> {
        let root = self.probe_root_path(probe_id)?;
        if !root.exists() {
            return Ok(());
        }
        fs::remove_dir_all(&root).map_err(|error| {
            VibexError::storage(
                "agent_runtime_probe_recovery_cleanup_failed",
                "failed to clean an interrupted runtime probe root",
            )
            .with_diagnostic("errorKind", format!("{:?}", error.kind()))
        })
    }

    fn build_evidence(
        &self,
        record: &AgentRuntimeProbeRecord,
        result: ProbeExecutionResult,
    ) -> vibex_core::AgentRuntimeProbeEvidence {
        vibex_core::AgentRuntimeProbeEvidence {
            schema_version: vibex_core::AGENT_RUNTIME_PROBE_SCHEMA_VERSION,
            agent_id: record.agent_id.clone(),
            agent_version: result.agent_version,
            adapter_id: record.adapter_id.clone(),
            adapter_version: result.adapter_version,
            descriptor_id: record.descriptor_id.clone(),
            descriptor_version: record.descriptor_version.clone(),
            descriptor_match: result.descriptor_match,
            projection_fingerprint: result.projection_fingerprint,
            source_revision: PROBE_SOURCE_REVISION.to_string(),
            platform_os: std::env::consts::OS.to_string(),
            platform_arch: std::env::consts::ARCH.to_string(),
            facts: result.facts,
            switch_behavior: result.switch_behavior,
            source_survived_prepare_failure: result.source_survived_prepare_failure,
            redaction_passed: true,
            recorded_at_ms: unix_timestamp_ms(),
        }
    }
}

fn probe_codex_home(
    agent_id: &str,
    root: &Path,
    home: &Path,
    projected_home: Option<&Path>,
) -> VibexResult<PathBuf> {
    let Some(projected_home) = projected_home.filter(|_| agent_id == "codex") else {
        return Ok(home.to_path_buf());
    };
    let invalid_path = || {
        VibexError::validation(
            "agent_runtime_probe_projection_home_invalid",
            "Codex probe projection home must be an isolated directory inside this probe",
        )
    };
    let relative = projected_home
        .strip_prefix(root)
        .map_err(|_| invalid_path())?;
    if relative.as_os_str().is_empty()
        || relative
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return Err(invalid_path());
    }
    let mut directory = root.to_path_buf();
    for component in std::iter::once(None).chain(relative.components().map(Some)) {
        if let Some(component) = component {
            directory.push(component);
        }
        let metadata = fs::symlink_metadata(&directory).map_err(|error| {
            VibexError::storage(
                "agent_runtime_probe_projection_home_unavailable",
                "failed to inspect Codex probe projection directory",
            )
            .with_diagnostic("errorKind", format!("{:?}", error.kind()))
        })?;
        let linked = metadata.file_type().is_symlink();
        #[cfg(windows)]
        let linked = {
            use std::os::windows::fs::MetadataExt;
            linked || metadata.file_attributes() & 0x400 != 0
        };
        if linked || !metadata.is_dir() {
            return Err(invalid_path());
        }
    }
    let resolve = |path: &Path| {
        fs::canonicalize(path).map_err(|error| {
            VibexError::storage(
                "agent_runtime_probe_projection_home_unavailable",
                "failed to resolve Codex probe projection directory",
            )
            .with_diagnostic("errorKind", format!("{:?}", error.kind()))
        })
    };
    let resolved_root = resolve(root)?;
    let resolved_home = resolve(projected_home)?;
    if resolved_home == resolved_root || !resolved_home.starts_with(&resolved_root) {
        return Err(invalid_path());
    }
    Ok(projected_home.to_path_buf())
}

fn add_isolation_environment(
    env: &mut Vec<(String, String)>,
    home: &Path,
    config: &Path,
    data: &Path,
    state: &Path,
    cache: &Path,
    codex_home: &Path,
) {
    // Projection env is assembled from several sources and may contain the
    // same key more than once. Remove every protected entry before appending
    // the probe-owned values so a later duplicate cannot escape the sandbox.
    env.retain(|(name, _)| !is_probe_environment_key(name));
    for (key, value) in [
        ("HOME", home),
        ("USERPROFILE", home),
        ("XDG_CONFIG_HOME", config),
        ("XDG_DATA_HOME", data),
        ("XDG_STATE_HOME", state),
        ("XDG_CACHE_HOME", cache),
        ("CODEX_HOME", codex_home),
    ] {
        let value = value.to_string_lossy().into_owned();
        env.push((key.to_string(), value));
    }
}

fn is_probe_environment_key(name: &str) -> bool {
    PROBE_ENV_KEYS
        .iter()
        .any(|protected| name.eq_ignore_ascii_case(protected))
}

/// Attaches the Claude session settings tier to one probe request.
fn claude_session_request(mut params: Value, meta: Option<&Value>) -> Value {
    crate::claude_session::attach_session_settings_meta(&mut params, meta);
    params
}

async fn probe_session_resume(
    initialize: &Value,
    process: &Arc<AcpProcess>,
    native_session_id: &str,
    cwd: &Path,
    request_timeout: Duration,
    claude_session_settings: Option<&Value>,
) -> AgentRuntimeProbeFact {
    let Some(capabilities) = initialize.get("agentCapabilities") else {
        return AgentRuntimeProbeFact {
            capability: AgentRuntimeProbeCapability::SessionResume,
            status: AgentRuntimeProbeFactStatus::Unsupported,
            diagnostic_code: Some("session_resume_unsupported".to_string()),
        };
    };
    let supports_load = capabilities
        .get("loadSession")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let supports_resume = capabilities
        .get("sessionCapabilities")
        .and_then(|value| value.get("resume"))
        .is_some();
    if !supports_resume && !supports_load {
        return AgentRuntimeProbeFact {
            capability: AgentRuntimeProbeCapability::SessionResume,
            status: AgentRuntimeProbeFactStatus::Unsupported,
            diagnostic_code: Some("session_resume_unsupported".to_string()),
        };
    }

    // Match the live runtime's preference order. If negotiated resume is
    // unavailable at the operation boundary, a declared session/load method
    // is a valid fallback for this capability.
    let operations = [
        (supports_resume, AcpOperation::SessionResume),
        (supports_load, AcpOperation::SessionLoad),
    ];
    let mut saw_unsupported = false;
    for (advertised, operation) in operations {
        if !advertised {
            continue;
        }
        let params = match operation {
            AcpOperation::SessionResume => claude_session_request(
                build_session_resume_params(native_session_id, cwd, json!([])),
                claude_session_settings,
            ),
            AcpOperation::SessionLoad => claude_session_request(
                build_session_load_params(native_session_id, cwd, &[], json!([])),
                claude_session_settings,
            ),
            _ => unreachable!("session restore probe only uses restore operations"),
        };
        match process
            .request(operation.method(), params, request_timeout)
            .await
        {
            Ok(response) => {
                match validate_restore_response(operation, &response, native_session_id) {
                    Ok(()) => {
                        return AgentRuntimeProbeFact::passed(
                            AgentRuntimeProbeCapability::SessionResume,
                        );
                    }
                    Err(error) => {
                        return AgentRuntimeProbeFact {
                            capability: AgentRuntimeProbeCapability::SessionResume,
                            status: AgentRuntimeProbeFactStatus::Failed,
                            diagnostic_code: Some(safe_code(&error.code)),
                        };
                    }
                }
            }
            Err(error) if is_method_unsupported(&error) => {
                saw_unsupported = true;
            }
            Err(error) => {
                return AgentRuntimeProbeFact {
                    capability: AgentRuntimeProbeCapability::SessionResume,
                    status: AgentRuntimeProbeFactStatus::Failed,
                    diagnostic_code: Some(safe_code(&error.code)),
                };
            }
        }
    }
    AgentRuntimeProbeFact {
        capability: AgentRuntimeProbeCapability::SessionResume,
        status: AgentRuntimeProbeFactStatus::Unsupported,
        diagnostic_code: Some(
            if saw_unsupported {
                "session_resume_method_unsupported"
            } else {
                "session_resume_unsupported"
            }
            .to_string(),
        ),
    }
}

fn auth_fact(
    initialize: &Value,
    profile_id: &vibex_core::ProviderProfileId,
    config_service: &ProviderConfigService,
) -> VibexResult<AgentRuntimeProbeFact> {
    let explicit_ready = initialize
        .get("auth")
        .and_then(|auth| auth.get("ready").or_else(|| auth.get("authenticated")))
        .and_then(Value::as_bool)
        .or_else(|| initialize.get("authenticated").and_then(Value::as_bool));
    if explicit_ready.is_some() {
        return Ok(classify_authentication(explicit_ready, false));
    }
    let profile = config_service.get_profile(profile_id)?;
    let Some(profile) = profile else {
        return Ok(AgentRuntimeProbeFact::blocked(
            AgentRuntimeProbeCapability::Authentication,
            "provider_profile_missing",
        ));
    };
    let has_credential = profile.secrets.iter().any(|secret| {
        matches!(
            secret.setup_state,
            vibex_core::ProviderSecretSetupState::Available
                | vibex_core::ProviderSecretSetupState::Referenced
        )
    });
    Ok(classify_authentication(None, has_credential))
}

fn classify_authentication(
    explicit_ready: Option<bool>,
    has_credential_reference: bool,
) -> AgentRuntimeProbeFact {
    match explicit_ready {
        Some(true) => AgentRuntimeProbeFact::passed(AgentRuntimeProbeCapability::Authentication),
        Some(false) => AgentRuntimeProbeFact {
            capability: AgentRuntimeProbeCapability::Authentication,
            status: AgentRuntimeProbeFactStatus::Failed,
            diagnostic_code: Some("authentication_not_ready".to_string()),
        },
        None if has_credential_reference => AgentRuntimeProbeFact::blocked(
            AgentRuntimeProbeCapability::Authentication,
            "authentication_not_confirmed",
        ),
        None => AgentRuntimeProbeFact::blocked(
            AgentRuntimeProbeCapability::Authentication,
            "authentication_not_configured",
        ),
    }
}

fn classify_startup_model(
    target_model: &str,
    startup_model: Option<&str>,
    observed_model: Option<&str>,
) -> AgentRuntimeProbeFact {
    if observed_model.is_some_and(|model| model != target_model) {
        AgentRuntimeProbeFact {
            capability: AgentRuntimeProbeCapability::ModelSelection,
            status: AgentRuntimeProbeFactStatus::Failed,
            diagnostic_code: Some("startup_model_mismatch".to_string()),
        }
    } else if startup_model == Some(target_model) {
        AgentRuntimeProbeFact::passed(AgentRuntimeProbeCapability::ModelSelection)
    } else {
        AgentRuntimeProbeFact {
            capability: AgentRuntimeProbeCapability::ModelSelection,
            status: AgentRuntimeProbeFactStatus::Failed,
            diagnostic_code: Some("startup_model_not_applied".to_string()),
        }
    }
}

fn probe_current_model_id(
    identity: Option<&vibex_core::AgentRuntimeVersionIdentity>,
    initialize: &Value,
    response: &Value,
) -> Option<String> {
    let observed = extract_current_model_id(response)?;
    let supported = identity.is_some_and(|identity| {
        identity.route.agent_id.as_str() == crate::registry::CODEX_AGENT_ID
            && identity.route.adapter_id.as_str() == crate::registry::CODEX_ADAPTER_ID
            && identity.route.transport_kind == vibex_core::TransportKind::Acp
            && identity.adapter_version.as_deref() == Some("2.12.0")
            && extract_reported_adapter_version(initialize) == Some("2.12.0")
            && initialize
                .pointer("/agentInfo/name")
                .and_then(Value::as_str)
                == Some(crate::registry::CODEX_ADAPTER_PACKAGE)
    });
    if supported && let Some(model) = codex_model_with_consistent_options(response, &observed) {
        return Some(model.to_string());
    }
    Some(observed)
}

fn codex_model_with_consistent_options<'a>(response: &Value, observed: &'a str) -> Option<&'a str> {
    let (model, suffix) = observed.split_once('[')?;
    let effort = suffix.strip_suffix(']')?;
    if model.is_empty()
        || model.trim() != model
        || model.contains(']')
        || effort.is_empty()
        || !effort
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
    {
        return None;
    }
    let options = response.get("configOptions")?.as_array()?;
    for (id, category, expected) in [
        ("model", "model", model),
        ("reasoning_effort", "thought_level", effort),
    ] {
        let mut matches = options.iter().filter(|option| {
            option.get("id").and_then(Value::as_str) == Some(id)
                || option.get("category").and_then(Value::as_str) == Some(category)
        });
        let Some(option) = matches.next() else {
            if id == "reasoning_effort" {
                continue;
            }
            return None;
        };
        if matches.next().is_some()
            || option.get("id").and_then(Value::as_str) != Some(id)
            || option.get("category").and_then(Value::as_str) != Some(category)
            || option.get("type").and_then(Value::as_str) != Some("select")
            || option.get("currentValue").and_then(Value::as_str) != Some(expected)
        {
            return None;
        }
    }
    Some(model)
}

fn effective_provider_fact(
    identity: Option<&vibex_core::AgentRuntimeVersionIdentity>,
    initialize: &Value,
    session: &Value,
    model_apply_response: Option<&Value>,
    startup_model: Option<&str>,
    projection: Option<&vibex_config_switch::ResolvedAgentProviderProjection>,
    expected_provider_identities: &[String],
) -> AgentRuntimeProbeFact {
    let values = model_apply_response
        .into_iter()
        .chain([session, initialize]);
    let explicit = values.clone().find_map(|value| {
        [
            "effectiveProvider",
            "provider",
            "providerId",
            "modelProvider",
            "endpoint",
        ]
        .into_iter()
        .find_map(|key| value.get(key).and_then(Value::as_str).map(str::trim))
        .filter(|value| !value.is_empty())
    });
    let target_model = projection.and_then(|value| value.effective_model.as_deref());
    let current_model = model_apply_response
        .and_then(|value| probe_current_model_id(identity, initialize, value))
        .or_else(|| probe_current_model_id(identity, initialize, session))
        .or_else(|| probe_current_model_id(identity, initialize, initialize))
        .or_else(|| startup_model.map(ToString::to_string));
    classify_effective_provider(
        explicit,
        expected_provider_identities,
        target_model,
        current_model.as_deref(),
    )
}

fn classify_effective_provider(
    explicit_provider: Option<&str>,
    expected_provider_identities: &[String],
    target_model: Option<&str>,
    current_model: Option<&str>,
) -> AgentRuntimeProbeFact {
    let observed_provider = explicit_provider.and_then(normalize_provider_identity);
    let provider_matches = observed_provider.as_ref().is_some_and(|observed| {
        expected_provider_identities
            .iter()
            .any(|expected| expected.eq_ignore_ascii_case(observed))
    });
    if provider_matches && target_model.is_some() && current_model == target_model {
        AgentRuntimeProbeFact::passed(AgentRuntimeProbeCapability::ProviderProjection)
    } else {
        AgentRuntimeProbeFact {
            capability: AgentRuntimeProbeCapability::ProviderProjection,
            status: AgentRuntimeProbeFactStatus::Blocked,
            diagnostic_code: Some(
                if explicit_provider.is_some()
                    && !expected_provider_identities.is_empty()
                    && !provider_matches
                {
                    "effective_provider_mismatch"
                } else if provider_matches && target_model.is_some() {
                    "effective_model_not_confirmed"
                } else {
                    "effective_provider_not_confirmed"
                }
                .to_string(),
            ),
        }
    }
}

fn safe_projection_provider_identities(preview: &AgentProviderProjectionPreview) -> Vec<String> {
    let mut identities = preview
        .targets
        .iter()
        .filter(|target| {
            target.field == "endpoint"
                && !target.value_preview.eq_ignore_ascii_case("not configured")
        })
        .filter_map(|target| normalize_provider_identity(&target.value_preview))
        .collect::<Vec<_>>();
    identities.sort();
    identities.dedup();
    identities
}

fn normalize_provider_identity(value: &str) -> Option<String> {
    let url = Url::parse(value.trim()).ok()?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return None;
    }
    Some(url.origin().ascii_serialization().to_ascii_lowercase())
}

fn extract_reported_adapter_version(initialize: &Value) -> Option<&str> {
    initialize
        .get("agentInfo")
        .and_then(|info| info.get("version"))
        .and_then(Value::as_str)
}

fn extract_reported_agent_version<'a>(agent_id: &str, initialize: &'a Value) -> Option<&'a str> {
    let metadata_version = initialize
        .get("_meta")
        .and_then(|meta| meta.get("agentVersion"))
        .and_then(Value::as_str);
    if agent_id == "codex" {
        metadata_version
    } else {
        extract_reported_adapter_version(initialize).or(metadata_version)
    }
}

fn binary_identity_fact(
    identity: &vibex_core::AgentRuntimeVersionIdentity,
    observed_agent_version: Option<&str>,
    observed_adapter_version: Option<&str>,
) -> AgentRuntimeProbeFact {
    if identity.route.agent_id.as_str() == "codex" {
        match (
            identity.adapter_version.as_deref(),
            observed_adapter_version,
        ) {
            (Some(expected), Some(observed)) if expected != observed => {
                return AgentRuntimeProbeFact {
                    capability: AgentRuntimeProbeCapability::BinaryIdentity,
                    status: AgentRuntimeProbeFactStatus::Failed,
                    diagnostic_code: Some("adapter_version_mismatch".to_string()),
                };
            }
            (_, None) => {
                return AgentRuntimeProbeFact::blocked(
                    AgentRuntimeProbeCapability::BinaryIdentity,
                    "adapter_version_not_reported",
                );
            }
            _ => {}
        }
    }
    match (identity.agent_version.as_deref(), observed_agent_version) {
        (Some(expected), Some(observed)) if expected == observed => {
            AgentRuntimeProbeFact::passed(AgentRuntimeProbeCapability::BinaryIdentity)
        }
        (Some(_), Some(_)) => AgentRuntimeProbeFact {
            capability: AgentRuntimeProbeCapability::BinaryIdentity,
            status: AgentRuntimeProbeFactStatus::Failed,
            diagnostic_code: Some("agent_version_mismatch".to_string()),
        },
        (Some(_), None) | (None, None) => AgentRuntimeProbeFact::blocked(
            AgentRuntimeProbeCapability::BinaryIdentity,
            "agent_version_not_reported",
        ),
        (None, Some(_)) => {
            AgentRuntimeProbeFact::passed(AgentRuntimeProbeCapability::BinaryIdentity)
        }
    }
}

fn aggregate_probe_status(
    evidence: &vibex_core::AgentRuntimeProbeEvidence,
) -> AgentRuntimeProbeStatus {
    if evidence
        .facts
        .iter()
        .any(|fact| fact.status == AgentRuntimeProbeFactStatus::Failed)
    {
        return AgentRuntimeProbeStatus::Failed;
    }
    if evidence.facts.iter().any(|fact| {
        is_provider_verification_fact(fact.capability)
            && fact.status != AgentRuntimeProbeFactStatus::Passed
    }) || !evidence.provider_projection_verified()
    {
        return AgentRuntimeProbeStatus::Blocked;
    }
    AgentRuntimeProbeStatus::Passed
}

fn aggregate_probe_diagnostic(
    evidence: &vibex_core::AgentRuntimeProbeEvidence,
    status: AgentRuntimeProbeStatus,
) -> Option<String> {
    if status == AgentRuntimeProbeStatus::Passed {
        return None;
    }
    evidence
        .facts
        .iter()
        .find(|fact| fact.status == AgentRuntimeProbeFactStatus::Failed)
        .or_else(|| {
            evidence.facts.iter().find(|fact| {
                is_provider_verification_fact(fact.capability)
                    && fact.status != AgentRuntimeProbeFactStatus::Passed
            })
        })
        .and_then(|fact| fact.diagnostic_code.clone())
        .or_else(|| Some("provider_projection_unverified".to_string()))
}

fn is_provider_verification_fact(capability: AgentRuntimeProbeCapability) -> bool {
    matches!(
        capability,
        AgentRuntimeProbeCapability::BinaryIdentity
            | AgentRuntimeProbeCapability::AcpHandshake
            | AgentRuntimeProbeCapability::Authentication
            | AgentRuntimeProbeCapability::Session
            | AgentRuntimeProbeCapability::ModelSelection
            | AgentRuntimeProbeCapability::ProviderProjection
            | AgentRuntimeProbeCapability::Redaction
    )
}

fn is_blocking_probe_error(code: &str) -> bool {
    matches!(
        code,
        "agent_runtime_probe_prerequisite_missing"
            | "agent_runtime_probe_descriptor_stale"
            | "agent_runtime_profile_not_found"
            | "agent_model_provider_binding_not_found"
            | "provider_profile_missing"
    ) || (code.contains("secret")
        && (code.contains("missing") || code.contains("unavailable") || code.contains("reference")))
}

fn is_method_unsupported(error: &VibexError) -> bool {
    error.code.contains("method_not_found")
        || error.code.contains("unsupported")
        || error.code.contains("not_supported")
}

fn safe_code(code: &str) -> String {
    let mut value = code
        .chars()
        .filter(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || *ch == '_')
        .take(96)
        .collect::<String>();
    if value.len() < MIN_DIAGNOSTIC_LEN {
        value = "probe_failed".to_string();
    }
    value
}

fn safe_version_value(value: &str) -> Option<&str> {
    let value = value.trim();
    (!value.is_empty()
        && value.len() <= 192
        && value.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.' | '+')
        }))
    .then_some(value)
}

fn materialized_probe_process_args(
    agent_id: &str,
    config: &vibex_core::AcpProviderConfig,
    projection: Option<&vibex_config_switch::ResolvedAgentProviderProjection>,
) -> Vec<String> {
    let mut args = effective_acp_process_args(config, agent_id);
    if let Some(projection) = projection {
        append_projection_process_args(agent_id, &mut args, &projection.process_args);
    }
    args
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use tempfile::tempdir;
    use vibex_config_switch::ResolvedAgentProviderProjection;
    use vibex_core::{
        AGENT_RUNTIME_PROBE_SCHEMA_VERSION, AcpAdapterId, AcpProcessStrategy, AcpProviderConfig,
        AgentId, AgentModelProviderBindingId, AgentProviderProjectionDescriptorId,
        AgentRuntimeProbeEvidence,
    };

    use super::*;

    fn evidence(facts: Vec<AgentRuntimeProbeFact>) -> AgentRuntimeProbeEvidence {
        AgentRuntimeProbeEvidence {
            schema_version: AGENT_RUNTIME_PROBE_SCHEMA_VERSION,
            agent_id: AgentId::parse("fixture").unwrap(),
            agent_version: Some("1.0.0".to_string()),
            adapter_id: AcpAdapterId::parse("fixture-acp").unwrap(),
            adapter_version: Some("1.0.0".to_string()),
            descriptor_id: AgentProviderProjectionDescriptorId::parse("projection_fixture_v1")
                .unwrap(),
            descriptor_version: "1".to_string(),
            descriptor_match: ProjectionDescriptorMatch::Exact,
            projection_fingerprint: Some("sha256:0123456789abcdef".to_string()),
            source_revision: "fixture".to_string(),
            platform_os: "linux".to_string(),
            platform_arch: "x86_64".to_string(),
            facts,
            switch_behavior: ProviderSwitchBehavior::RestartAndResume,
            source_survived_prepare_failure: false,
            redaction_passed: true,
            recorded_at_ms: 1,
        }
    }

    fn required_facts() -> Vec<AgentRuntimeProbeFact> {
        [
            AgentRuntimeProbeCapability::BinaryIdentity,
            AgentRuntimeProbeCapability::AcpHandshake,
            AgentRuntimeProbeCapability::Authentication,
            AgentRuntimeProbeCapability::Session,
            AgentRuntimeProbeCapability::ModelSelection,
            AgentRuntimeProbeCapability::ProviderProjection,
            AgentRuntimeProbeCapability::Redaction,
        ]
        .into_iter()
        .map(AgentRuntimeProbeFact::passed)
        .collect()
    }

    #[test]
    fn credential_reference_does_not_confirm_authentication() {
        let fact = classify_authentication(None, true);
        assert_eq!(fact.status, AgentRuntimeProbeFactStatus::Blocked);
        assert_eq!(
            fact.diagnostic_code.as_deref(),
            Some("authentication_not_confirmed")
        );
        assert_eq!(
            classify_authentication(Some(true), false).status,
            AgentRuntimeProbeFactStatus::Passed
        );
        assert_eq!(
            classify_authentication(Some(false), true).status,
            AgentRuntimeProbeFactStatus::Failed
        );
    }

    #[test]
    fn reported_agent_version_accepts_standard_and_grok_metadata() {
        assert_eq!(
            extract_reported_agent_version(
                "fixture",
                &json!({
                    "agentInfo": {"version": "2.0.0"},
                    "_meta": {"agentVersion": "1.0.8"}
                })
            ),
            Some("2.0.0")
        );
        assert_eq!(
            extract_reported_agent_version(
                "grok",
                &json!({
                    "_meta": {"agentVersion": "1.0.8"}
                })
            ),
            Some("1.0.8")
        );
    }

    fn codex_identity() -> vibex_core::AgentRuntimeVersionIdentity {
        vibex_core::AgentRuntimeVersionIdentity {
            route: vibex_core::AgentRuntimeRouteKey {
                agent_id: AgentId::parse("codex").unwrap(),
                transport_kind: vibex_core::TransportKind::Acp,
                adapter_id: AcpAdapterId::parse("codex-acp").unwrap(),
            },
            adapter_version: Some("2.12.0".to_string()),
            agent_version: Some("0.171.0".to_string()),
            runtime_dependencies: BTreeMap::new(),
            source: vibex_core::AgentVersionSource::Managed,
        }
    }

    #[test]
    fn codex_adapter_version_is_not_underlying_agent_evidence() {
        let initialize = json!({"agentInfo": {"version": "2.12.0"}});
        let agent_version = extract_reported_agent_version("codex", &initialize);
        let adapter_version = extract_reported_adapter_version(&initialize);
        assert_eq!(agent_version, None);
        assert_eq!(adapter_version, Some("2.12.0"));
        let fact = binary_identity_fact(&codex_identity(), agent_version, adapter_version);
        assert_eq!(fact.status, AgentRuntimeProbeFactStatus::Blocked);
        assert_eq!(
            fact.diagnostic_code.as_deref(),
            Some("agent_version_not_reported")
        );
    }

    #[test]
    fn codex_identity_checks_each_reported_version_independently() {
        let identity = codex_identity();
        let initialize = json!({
            "agentInfo": {"version": "2.12.0"},
            "_meta": {"agentVersion": "0.171.0"}
        });
        assert_eq!(
            binary_identity_fact(
                &identity,
                extract_reported_agent_version("codex", &initialize),
                extract_reported_adapter_version(&initialize),
            )
            .status,
            AgentRuntimeProbeFactStatus::Passed
        );
        for (agent, adapter, status, code) in [
            (
                Some("0.171.0"),
                Some("2.11.0"),
                AgentRuntimeProbeFactStatus::Failed,
                "adapter_version_mismatch",
            ),
            (
                Some("0.170.0"),
                Some("2.12.0"),
                AgentRuntimeProbeFactStatus::Failed,
                "agent_version_mismatch",
            ),
            (
                Some("0.171.0"),
                None,
                AgentRuntimeProbeFactStatus::Blocked,
                "adapter_version_not_reported",
            ),
            (
                None,
                Some("2.12.0"),
                AgentRuntimeProbeFactStatus::Blocked,
                "agent_version_not_reported",
            ),
        ] {
            let fact = binary_identity_fact(&identity, agent, adapter);
            assert_eq!(fact.status, status);
            assert_eq!(fact.diagnostic_code.as_deref(), Some(code));
        }
    }

    #[test]
    fn malformed_versions_are_rejected_not_sanitized_into_matches() {
        for value in [
            "",
            " ",
            "0.17!1.0",
            "0.171.0\nsecret",
            "https://fixture.invalid",
            "C:\\private",
        ] {
            assert_eq!(safe_version_value(value), None);
        }
        assert_eq!(safe_version_value(&"1".repeat(193)), None);
        assert_eq!(safe_version_value(" 0.171.0 "), Some("0.171.0"));
        assert_eq!(
            safe_version_value("0.171.0-alpha.1+build.2"),
            Some("0.171.0-alpha.1+build.2")
        );
        let malformed = json!({"agentInfo": {"version": 2}, "_meta": {"agentVersion": false}});
        assert_eq!(extract_reported_agent_version("codex", &malformed), None);
        assert_eq!(extract_reported_adapter_version(&malformed), None);
    }

    fn codex_launch_config() -> AcpProviderConfig {
        AcpProviderConfig {
            command: "fixture".to_string(),
            args: Vec::new(),
            env: Vec::new(),
            cwd_template: None,
            process_strategy: AcpProcessStrategy::PerSession,
            terminal_tools: false,
            terminal_auth: false,
            models: vec!["target-model".to_string()],
            modes: Vec::new(),
            features: Vec::new(),
            disabled_tools: Vec::new(),
        }
    }

    #[test]
    fn codex_startup_model_uses_final_materialized_environment() {
        let config = codex_launch_config();
        let mut env = vec![("CODEX_HOME".to_string(), "probe-overlay".to_string())];
        crate::codex::upsert_codex_model_override(&mut env, "target-model");
        assert!(config.env.is_empty());
        assert_eq!(
            crate::runtime::startup_model_from_launch(&config, "codex", &env).as_deref(),
            Some("target-model")
        );
        env.push((
            "CODEX_CONFIG".to_string(),
            json!({"model": "other-model"}).to_string(),
        ));
        assert_eq!(
            crate::runtime::startup_model_from_launch(&config, "codex", &env),
            None
        );
    }

    #[test]
    fn codex_startup_model_rejects_discovery_and_missing_launch_evidence() {
        let config = codex_launch_config();
        let home = ("CODEX_HOME".to_string(), "probe-overlay".to_string());
        let model = (
            "CODEX_CONFIG".to_string(),
            json!({"model": "target-model"}).to_string(),
        );
        for env in [
            Vec::new(),
            vec![home.clone()],
            vec![model.clone()],
            vec![("CODEX_HOME".to_string(), " ".to_string()), model.clone()],
            vec![
                home.clone(),
                ("CODEX_CONFIG".to_string(), "invalid".to_string()),
            ],
            vec![home.clone(), ("CODEX_CONFIG".to_string(), "[]".to_string())],
        ] {
            assert_eq!(
                crate::runtime::startup_model_from_launch(&config, "codex", &env),
                None
            );
        }
        let mut discovery = config;
        discovery.models.push("another-model".to_string());
        assert_eq!(
            crate::runtime::startup_model_from_launch(&discovery, "codex", &[home, model]),
            None
        );
    }

    #[test]
    fn codex_startup_model_honors_platform_environment_key_rules() {
        let env = vec![
            ("CODEX_HOME".to_string(), "probe-overlay".to_string()),
            (
                "CODEX_CONFIG".to_string(),
                json!({"model": "target-model"}).to_string(),
            ),
            (
                "codex_config".to_string(),
                json!({"model": "other-model"}).to_string(),
            ),
        ];
        assert_eq!(
            crate::runtime::startup_model_from_launch(&codex_launch_config(), "codex", &env)
                .as_deref(),
            if cfg!(windows) {
                None
            } else {
                Some("target-model")
            }
        );
    }

    #[test]
    fn startup_projection_cannot_override_a_conflicting_session_model() {
        for observed in [None, Some("target-model")] {
            assert_eq!(
                classify_startup_model("target-model", Some("target-model"), observed).status,
                AgentRuntimeProbeFactStatus::Passed
            );
        }
        let mismatch =
            classify_startup_model("target-model", Some("target-model"), Some("other-model"));
        assert_eq!(mismatch.status, AgentRuntimeProbeFactStatus::Failed);
        assert_eq!(
            mismatch.diagnostic_code.as_deref(),
            Some("startup_model_mismatch")
        );
        assert_eq!(
            classify_startup_model("target-model", None, Some("target-model"))
                .diagnostic_code
                .as_deref(),
            Some("startup_model_not_applied")
        );
    }

    fn codex_model_initialize() -> Value {
        json!({"agentInfo": {"name": "@agentclientprotocol/codex-acp", "version": "2.12.0"}})
    }

    fn codex_effort_session(model: &str, effort: &str) -> Value {
        json!({
            "models": {"currentModelId": format!("{model}[{effort}]")},
            "configOptions": [
                {"id": "model", "category": "model", "type": "select", "currentValue": model},
                {"id": "reasoning_effort", "category": "thought_level", "type": "select", "currentValue": effort}
            ]
        })
    }

    #[test]
    fn codex_probe_decodes_model_with_consistent_live_config_evidence() {
        for effort in ["low", "medium", "high", "xhigh", "provider-depth"] {
            let session = codex_effort_session("provider/model", effort);
            let model = probe_current_model_id(
                Some(&codex_identity()),
                &codex_model_initialize(),
                &session,
            );
            assert_eq!(model.as_deref(), Some("provider/model"));
            assert_eq!(
                classify_startup_model("provider/model", Some("provider/model"), model.as_deref())
                    .status,
                AgentRuntimeProbeFactStatus::Passed
            );
            assert_eq!(
                classify_startup_model("other-model", Some("other-model"), model.as_deref()).status,
                AgentRuntimeProbeFactStatus::Failed
            );
        }
    }

    #[test]
    fn codex_custom_model_without_effort_option_keeps_base_model_evidence() {
        let response = json!({
            "models": {"currentModelId": "provider/custom-model[medium]"},
            "configOptions": [
                {"id": "model", "category": "model", "type": "select", "currentValue": "provider/custom-model"}
            ]
        });
        let model = probe_current_model_id(
            Some(&codex_identity()),
            &codex_model_initialize(),
            &response,
        );
        assert_eq!(model.as_deref(), Some("provider/custom-model"));
        assert_eq!(
            classify_startup_model(
                "provider/custom-model",
                Some("provider/custom-model"),
                model.as_deref()
            )
            .status,
            AgentRuntimeProbeFactStatus::Passed
        );
        assert_eq!(
            classify_startup_model(
                "provider/custom-model[high]",
                Some("provider/custom-model[high]"),
                model.as_deref()
            )
            .status,
            AgentRuntimeProbeFactStatus::Failed
        );
    }

    #[test]
    fn codex_probe_keeps_encoded_model_when_config_evidence_is_missing_or_conflicts() {
        let session = codex_effort_session("target-model", "high");
        let mut missing_options = session.clone();
        missing_options
            .as_object_mut()
            .unwrap()
            .remove("configOptions");
        let mut missing_model = session.clone();
        missing_model["configOptions"]
            .as_array_mut()
            .unwrap()
            .remove(0);
        let mut wrong_model = session.clone();
        wrong_model["configOptions"][0]["currentValue"] = json!("other-model");
        let mut wrong_effort = session.clone();
        wrong_effort["configOptions"][1]["currentValue"] = json!("low");
        let mut wrong_type = session.clone();
        wrong_type["configOptions"][1]["type"] = json!("boolean");
        let mut duplicate = session.clone();
        duplicate["configOptions"]
            .as_array_mut()
            .unwrap()
            .push(session["configOptions"][1].clone());
        let mut alias = session.clone();
        alias["configOptions"][1]["id"] = json!("effort");
        for response in [
            missing_options,
            missing_model,
            wrong_model,
            wrong_effort,
            wrong_type,
            duplicate,
            alias,
        ] {
            let observed = probe_current_model_id(
                Some(&codex_identity()),
                &codex_model_initialize(),
                &response,
            );
            assert_eq!(observed.as_deref(), Some("target-model[high]"));
            assert_eq!(
                classify_startup_model("target-model", Some("target-model"), observed.as_deref())
                    .status,
                AgentRuntimeProbeFactStatus::Failed
            );
        }
    }

    #[test]
    fn codex_probe_model_decoding_requires_exact_route_and_reported_adapter() {
        let session = codex_effort_session("target-model", "high");
        let identity = codex_identity();
        let initialize = codex_model_initialize();
        let mut other_agent = identity.clone();
        other_agent.route.agent_id = AgentId::parse("copilot").unwrap();
        let mut other_adapter = identity.clone();
        other_adapter.route.adapter_id = AcpAdapterId::parse("custom-codex").unwrap();
        let mut older = identity.clone();
        older.adapter_version = Some("2.11.0".to_string());
        let mut newer = identity.clone();
        newer.adapter_version = Some("2.13.0".to_string());
        let mut missing = identity.clone();
        missing.adapter_version = None;
        for candidate in [&other_agent, &other_adapter, &older, &newer, &missing] {
            assert_eq!(
                probe_current_model_id(Some(candidate), &initialize, &session).as_deref(),
                Some("target-model[high]")
            );
        }
        for reported in [
            json!({}),
            json!({"agentInfo": {"version": "2.12.0"}}),
            json!({"agentInfo": {"name": "other", "version": "2.12.0"}}),
            json!({"agentInfo": {"name": "@agentclientprotocol/codex-acp", "version": "2.13.0"}}),
        ] {
            assert_eq!(
                probe_current_model_id(Some(&identity), &reported, &session).as_deref(),
                Some("target-model[high]")
            );
        }
        assert_eq!(
            probe_current_model_id(None, &initialize, &session).as_deref(),
            Some("target-model[high]")
        );
    }

    #[test]
    fn codex_probe_model_decoding_rejects_malformed_variants() {
        for encoded in [
            "target-model[]",
            "target-model[high][low]",
            "target-model[high]junk",
            "target-model[[high]]",
            "[high]",
            "target-model[high",
            "target-model[ high]",
            "target-model[high,fast=true]",
        ] {
            let mut session = codex_effort_session("target-model", "high");
            session["models"]["currentModelId"] = json!(encoded);
            assert_eq!(codex_model_with_consistent_options(&session, encoded), None);
            assert_eq!(
                probe_current_model_id(
                    Some(&codex_identity()),
                    &codex_model_initialize(),
                    &session
                )
                .as_deref(),
                Some(encoded)
            );
        }
        let plain = json!({"models": {"currentModelId": "target-model"}});
        assert_eq!(
            probe_current_model_id(Some(&codex_identity()), &codex_model_initialize(), &plain)
                .as_deref(),
            Some("target-model")
        );
    }

    #[test]
    fn decoded_codex_model_does_not_supply_authentication_or_provider_identity() {
        let session = codex_effort_session("target-model", "high");
        let model =
            probe_current_model_id(Some(&codex_identity()), &codex_model_initialize(), &session);
        assert_eq!(
            classify_authentication(None, true).status,
            AgentRuntimeProbeFactStatus::Blocked
        );
        for endpoint in [None, Some("https://wrong.example")] {
            assert_eq!(
                classify_effective_provider(
                    endpoint,
                    &["https://expected.example".to_string()],
                    Some("target-model"),
                    model.as_deref()
                )
                .status,
                AgentRuntimeProbeFactStatus::Blocked
            );
        }
    }

    #[test]
    fn model_catalog_does_not_confirm_effective_provider_or_model() {
        let projection = ResolvedAgentProviderProjection {
            binding_id: AgentModelProviderBindingId::new(),
            non_secret_env: BTreeMap::new(),
            secret_env: Vec::new(),
            overlay_root: PathBuf::from("probe-root"),
            overlay_files: Vec::new(),
            process_args: Vec::new(),
            session_config: Vec::new(),
            effective_model: Some("target-model".to_string()),
            switch_behavior: ProviderSwitchBehavior::RestartAndResume,
            fingerprint: "sha256:0123456789abcdef".to_string(),
            session_env_keys: Vec::new(),
            session_header_env: Vec::new(),
        };
        let session = json!({"models": {"availableModels": [{"modelId": "target-model"}]}});
        let fact = effective_provider_fact(
            None,
            &json!({}),
            &session,
            None,
            None,
            Some(&projection),
            &[],
        );
        assert_eq!(fact.status, AgentRuntimeProbeFactStatus::Blocked);
        assert_eq!(
            fact.diagnostic_code.as_deref(),
            Some("effective_provider_not_confirmed")
        );
    }

    #[test]
    fn startup_model_confirms_model_when_copilot_omits_current_model() {
        let projection = ResolvedAgentProviderProjection {
            binding_id: AgentModelProviderBindingId::new(),
            non_secret_env: BTreeMap::new(),
            secret_env: Vec::new(),
            overlay_root: PathBuf::from("probe-root"),
            overlay_files: Vec::new(),
            process_args: Vec::new(),
            session_config: Vec::new(),
            effective_model: Some("target-model".to_string()),
            switch_behavior: ProviderSwitchBehavior::RestartAndResume,
            fingerprint: "sha256:0123456789abcdef".to_string(),
            session_env_keys: Vec::new(),
            session_header_env: Vec::new(),
        };
        let fact = effective_provider_fact(
            None,
            &json!({"endpoint": "https://profile.example"}),
            &json!({}),
            None,
            Some("target-model"),
            Some(&projection),
            &["https://profile.example".to_string()],
        );
        assert_eq!(fact.status, AgentRuntimeProbeFactStatus::Passed);
        let mut session = codex_effort_session("target-model", "medium");
        session["endpoint"] = json!("https://profile.example/v1");
        let identity = codex_identity();
        let initialize = codex_model_initialize();
        let provider_fact = |response: &Value| {
            effective_provider_fact(
                Some(&identity),
                &initialize,
                response,
                None,
                Some("target-model"),
                Some(&projection),
                &["https://profile.example".to_string()],
            )
        };
        assert_eq!(
            provider_fact(&session).status,
            AgentRuntimeProbeFactStatus::Passed
        );
        session["endpoint"] = json!("https://wrong.example");
        assert_eq!(
            provider_fact(&session).status,
            AgentRuntimeProbeFactStatus::Blocked
        );
        session["endpoint"] = json!("https://profile.example");
        session["configOptions"][1]["currentValue"] = json!("high");
        assert_eq!(
            provider_fact(&session).status,
            AgentRuntimeProbeFactStatus::Blocked
        );
    }

    #[test]
    fn matching_model_does_not_confirm_the_wrong_provider_identity() {
        let fact = classify_effective_provider(
            Some("https://profile-a.example/v1"),
            &["https://profile-b.example".to_string()],
            Some("shared-model"),
            Some("shared-model"),
        );
        assert_eq!(fact.status, AgentRuntimeProbeFactStatus::Blocked);
        assert_eq!(
            fact.diagnostic_code.as_deref(),
            Some("effective_provider_mismatch")
        );
    }

    #[test]
    fn matching_safe_origin_and_model_confirm_provider_projection() {
        let fact = classify_effective_provider(
            Some("HTTPS://PROFILE-B.EXAMPLE:443/v1/models?request=probe"),
            &["https://profile-b.example".to_string()],
            Some("shared-model"),
            Some("shared-model"),
        );
        assert_eq!(fact.status, AgentRuntimeProbeFactStatus::Passed);
        assert_eq!(fact.diagnostic_code, None);
    }

    #[test]
    fn missing_expected_provider_identity_remains_blocked() {
        let fact = classify_effective_provider(
            Some("https://profile-b.example"),
            &[],
            Some("shared-model"),
            Some("shared-model"),
        );
        assert_eq!(fact.status, AgentRuntimeProbeFactStatus::Blocked);
        assert_eq!(
            fact.diagnostic_code.as_deref(),
            Some("effective_provider_not_confirmed")
        );
    }

    #[test]
    fn aggregate_fails_on_any_failed_fact_and_blocks_partial_success() {
        let mut facts = required_facts();
        assert_eq!(
            aggregate_probe_status(&evidence(facts.clone())),
            AgentRuntimeProbeStatus::Passed
        );
        facts.retain(|fact| fact.capability != AgentRuntimeProbeCapability::ProviderProjection);
        facts.push(AgentRuntimeProbeFact::blocked(
            AgentRuntimeProbeCapability::ProviderProjection,
            "effective_provider_not_confirmed",
        ));
        assert_eq!(
            aggregate_probe_status(&evidence(facts.clone())),
            AgentRuntimeProbeStatus::Blocked
        );
        facts.push(AgentRuntimeProbeFact {
            capability: AgentRuntimeProbeCapability::ModelCatalog,
            status: AgentRuntimeProbeFactStatus::Failed,
            diagnostic_code: Some("model_catalog_failed".to_string()),
        });
        assert_eq!(
            aggregate_probe_status(&evidence(facts)),
            AgentRuntimeProbeStatus::Failed
        );
    }

    #[test]
    fn probe_root_guard_removes_isolated_state() {
        let parent = tempdir().unwrap();
        let root = parent.path().join("probe");
        fs::create_dir_all(root.join("overlay")).unwrap();
        fs::write(root.join("overlay/config.json"), b"fixture").unwrap();
        drop(ProbeRootGuard::new(root.clone()));
        assert!(!root.exists());
    }

    #[test]
    fn probe_process_args_use_materialized_probe_overlay_paths() {
        let config = AcpProviderConfig {
            command: "fixture".to_string(),
            args: vec!["--verbose".to_string(), "acp".to_string()],
            env: Vec::new(),
            cwd_template: Some("{workspaceRoot}".to_string()),
            process_strategy: AcpProcessStrategy::PerSession,
            terminal_tools: false,
            terminal_auth: false,
            models: Vec::new(),
            modes: Vec::new(),
            features: Vec::new(),
            disabled_tools: Vec::new(),
        };
        let probe_root = PathBuf::from("probe-root")
            .join("binding")
            .join("workspace");

        let crow_projection = ResolvedAgentProviderProjection {
            binding_id: AgentModelProviderBindingId::new(),
            non_secret_env: BTreeMap::new(),
            secret_env: Vec::new(),
            overlay_root: probe_root.clone(),
            overlay_files: Vec::new(),
            process_args: vec![
                "--config-dir".to_string(),
                probe_root.to_string_lossy().into_owned(),
            ],
            session_config: Vec::new(),
            effective_model: Some("target-model".to_string()),
            switch_behavior: ProviderSwitchBehavior::RestartAndResume,
            fingerprint: "sha256:crow".to_string(),
            session_env_keys: Vec::new(),
            session_header_env: Vec::new(),
        };
        assert_eq!(
            materialized_probe_process_args("crow-cli", &config, Some(&crow_projection)),
            vec![
                "--verbose",
                "acp",
                "--config-dir",
                probe_root.to_string_lossy().as_ref(),
            ]
        );

        let stakpak_config_path = probe_root.join("stakpak.toml");
        let stakpak_projection = ResolvedAgentProviderProjection {
            process_args: vec![
                "--profile".to_string(),
                "vibex".to_string(),
                "--config".to_string(),
                stakpak_config_path.to_string_lossy().into_owned(),
            ],
            fingerprint: "sha256:stakpak".to_string(),
            ..crow_projection
        };
        assert_eq!(
            materialized_probe_process_args("stakpak", &config, Some(&stakpak_projection)),
            vec![
                "--verbose",
                "--profile",
                "vibex",
                "--config",
                stakpak_config_path.to_string_lossy().as_ref(),
                "acp",
            ]
        );
    }

    #[test]
    fn copilot_probe_args_replace_existing_model_with_startup_projection() {
        let mut config = AcpProviderConfig {
            command: "copilot".to_string(),
            args: vec![
                "--acp".to_string(),
                "--model=stale-model".to_string(),
                "--model".to_string(),
                "older-model".to_string(),
            ],
            env: Vec::new(),
            cwd_template: Some("{workspaceRoot}".to_string()),
            process_strategy: AcpProcessStrategy::PerSession,
            terminal_tools: false,
            terminal_auth: false,
            models: Vec::new(),
            modes: Vec::new(),
            features: Vec::new(),
            disabled_tools: Vec::new(),
        };

        apply_startup_model_to_config(&mut config, "copilot", Some("target-model"));

        assert_eq!(config.args, vec!["--acp", "--model", "target-model"]);
        assert_eq!(
            crate::runtime::startup_model_from_launch(&config, "copilot", &[]).as_deref(),
            Some("target-model")
        );
    }

    #[test]
    fn isolation_environment_replaces_all_protected_duplicates() {
        let parent = tempdir().unwrap();
        let home = parent.path().join("home");
        let config = parent.path().join("config");
        let data = parent.path().join("data");
        let state = parent.path().join("state");
        let cache = parent.path().join("cache");
        let mut env = PROBE_ENV_KEYS
            .iter()
            .flat_map(|key| {
                [
                    (key.to_string(), "/user/home".to_string()),
                    (key.to_ascii_lowercase(), "/shadow/home".to_string()),
                ]
            })
            .collect::<Vec<_>>();
        env.push(("UNRELATED".to_string(), "keep".to_string()));

        add_isolation_environment(&mut env, &home, &config, &data, &state, &cache, &home);

        for (key, expected) in [
            ("HOME", &home),
            ("USERPROFILE", &home),
            ("XDG_CONFIG_HOME", &config),
            ("XDG_DATA_HOME", &data),
            ("XDG_STATE_HOME", &state),
            ("XDG_CACHE_HOME", &cache),
            ("CODEX_HOME", &home),
        ] {
            let entries = env
                .iter()
                .filter(|(name, _)| name.eq_ignore_ascii_case(key))
                .collect::<Vec<_>>();
            assert_eq!(entries.len(), 1);
            assert_eq!(entries[0].0, key);
            assert_eq!(entries[0].1, expected.to_string_lossy());
        }
        assert_eq!(
            env.iter().find(|(key, _)| key == "UNRELATED"),
            Some(&("UNRELATED".to_string(), "keep".to_string()))
        );
    }

    #[test]
    fn codex_probe_home_preserves_materialized_config_and_catalog() {
        let parent = tempdir().unwrap();
        let root = parent.path().join("probe");
        let home = root.join("home");
        let overlay = root.join("binding").join("workspace");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&overlay).unwrap();
        fs::write(overlay.join("config.toml"), "model = \"fixture\"\n").unwrap();
        fs::write(overlay.join("vibex-models.json"), "{\"models\":[]}").unwrap();
        let codex_home = probe_codex_home("codex", &root, &home, Some(&overlay)).unwrap();
        let mut env = vec![
            ("CODEX_HOME".to_string(), "/user/codex".to_string()),
            ("Codex_Home".to_string(), "/shadow/codex".to_string()),
            ("HOME".to_string(), "/user/home".to_string()),
            ("UNRELATED".to_string(), "keep".to_string()),
        ];
        let mut config = codex_launch_config();
        apply_startup_model_to_config(&mut config, "codex", Some("fixture"));
        crate::codex::upsert_codex_model_override(&mut env, "fixture");
        add_isolation_environment(&mut env, &home, &home, &home, &home, &home, &codex_home);
        assert_eq!(
            crate::runtime::startup_model_from_launch(&config, "codex", &env).as_deref(),
            Some("fixture")
        );
        let homes = env
            .iter()
            .filter(|(key, _)| key.eq_ignore_ascii_case("CODEX_HOME"))
            .collect::<Vec<_>>();
        assert_eq!(homes.len(), 1);
        let effective_home = Path::new(&homes[0].1);
        assert_eq!(effective_home, overlay);
        assert_eq!(
            fs::read_to_string(effective_home.join("config.toml")).unwrap(),
            "model = \"fixture\"\n"
        );
        assert_eq!(
            fs::read_to_string(effective_home.join("vibex-models.json")).unwrap(),
            "{\"models\":[]}"
        );
        assert!(!home.join("config.toml").exists());
        assert!(env.contains(&("HOME".to_string(), home.to_string_lossy().into_owned())));
        assert!(env.contains(&("UNRELATED".to_string(), "keep".to_string())));
    }

    #[test]
    fn codex_probe_home_fallback_does_not_trust_environment_or_other_agents() {
        let parent = tempdir().unwrap();
        let home = parent.path().join("home");
        let outside = parent.path().join("outside");
        assert_eq!(
            probe_codex_home("codex", parent.path(), &home, None).unwrap(),
            home
        );
        assert_eq!(
            probe_codex_home("deepseek-harness", parent.path(), &home, Some(&outside)).unwrap(),
            home
        );
        let mut env = vec![("DSH_HOME".to_string(), "projected-dsh-home".to_string())];
        add_isolation_environment(&mut env, &home, &home, &home, &home, &home, &home);
        assert!(env.contains(&("DSH_HOME".to_string(), "projected-dsh-home".to_string())));
        assert!(env.contains(&(
            "CODEX_HOME".to_string(),
            home.to_string_lossy().into_owned()
        )));
    }

    #[test]
    fn codex_probe_home_rejects_outside_root_and_parent_components() {
        let parent = tempdir().unwrap();
        let root = parent.path().join("probe");
        let outside = parent.path().join("probe-other");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&outside).unwrap();
        for overlay in [&outside, &root, &root.join("..").join("probe-other")] {
            let error =
                probe_codex_home("codex", &root, &root.join("home"), Some(overlay)).unwrap_err();
            assert_eq!(error.code, "agent_runtime_probe_projection_home_invalid");
        }
    }

    #[test]
    fn codex_probe_home_rejects_missing_and_file_targets() {
        let parent = tempdir().unwrap();
        let root = parent.path();
        let home = root.join("home");
        let missing =
            probe_codex_home("codex", root, &home, Some(&root.join("missing"))).unwrap_err();
        assert_eq!(
            missing.code,
            "agent_runtime_probe_projection_home_unavailable"
        );
        let file = root.join("file");
        fs::write(&file, "fixture").unwrap();
        let error = probe_codex_home("codex", root, &home, Some(&file)).unwrap_err();
        assert_eq!(error.code, "agent_runtime_probe_projection_home_invalid");
    }

    #[cfg(unix)]
    #[test]
    fn codex_probe_home_rejects_linked_directories() {
        let parent = tempdir().unwrap();
        let root = parent.path().join("probe");
        let outside = parent.path().join("outside");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&outside).unwrap();
        let linked = root.join("linked");
        std::os::unix::fs::symlink(&outside, &linked).unwrap();
        let error =
            probe_codex_home("codex", &root, &root.join("home"), Some(&linked)).unwrap_err();
        assert_eq!(error.code, "agent_runtime_probe_projection_home_invalid");
    }

    #[cfg(windows)]
    #[test]
    fn codex_probe_home_rejects_windows_junctions_at_root_and_ancestor() {
        use std::os::windows::process::CommandExt;

        let parent = tempdir().unwrap();
        let root = parent.path().join("probe");
        let outside = parent.path().join("outside");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(outside.join("workspace")).unwrap();
        let linked = root.join("linked");
        let output = std::process::Command::new("powershell.exe")
            .args([
                "-NoLogo", "-NoProfile", "-NonInteractive", "-Command",
                "$ErrorActionPreference='Stop'; New-Item -ItemType Junction -Path $env:VIBEX_TEST_JUNCTION -Target $env:VIBEX_TEST_TARGET | Out-Null",
            ])
            .env("VIBEX_TEST_JUNCTION", &linked)
            .env("VIBEX_TEST_TARGET", &outside)
            .creation_flags(0x08000000)
            .output()
            .unwrap();
        assert!(output.status.success(), "junction fixture creation failed");
        for probe_root in [&root, &linked] {
            let error = probe_codex_home(
                "codex",
                probe_root,
                &root.join("home"),
                Some(&linked.join("workspace")),
            )
            .unwrap_err();
            assert_eq!(error.code, "agent_runtime_probe_projection_home_invalid");
        }
    }
}
