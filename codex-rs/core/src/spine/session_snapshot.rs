use anyhow::Context;
use codex_config::config_toml::ConfigToml;
use codex_config::config_toml::OrchestratorFeatureToml;
use codex_config::config_toml::OrchestratorToml;
use codex_config::config_toml::SpineSpawnConfigToml;
use codex_config::spine_snapshot::ConfigLockfileToml;
use codex_config::types::MemoriesToml;
use codex_features::CurrentTimeReminderConfigToml;
use codex_features::Feature;
use codex_features::FeatureToml;
use codex_features::FeaturesToml;
use codex_features::MultiAgentV2ConfigToml;
use codex_features::RolloutBudgetConfigToml;
use codex_features::TokenBudgetConfigToml;
use codex_features::ToolRegistryConfigToml;
use codex_protocol::ThreadId;

use crate::config::Config;
use crate::spine::config_snapshot::clear_config_lock_debug_controls;
use crate::spine::config_snapshot::config_lockfile;
use crate::spine::config_snapshot::toml_round_trip;

use crate::session::session::SessionConfiguration;

pub(crate) async fn export_config_lock_if_configured(
    session_configuration: &SessionConfiguration,
    conversation_id: ThreadId,
) -> anyhow::Result<()> {
    let config = session_configuration.original_config_do_not_use.as_ref();
    let Some(export_dir) = config.config_lock_export_dir.as_ref() else {
        return Ok(());
    };

    let lock = session_configuration.to_config_lockfile_toml()?;
    let lock = toml::to_string_pretty(&lock).context("failed to serialize config lock")?;
    let path = export_dir.join(format!("{conversation_id}.config.lock.toml"));

    tokio::fs::create_dir_all(export_dir)
        .await
        .with_context(|| {
            format!(
                "failed to create config lock export directory {}",
                export_dir.display()
            )
        })?;
    tokio::fs::write(&path, lock)
        .await
        .with_context(|| format!("failed to write config lock to {}", path.display()))?;

    Ok(())
}

impl SessionConfiguration {
    pub(crate) fn to_config_lockfile_toml(&self) -> anyhow::Result<ConfigLockfileToml> {
        let lock_config = session_configuration_to_lock_config_toml(self)?;
        let config = self.original_config_do_not_use.as_ref();
        let spine_config = config.spine.snapshot().clone();
        Ok(config_lockfile(lock_config, spine_config))
    }
}

fn session_configuration_to_lock_config_toml(
    sc: &SessionConfiguration,
) -> anyhow::Result<ConfigToml> {
    let config = sc.original_config_do_not_use.as_ref();
    // Start from the resolved layer stack, then patch in values that are only
    // known after session setup. Export and replay validation both use this
    // path, so every field here is part of the lockfile contract.
    let mut lock_config: ConfigToml = config
        .config_layer_stack
        .effective_config()
        .try_into()
        .context("failed to deserialize effective config for config lock")?;
    if config.config_lock_save_fields_resolved_from_model_catalog {
        save_session_resolved_fields(sc, &mut lock_config);
    }

    save_config_resolved_fields(config, &mut lock_config)?;
    drop_lockfile_inputs(&mut lock_config);
    // Apply exact managed values last so cleanup cannot discard their runtime-effective values.
    config
        .config_layer_stack
        .requirements_toml()
        .apply_exact_to_config(&mut lock_config);

    Ok(lock_config)
}

/// Saves values chosen during session construction from the model catalog,
/// collaboration mode, and resolved prompt setup.
///
/// These values are not always present in the raw layer stack, so copy them
/// from the live session when the lockfile should be fully self-contained.
fn save_session_resolved_fields(sc: &SessionConfiguration, lock_config: &mut ConfigToml) {
    lock_config.model = Some(sc.step_settings.collaboration_mode.model().to_string());
    lock_config.model_reasoning_effort = sc.step_settings.collaboration_mode.reasoning_effort();
    lock_config.model_reasoning_summary = sc.step_settings.reasoning_summary;
    lock_config.service_tier = sc.step_settings.service_tier.clone();
    lock_config.instructions = Some(sc.base_instructions.clone());
    lock_config.developer_instructions = sc.developer_instructions.clone();
    lock_config.compact_prompt = sc.original_config_do_not_use.compact_prompt.clone();
    lock_config.personality = sc.step_settings.personality;
    lock_config.approval_policy = Some(sc.step_settings.approval_policy.value());
    lock_config.approvals_reviewer = Some(sc.step_settings.approvals_reviewer);
}

