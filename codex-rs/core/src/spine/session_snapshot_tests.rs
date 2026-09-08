use super::*;
use crate::config::ConfigBuilder;
use crate::spine::config_snapshot::read_config_lock_from_path;
use codex_config::LoaderOverrides;
use codex_config::test_support::CloudConfigBundleFixture;
use codex_models_manager::bundled_models_response;
use codex_utils_absolute_path::AbsolutePathBuf;
use pretty_assertions::assert_eq;
use std::path::Path;
use std::sync::Arc;

fn write_model_catalog(path: &Path) {
    let mut catalog = bundled_models_response()
        .unwrap_or_else(|err| panic!("bundled models.json should parse: {err}"));
    catalog.models = catalog.models.into_iter().take(1).collect();
    std::fs::write(
        path,
        serde_json::to_string(&catalog).expect("serialize model catalog"),
    )
    .expect("write model catalog");
}

#[tokio::test]
async fn lock_contains_prompts_and_materializes_features() {
    let mut sc = crate::session::tests::make_session_configuration_for_tests().await;
    let mut config = (*sc.original_config_do_not_use).clone();
    config.tool_registry.error_on_tool_collisions = true;
    config.multi_agent_v2.subagent_developer_instructions =
        Some("Locked subagent developer instructions.".to_string());
    config.token_budget = Some(crate::config::TokenBudgetConfig {
        reminder_threshold_tokens: Some(16_000),
        reminder_message_template: "Locked reminder: {n_remaining} tokens.".to_string(),
        guidance_message: Some("Locked context-window guidance.".to_string()),
        auto_compact_fallback_prompt: Some("Write notes before rollover.".to_string()),
        auto_compact_fallback_buffer_tokens: Some(8_000),
        ..Default::default()
    });
    config
        .features
        .enable(Feature::TokenBudget)
        .expect("token_budget should be enableable in tests");
    config.rollout_budget = Some(crate::config::RolloutBudgetConfig {
        limit_tokens: 100_000,
        reminder_at_remaining_tokens: vec![50_000, 25_000, 10_000],
        sampling_token_weight: 1.0,
        prefill_token_weight: 0.25,
    });
    config
        .features
        .enable(Feature::RolloutBudget)
        .expect("rollout_budget should be enableable in tests");
    config.current_time_reminder = Some(crate::config::CurrentTimeReminderConfig::default());
    config
        .features
        .enable(Feature::CurrentTimeReminder)
        .expect("current_time_reminder should be enableable in tests");
    let mut config = config;
    crate::spine::config::restore_sampling_config(&mut config, &codex_history::InitialHistory::New)
        .expect("resolve SDK before constructing session snapshot");
    sc.original_config_do_not_use = Arc::new(config);
    sc.base_instructions = "resolved instructions".to_string();
    sc.developer_instructions = Some("resolved developer instructions".to_string());
    Arc::make_mut(&mut sc.original_config_do_not_use).compact_prompt =
        Some("resolved compact prompt".to_string());

    let lockfile = sc.to_config_lockfile_toml().expect("lock should serialize");
    let lock = &lockfile.config;

    assert_eq!(lock.instructions, Some(sc.base_instructions.clone()));
    assert_eq!(lock.developer_instructions, sc.developer_instructions);
    assert_eq!(
        lock.compact_prompt,
        sc.original_config_do_not_use.compact_prompt
    );
    assert_eq!(
        lock.model,
        Some(sc.step_settings.collaboration_mode.model().to_string())
    );
    assert_eq!(
        lock.model_reasoning_effort,
        sc.step_settings.collaboration_mode.reasoning_effort()
    );
    assert_eq!(lock.profile, None);
    assert!(lock.profiles.is_empty());
    assert!(
        lock.debug
            .as_ref()
            .is_none_or(|debug| debug.config_lockfile.is_none())
    );
    assert!(lock.memories.is_some());
    let features = lock
        .features
        .as_ref()
        .expect("lock should materialize feature states");
    assert_eq!(
        features.tool_registry,
        Some(ToolRegistryConfigToml {
            error_on_tool_collisions: Some(true),
            turn_metadata_includes_tool_info: Some(
                sc.original_config_do_not_use
                    .tool_registry
                    .turn_metadata_includes_tool_info
            ),
        })
    );
    let feature_entries = features.entries();
    for spec in codex_features::FEATURES
        .iter()
        .filter(|spec| spec.stage != codex_features::Stage::Removed)
    {
        assert_eq!(
            feature_entries.get(spec.key),
            Some(&sc.original_config_do_not_use.features.enabled(spec.id)),
            "{}",
            spec.key
        );
    }
    assert_eq!(
        features.code_mode,
        Some(FeatureToml::Enabled(
            sc.original_config_do_not_use
                .features
                .enabled(Feature::CodeMode)
        ))
    );

    let multi_agent_v2 = features
        .multi_agent_v2
        .as_ref()
        .expect("multi_agent_v2 config should be materialized");
    assert!(matches!(
        multi_agent_v2,
        FeatureToml::Config(MultiAgentV2ConfigToml {
            enabled: Some(false),
            max_concurrent_threads_per_session: Some(_),
            min_wait_timeout_ms: Some(_),
            max_wait_timeout_ms: Some(_),
            default_wait_timeout_ms: Some(_),
            subagent_developer_instructions: Some(instructions),
            hide_spawn_agent_metadata: Some(_),
            ..
        }) if instructions == "Locked subagent developer instructions."
    ));

    assert_eq!(
        features.token_budget,
        Some(FeatureToml::Config(TokenBudgetConfigToml {
            enabled: Some(true),
            use_history_notes_extension: Some(false),
            reminder_threshold_tokens: Some(16_000),
            reminder_message_template: Some("Locked reminder: {n_remaining} tokens.".to_string()),
            guidance_message: Some("Locked context-window guidance.".to_string()),
            auto_compact_fallback_prompt: Some("Write notes before rollover.".to_string()),
            auto_compact_fallback_buffer_tokens: Some(8_000),
        }))
    );

    assert_eq!(
        features.rollout_budget,
        Some(FeatureToml::Config(RolloutBudgetConfigToml {
            enabled: Some(true),
            limit_tokens: Some(100_000),
            reminder_at_remaining_tokens: Some(vec![50_000, 25_000, 10_000]),
            sampling_token_weight: Some(1.0),
            prefill_token_weight: Some(0.25),
        }))
    );
    assert_eq!(
        features.current_time_reminder,
        Some(FeatureToml::Config(CurrentTimeReminderConfigToml {
            enabled: Some(true),
            reminder_interval_seconds: Some(1),
            clock_source: Some(codex_features::CurrentTimeSource::System),
            delivery_mode: Some(codex_features::CurrentTimeReminderDeliveryMode::AnyInference),
            sleep_tool: Some(false),
        }))
    );

    assert_eq!(
        lockfile.version,
        crate::spine::config_snapshot::CONFIG_LOCK_VERSION
    );
}

