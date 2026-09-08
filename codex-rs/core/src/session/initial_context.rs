//! Build initial model context using the window being installed.
use super::*;

impl Session {
    pub(crate) async fn build_initial_context_with_world_state(
        &self,
        turn_context: &TurnContext,
        world_state: &WorldState,
    ) -> Vec<ResponseItem> {
        let window_ids = self.state.lock().await.auto_compact_window_ids();
        self.build_initial_context_for_window(turn_context, world_state, window_ids)
            .await
    }

    pub(super) async fn build_initial_context_for_window(
        &self,
        turn_context: &TurnContext,
        world_state: &WorldState,
        auto_compact_window_ids: AutoCompactWindowIds,
    ) -> Vec<ResponseItem> {
        let mut developer_sections = Vec::<RenderedFragment>::with_capacity(8);
        let mut contextual_user_sections = Vec::<RenderedFragment>::with_capacity(2);
        let mut separate_developer_sections = Vec::<RenderedFragment>::new();
        let mut context_window_hints = Vec::new();
        let session_source = self
            .state
            .lock()
            .await
            .session_configuration
            .session_source
            .clone();
        let separate_guardian_developer_message =
            crate::guardian::is_basic_session_source(&session_source);
        // Keep the guardian policy prompt out of the aggregated developer bundle so it
        // stays isolated as its own top-level developer message for guardian subagents.
        if !separate_guardian_developer_message
            && let Some(developer_instructions) = turn_context.developer_instructions.as_deref()
            && !developer_instructions.is_empty()
        {
            developer_sections
                .push(DeveloperInstructions::new(developer_instructions).render_fragment());
        }
        let loaded_plugins = self
            .services
            .plugins_manager
            .plugins_for_config(&turn_context.config.plugins_config_input())
            .await;
        let recommended_plugin_candidates = if turn_context
            .config
            .features
            .plugin_recommendations_enabled()
        {
            let auth = self.services.auth_manager.auth().await;
            let plugins_config = turn_context.config.plugins_config_input();
            self.services
                .plugins_manager
                .recommended_plugin_candidates_for_config(RecommendedPluginCandidatesInput {
                    plugins_config: &plugins_config,
                    loaded_plugins: &loaded_plugins,
                    auth: auth.as_ref(),
                    disabled_tools: &turn_context.config.tool_suggest.disabled_tools,
                    app_server_client_name: turn_context.app_server_client_name.as_deref(),
                })
                .await
        } else {
            None
        };
        if let Some(recommended_plugins) = recommended_plugin_candidates
            .as_deref()
            .and_then(RecommendedPluginsInstructions::from_plugins)
        {
            contextual_user_sections.push(recommended_plugins.render_fragment());
        }
        let context_contributors = self.services.extensions.context_contributors().to_vec();
        for contributor in &context_contributors {
            for fragment in contributor
                .contribute_thread_context(
                    &self.services.session_extension_data,
                    &self.services.thread_extension_data,
                )
                .await
            {
                match fragment.slot() {
                    PromptSlot::ContextWindow => {
                        context_window_hints.push(fragment.text().to_string());
                    }
                    PromptSlot::DeveloperPolicy | PromptSlot::DeveloperCapabilities => {
                        developer_sections.push(fragment.into());
                    }
                }
            }
        }
        for contributor in &context_contributors {
            for fragment in contributor
                .contribute_turn_context(TurnContextContributionInput {
                    thread_id: self.thread_id(),
                    turn_id: turn_context.sub_id.as_str(),
                    session_store: &self.services.session_extension_data,
                    thread_store: &self.services.thread_extension_data,
                    turn_store: turn_context.extension_data.as_ref(),
                    model_context_window: turn_context.model_context_window(),
                })
                .await
            {
                developer_sections.push(fragment.into());
            }
        }
        // This is full-context metadata. Steady-state context diffs should not re-emit it.
        if turn_context.config.features.enabled(Feature::TokenBudget)
            && turn_context.model_context_window().is_some()
        {
            // Keep the legacy bridge hint when native Notes is disabled. A failed
            // native request must not fall back to the bridge.
            if !turn_context
                .config
                .token_budget
                .as_ref()
                .is_some_and(|config| config.use_history_notes_extension)
                && let Some(mcp_result) = self
                    .services
                    .mcp_runtime
                    .latest_call_tool(
                        "notes",
                        "thread_hint",
                        /*environment_id*/ None,
                        /*arguments*/ None,
                        Some(serde_json::json!({
                            "threadId": self.thread_id().to_string(),
                        })),
                        /*requested_timeout*/ None,
                        /*wait_for_server*/ true,
                    )
                    .await
                    .ok()
                    .and_then(|result| {
                        let text = result
                            .content
                            .iter()
                            .filter_map(|content| {
                                content.get("text").and_then(serde_json::Value::as_str)
                            })
                            .filter(|text| !text.is_empty())
                            .collect::<Vec<_>>()
                            .join("\n");
                        (!text.is_empty()).then_some(text)
                    })
            {
                context_window_hints.push(mcp_result);
            }
            separate_developer_sections.push(
                crate::context::TokenBudgetContext::new(
                    session_source
                        .get_agent_path()
                        .unwrap_or_else(codex_protocol::AgentPath::root),
                    auto_compact_window_ids.first_window_id,
                    auto_compact_window_ids.previous_window_id,
                    auto_compact_window_ids.window_id,
                    (!context_window_hints.is_empty()).then(|| context_window_hints.join("\n")),
                )
                .render_fragment(),
            );
        }
        // Render the active mode after the usage hint so it can override that hint.
        let mut initial_multi_agent_mode = None;
        let mut managed_developer_instructions = None;
        for fragment in world_state.render_full() {
            match fragment.role() {
                "developer"
                    if fragment.markers().0 == ModelSwitchInstructions::type_markers().0 =>
                {
                    // New-model instructions must precede the rest of the developer context.
                    developer_sections.insert(0, fragment.render_fragment());
                }
                "developer" if fragment.markers().0 == MULTI_AGENT_MODE_OPEN_TAG => {
                    initial_multi_agent_mode = Some(fragment);
                }
                "developer"
                    if fragment.markers().0 == ManagedDeveloperInstructions::type_markers().0 =>
                {
                    managed_developer_instructions = Some(fragment);
                }
                "developer"
                    if fragment.markers().0 == MultiAgentRoleInstructions::type_markers().0 =>
                {
                    separate_developer_sections.push(fragment.render_fragment());
                }
                "developer"
                    if fragment.requires_separate_message() && fragment.markers().0.is_empty() =>
                {
                    separate_developer_sections.push(fragment.render_fragment());
                }
                "developer" => developer_sections.push(fragment.render_fragment()),
                "user" => contextual_user_sections.push(fragment.render_fragment()),
                _ => {}
            }
        }

        let mut items = Vec::with_capacity(4);
        if let Some(developer_message) =
            crate::context_manager::updates::build_rendered_message(developer_sections)
        {
            items.push(developer_message);
        }
        for section in separate_developer_sections {
            if let Some(developer_message) =
                crate::context_manager::updates::build_rendered_message(vec![section])
            {
                items.push(developer_message);
            }
        }
        if let Some(initial_multi_agent_mode) = initial_multi_agent_mode
            && let Some(message) = crate::context_manager::updates::build_rendered_message(vec![
                initial_multi_agent_mode.render_fragment(),
            ])
        {
            items.push(message);
        }
        if let Some(contextual_user_message) =
            crate::context_manager::updates::build_rendered_message(contextual_user_sections)
        {
            items.push(contextual_user_message);
        }
        // Emit the guardian policy prompt as a separate developer item so the guardian
        // subagent sees a distinct, easy-to-audit instruction block.
        if separate_guardian_developer_message
            && let Some(developer_instructions) = turn_context.developer_instructions.as_deref()
            && !developer_instructions.is_empty()
            && let Some(guardian_developer_message) =
                crate::context_manager::updates::build_rendered_message(vec![
                    GuardianPolicy::new(developer_instructions).render_fragment(),
                ])
        {
            items.push(guardian_developer_message);
        }
        if let Some(managed_developer_instructions) = managed_developer_instructions
            && let Some(message) = crate::context_manager::updates::build_rendered_message(vec![
                managed_developer_instructions.render_fragment(),
            ])
        {
            items.push(message);
        }
        // New context windows and compaction install these items directly into replacement history.
        for item in &mut items {
            item.set_turn_id_if_missing(&turn_context.sub_id);
        }
        items
    }
}