/// Saves values stored on `Config` after higher-level resolution,
/// normalization, defaulting, or feature materialization.
///
/// Persist the resolved representation so replay compares against the behavior
/// Codex actually ran with, not only the user-authored TOML inputs.
fn save_config_resolved_fields(
    config: &Config,
    lock_config: &mut ConfigToml,
) -> anyhow::Result<()> {
    lock_config.web_search = Some(config.web_search_mode.value());
    lock_config.model_provider = Some(config.model_provider_id.clone());
    lock_config.plan_mode_reasoning_effort = config.plan_mode_reasoning_effort.clone();
    lock_config.model_verbosity = config.model_verbosity;
    lock_config.include_permissions_instructions = Some(config.include_permissions_instructions);
    lock_config.include_apps_instructions = Some(config.include_apps_instructions);
    lock_config.include_collaboration_mode_instructions =
        Some(config.include_collaboration_mode_instructions);
    lock_config.include_environment_context = Some(config.include_environment_context);
    lock_config.background_terminal_max_timeout = Some(config.background_terminal_max_timeout);

    // Feature aliases and feature configs need to be written in their resolved
    // form; otherwise replay can drift when a legacy key maps to the same
    // runtime feature.
    let features = lock_config
        .features
        .get_or_insert_with(FeaturesToml::default);
    let mut feature_values =
        toml::Value::try_from(&*features).context("serialize snapshot feature settings")?;
    let toml::Value::Table(feature_table) = &mut feature_values else {
        unreachable!("FeaturesToml serializes as a TOML table");
    };
    feature_table.remove("apps_mcp_path_override");
    for spec in codex_features::FEATURES
        .iter()
        .filter(|spec| spec.stage != codex_features::Stage::Removed)
    {
        let enabled = toml::Value::Boolean(config.features.enabled(spec.id));
        match feature_table.get_mut(spec.key) {
            Some(toml::Value::Table(settings)) => {
                settings.insert("enabled".to_string(), enabled);
            }
            _ => {
                feature_table.insert(spec.key.to_string(), enabled);
            }
        }
    }
    *features = feature_values
        .try_into()
        .context("deserialize snapshot feature settings")?;
    if config.tool_registry.error_on_tool_collisions || features.tool_registry.is_some() {
        features.tool_registry = Some(ToolRegistryConfigToml {
            error_on_tool_collisions: Some(config.tool_registry.error_on_tool_collisions),
            turn_metadata_includes_tool_info: Some(
                config.tool_registry.turn_metadata_includes_tool_info,
            ),
        });
    }
    let mut multi_agent_v2: MultiAgentV2ConfigToml =
        resolved_config_to_toml(&config.multi_agent_v2, "features.multi_agent_v2")?;
    multi_agent_v2.enabled = Some(config.features.enabled(Feature::MultiAgentV2));
    features.multi_agent_v2 = Some(FeatureToml::Config(multi_agent_v2));
    if let Some(token_budget) = config.token_budget.as_ref()
        && super::token_budget::has_explicit_settings(config)
    {
        let mut token_budget: TokenBudgetConfigToml =
            resolved_config_to_toml(token_budget, "features.token_budget")?;
        token_budget.enabled = Some(config.features.enabled(Feature::TokenBudget));
        features.token_budget = Some(FeatureToml::Config(token_budget));
    }
    if let Some(rollout_budget) = config.rollout_budget.as_ref() {
        let mut rollout_budget: RolloutBudgetConfigToml =
            resolved_config_to_toml(rollout_budget, "features.rollout_budget")?;
        rollout_budget.enabled = Some(config.features.enabled(Feature::RolloutBudget));
        features.rollout_budget = Some(FeatureToml::Config(rollout_budget));
    }
    if let Some(current_time_reminder) = config.current_time_reminder.as_ref() {
        let mut current_time_reminder: CurrentTimeReminderConfigToml =
            resolved_config_to_toml(current_time_reminder, "features.current_time_reminder")?;
        current_time_reminder.enabled = Some(config.features.enabled(Feature::CurrentTimeReminder));
        features.current_time_reminder = Some(FeatureToml::Config(current_time_reminder));
    }
    lock_config.spine_spawn = Some(SpineSpawnConfigToml {
        max_concurrent_threads_per_session: Some(
            config.spine_spawn.max_concurrent_threads_per_session,
        ),
    });
    lock_config.memories = Some(resolved_config_to_toml::<MemoriesToml>(
        &config.memories,
        "memories",
    )?);

    let agents = lock_config.agents.get_or_insert_with(Default::default);
    agents.enabled = Some(config.agents_enabled);
    agents.max_concurrent_threads_per_session = config.agent_max_threads;
    agents.max_depth = Some(config.agent_max_depth);
    agents.default_subagent_model = config.agent_default_subagent_model.clone();
    agents.default_subagent_reasoning_effort =
        config.agent_default_subagent_reasoning_effort.clone();
    agents.interrupt_message = Some(config.agent_interrupt_message_enabled);

    lock_config
        .skills
        .get_or_insert_with(Default::default)
        .include_instructions = Some(config.include_skill_instructions);
    lock_config
        .orchestrator
        .get_or_insert_with(OrchestratorToml::default)
        .skills
        .get_or_insert_with(OrchestratorFeatureToml::default)
        .enabled = Some(config.orchestrator_skills_enabled);
    lock_config
        .orchestrator
        .get_or_insert_with(OrchestratorToml::default)
        .mcp
        .get_or_insert_with(Default::default)
        .enabled = Some(config.orchestrator_mcp_enabled);

    Ok(())
}

fn drop_lockfile_inputs(lock_config: &mut ConfigToml) {
    // The lockfile should contain replayable values, not the profile,
    // debug-control, file-include, and environment-specific inputs that
    // produced those values in the original session.
    lock_config.profile = None;
    lock_config.profiles.clear();
    clear_config_lock_debug_controls(lock_config);
    lock_config.model_instructions_file = None;
    lock_config.experimental_compact_prompt_file = None;
    lock_config.model_catalog_json = None;
    lock_config.sandbox_mode = None;
    lock_config.sandbox_workspace_write = None;
    lock_config.default_permissions = None;
    lock_config.permissions = None;
    lock_config.experimental_use_unified_exec_tool = None;
}

fn resolved_config_to_toml<Toml>(
    value: &impl serde::Serialize,
    label: &'static str,
) -> anyhow::Result<Toml>
where
    Toml: serde::de::DeserializeOwned + serde::Serialize,
{
    toml_round_trip(value, label).map_err(anyhow::Error::from)
}

#[cfg(test)]
#[path = "session_snapshot_tests.rs"]
mod tests;