#[tokio::test]
async fn lock_preserves_model_owned_token_budget_defaults() {
    let mut sc = crate::session::tests::make_session_configuration_for_tests().await;
    let mut config = (*sc.original_config_do_not_use).clone();
    config.token_budget = Some(crate::config::TokenBudgetConfig::default());
    config
        .features
        .enable(Feature::TokenBudget)
        .expect("token_budget should be enableable in tests");
    let mut config = config;
    crate::spine::config::restore_sampling_config(&mut config, &codex_history::InitialHistory::New)
        .expect("resolve SDK before constructing session snapshot");
    sc.original_config_do_not_use = Arc::new(config);

    let lockfile = sc.to_config_lockfile_toml().expect("lock should serialize");
    let features = lockfile
        .config
        .features
        .as_ref()
        .expect("lock should materialize feature states");

    assert_eq!(
        features.token_budget.as_ref(),
        Some(&FeatureToml::Enabled(true))
    );
    assert_eq!(features.tool_registry, None);
}

#[tokio::test]
async fn lock_skips_session_values_when_model_catalog_fields_are_not_saved() {
    let mut sc = crate::session::tests::make_session_configuration_for_tests().await;
    let mut config = (*sc.original_config_do_not_use).clone();
    config.config_lock_save_fields_resolved_from_model_catalog = false;
    let mut config = config;
    crate::spine::config::restore_sampling_config(&mut config, &codex_history::InitialHistory::New)
        .expect("resolve SDK before constructing session snapshot");
    sc.original_config_do_not_use = Arc::new(config);
    sc.base_instructions = "catalog instructions".to_string();
    sc.developer_instructions = Some("catalog developer instructions".to_string());
    Arc::make_mut(&mut sc.original_config_do_not_use).compact_prompt =
        Some("catalog compact prompt".to_string());
    Arc::make_mut(&mut sc.step_settings).service_tier = Some("flex".to_string());

    let lockfile = sc.to_config_lockfile_toml().expect("lock should serialize");
    let lock = &lockfile.config;

    assert_eq!(lock.model, None);
    assert_eq!(lock.model_reasoning_effort, None);
    assert_eq!(lock.model_reasoning_summary, None);
    assert_eq!(lock.service_tier, None);
    assert_eq!(lock.instructions, None);
    assert_eq!(lock.developer_instructions, None);
    assert_eq!(lock.compact_prompt, None);
    assert_eq!(lock.personality, None);
    assert_eq!(lock.approval_policy, None);
    assert_eq!(lock.approvals_reviewer, None);
}

#[tokio::test]
async fn lock_contains_exact_managed_requirements() {
    let codex_home = tempfile::tempdir().expect("create temp dir");
    let sqlite_home = codex_home.path().join("managed-state");
    let log_dir = codex_home.path().join("managed-logs");
    let catalog_path = codex_home.path().join("managed-models.json");
    write_model_catalog(&catalog_path);
    let requirements = format!(
        r#"
sqlite_home = {:?}
log_dir = {:?}
model_catalog_json = {:?}
check_for_update_on_startup = false
allow_login_shell = false

[feedback]
enabled = false

[windows]
sandbox_private_desktop = false
"#,
        sqlite_home.display(),
        log_dir.display(),
        catalog_path.display(),
    );
    let mut config = crate::config::ConfigBuilder::without_managed_config_for_tests()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(codex_home.path().to_path_buf()))
        .cloud_config_bundle(
            CloudConfigBundleFixture::loader_with_enterprise_requirement(requirements),
        )
        .build()
        .await
        .expect("config should load");
    config.config_lock_save_fields_resolved_from_model_catalog = false;
    let mut sc = crate::session::tests::make_session_configuration_for_tests().await;
    let mut config = config;
    crate::spine::config::restore_sampling_config(&mut config, &codex_history::InitialHistory::New)
        .expect("resolve SDK before constructing session snapshot");
    sc.original_config_do_not_use = Arc::new(config);

    let lockfile = sc.to_config_lockfile_toml().expect("lock should serialize");
    let lock = &lockfile.config;

    assert_eq!(lock.sqlite_home.as_deref(), Some(sqlite_home.as_path()));
    assert_eq!(lock.log_dir.as_deref(), Some(log_dir.as_path()));
    assert_eq!(
        lock.model_catalog_json.as_deref(),
        Some(catalog_path.as_path())
    );
    assert_eq!(lock.check_for_update_on_startup, Some(false));
    assert_eq!(lock.allow_login_shell, Some(false));
    assert_eq!(
        lock.feedback.as_ref().and_then(|feedback| feedback.enabled),
        Some(false)
    );
    assert_eq!(
        lock.windows
            .as_ref()
            .and_then(|windows| windows.sandbox_private_desktop),
        Some(false)
    );
}

#[tokio::test]
async fn lock_drops_unmanaged_model_catalog_input() {
    let codex_home = tempfile::tempdir().expect("create temp dir");
    let catalog_path = codex_home.path().join("user-models.json");
    write_model_catalog(&catalog_path);
    let config = crate::config::ConfigBuilder::without_managed_config_for_tests()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(codex_home.path().to_path_buf()))
        .cli_overrides(vec![(
            "model_catalog_json".to_string(),
            toml::Value::String(catalog_path.display().to_string()),
        )])
        .build()
        .await
        .expect("config should load");
    let mut sc = crate::session::tests::make_session_configuration_for_tests().await;
    let mut config = config;
    crate::spine::config::restore_sampling_config(&mut config, &codex_history::InitialHistory::New)
        .expect("resolve SDK before constructing session snapshot");
    sc.original_config_do_not_use = Arc::new(config);

    let lockfile = sc.to_config_lockfile_toml().expect("lock should serialize");

    assert_eq!(lockfile.config.model_catalog_json, None);
}

fn config_lock_for_sdk_layers(
    working_directory: &Path,
    home_directory: Option<&Path>,
    explicit_path: Option<&Path>,
    project_config_trusted: bool,
) -> std::io::Result<ConfigLockfileToml> {
    let explicit_path = explicit_path
        .map(|path| AbsolutePathBuf::try_from(path.to_path_buf()))
        .transpose()?;
    let mut config: ConfigToml = toml::from_str("").expect("empty parsed config");
    config.spine_config_file = explicit_path.clone();
    let resolved = crate::config::SpineConfiguration::pending(
        explicit_path.as_ref(),
        None,
        working_directory,
        home_directory,
        project_config_trusted,
    )
    .resolve(None, &crate::config::ManagedFeatures::default())
    .map_err(std::io::Error::other)?;
    let spine_config = resolved.snapshot().clone();
    Ok(crate::spine::config_snapshot::config_lockfile(
        config,
        spine_config,
    ))
}

async fn write_and_read_lock(
    lock_path: &Path,
    lock: &ConfigLockfileToml,
) -> std::io::Result<ConfigLockfileToml> {
    std::fs::write(
        lock_path,
        toml::to_string(lock).expect("serialize config lock"),
    )?;
    read_config_lock_from_path(&AbsolutePathBuf::try_from(lock_path.to_path_buf())?).await
}

#[tokio::test]
async fn snapshot_survives_source_mutation_deletion_and_appearance() -> std::io::Result<()> {
    let temp = tempfile::tempdir()?;
    let home = temp.path().join("home");
    let working = temp.path().join("work");
    std::fs::create_dir_all(home.join(".spine"))?;
    std::fs::create_dir_all(&working)?;
    let home_source = home.join(".spine/spine.toml");
    let cwd_source = working.join("spine.toml");
    std::fs::write(&home_source, "[prompt]\nnode = \"home-v1\"\n")?;
    let lock = config_lock_for_sdk_layers(&working, Some(&home), None, true)?;
    let lock_path = temp.path().join("config-lock.toml");

    std::fs::write(&home_source, "[prompt]\nnode = \"home-v2\"\n")?;
    assert_eq!(write_and_read_lock(&lock_path, &lock).await?, lock);

    std::fs::write(&home_source, "[prompt]\nnode = \"home-v1\"\n")?;
    std::fs::write(&cwd_source, "[prompt]\nnode = \"cwd\"\n")?;
    assert_eq!(write_and_read_lock(&lock_path, &lock).await?, lock);

    let relocated = temp.path().join("relocated-cwd-spine.toml");
    std::fs::rename(&cwd_source, &relocated)?;
    std::fs::rename(&home_source, temp.path().join("relocated-home-spine.toml"))?;
    assert_eq!(write_and_read_lock(&lock_path, &lock).await?, lock);
    Ok(())
}

#[tokio::test]
async fn snapshot_preserves_merged_explicit_and_home_layers() -> std::io::Result<()> {
    let temp = tempfile::tempdir()?;
    let home = temp.path().join("home");
    let working = temp.path().join("work");
    std::fs::create_dir_all(home.join(".spine"))?;
    std::fs::create_dir_all(&working)?;
    let home_source = home.join(".spine/spine.toml");
    let explicit_source = temp.path().join("explicit.toml");
    std::fs::write(&home_source, "[prompt]\nnode = \"home-v1\"\n")?;
    std::fs::write(&explicit_source, "[prompt]\nnode = \"explicit node\"\n")?;
    let lock = config_lock_for_sdk_layers(&working, Some(&home), Some(&explicit_source), true)?;
    let lock_path = temp.path().join("config-lock.toml");

    std::fs::write(&home_source, "[prompt]\nnode = \"home-v2\"\n")?;
    assert_eq!(write_and_read_lock(&lock_path, &lock).await?, lock);
    Ok(())
}

#[tokio::test]
async fn untrusted_cwd_sources_are_absent_from_lock_contract() -> std::io::Result<()> {
    let temp = tempfile::tempdir()?;
    let working = temp.path().join("work");
    std::fs::create_dir_all(&working)?;
    let cwd_source = working.join("spine.toml");
    std::fs::write(&cwd_source, "[prompt]\nnode = \"cwd-v1\"\n")?;
    let lock = config_lock_for_sdk_layers(&working, None, None, false)?;
    assert_eq!(
        lock.spine_config
            .as_ref()
            .expect("version 2 lock metadata")
            .sources,
        Vec::new()
    );

    std::fs::write(&cwd_source, "[prompt]\nnode = \"cwd-v2\"\n")?;
    write_and_read_lock(&temp.path().join("config-lock.toml"), &lock)
        .await
        .expect("suppressed untrusted CWD layers must not affect replay");
    Ok(())
}

#[tokio::test]
async fn profile_v2_relative_spine_file_is_resolved_and_pinned() -> std::io::Result<()> {
    let temp = tempfile::tempdir()?;
    let codex_home = temp.path().join("codex-home");
    let profile_directory = temp.path().join("profiles/work");
    let profile_path = profile_directory.join("profile.toml");
    let spine_path = profile_directory.join("sdk/spine.toml");
    std::fs::create_dir_all(spine_path.parent().expect("SDK parent"))?;
    std::fs::create_dir_all(&codex_home)?;
    std::fs::write(&spine_path, "schema_version = 1\n")?;
    std::fs::write(&profile_path, "spine_config_file = \"sdk/spine.toml\"\n")?;
    let config = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(codex_home)
        .loader_overrides(LoaderOverrides {
            user_config_path: Some(AbsolutePathBuf::try_from(profile_path)?),
            user_config_profile: Some("work".parse().expect("profile-v2 name")),
            ..LoaderOverrides::without_managed_config_for_tests()
        })
        .fallback_cwd(Some(temp.path().to_path_buf()))
        .build()
        .await?;
    let mut sc = crate::session::tests::make_session_configuration_for_tests().await;
    let mut config = config;
    crate::spine::config::restore_sampling_config(&mut config, &codex_history::InitialHistory::New)
        .expect("resolve SDK before constructing session snapshot");
    sc.original_config_do_not_use = Arc::new(config);

    let lock = sc.to_config_lockfile_toml().expect("lock should serialize");
    assert_eq!(
        lock.config.spine_config_file,
        Some(AbsolutePathBuf::try_from(spine_path.clone())?)
    );
    assert!(
        lock.spine_config
            .expect("version 2 lock metadata")
            .sources
            .iter()
            .any(|source| source.required && source.path.as_path() == spine_path),
    );
    Ok(())
}

#[tokio::test]
async fn legacy_snapshots_convert_to_self_contained_version_three() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("legacy.toml");
    for version in [1, 2] {
        let mut legacy = config_lock_for_sdk_layers(temp.path(), None, None, false)?;
        legacy.version = version;
        legacy.codex_version = "0.3.3".to_string();
        legacy.config.spine_config_snapshot = None;
        if version == 1 {
            legacy.spine_config = None;
        } else {
            legacy
                .spine_config
                .as_mut()
                .expect("source ledger")
                .effective_config = None;
        }
        let migrated = write_and_read_lock(&path, &legacy).await?;
        let expected = config_lock_for_sdk_layers(temp.path(), None, None, false)?;
        assert_eq!(migrated.config, expected.config);
        assert_eq!(migrated.version, expected.version);
        assert_eq!(migrated.spine_config, expected.spine_config);
    }
    Ok(())
}
