//! Persist and install the history and Spine window as one checkpoint.
use super::*;

impl Session {
    pub(crate) async fn replace_compacted_history(
        &self,
        mut items: Vec<ResponseItemEnvelope>,
        reference_context_item: Option<TurnContextItem>,
        world_state_baseline: Option<Arc<WorldState>>,
        metadata: CompactedHistoryMetadata,
    ) -> anyhow::Result<()> {
        for envelope in &mut items {
            Self::assign_missing_response_item_id(&mut envelope.item);
        }
        let mut compacted_item = CompactedItem {
            message: metadata.message,
            replacement_history: Some(items.clone()),
            guardian_history: None,
            mcp_resource_origins: self.services.mcp_runtime.resource_origin_checkpoint(),
            window_number: Some(metadata.window_number),
            first_window_id: Some(metadata.window_ids.first_window_id.to_string()),
            previous_window_id: metadata
                .window_ids
                .previous_window_id
                .map(|id| id.to_string()),
            window_id: Some(metadata.window_ids.window_id.to_string()),
            compaction_response_id: metadata.compaction_response_id,
            latest_token_usage_record: self.state.lock().await.latest_token_usage_record.clone(),
        };
        // Wait for accepted updates to finish persisting, then keep later updates from
        // overtaking the current settings snapshot while its checkpoint is written.
        let _settings_guard = thread_settings::acquire_persistence_lock(self).await;
        // Build the checkpoint without publishing the replacement history or window.
        let guardian_history = {
            let state = self.state.lock().await;
            let mut candidate = state.history.clone();
            candidate.replace_compacted(items.clone());
            candidate.guardian_history_checkpoint()
        };
        compacted_item.guardian_history = guardian_history;
        let snapshot = world_state_baseline.map(|world_state| world_state.snapshot());
        let mut rollout_items = vec![RolloutItem::Compacted(compacted_item)];
        if let Some(snapshot) = &snapshot {
            rollout_items.push(RolloutItem::WorldState(WorldStateItem::full(
                snapshot.clone().into_object(),
            )));
        }
        if let Some(turn_context_item) = &reference_context_item {
            rollout_items.push(RolloutItem::TurnContext(turn_context_item.clone()));
        }
        rollout_items.push(RolloutItem::EventMsg(
            thread_settings::applied_event(self).await,
        ));

        // Keep readers on the previous window until its successor is durable.
        let mut state = self.state.lock().await;
        let prepared = state
            .prepare_spine_compact(&items)
            .map_err(anyhow::Error::msg)?;
        self.persist_spine_rollout_items(&rollout_items)
            .await
            .map_err(|error| self.latch_spine_error(error))?;
        if let Some(prepared) = prepared {
            state.install_spine_compact(prepared, &items);
        }
        state.replace_annotated_history(
            items,
            reference_context_item,
            HistoryReplacement::Compaction,
        );
        state.install_auto_compact_window(metadata.window_number, metadata.window_ids);
        if let Some(snapshot) = snapshot {
            state.history.set_world_state_baseline(snapshot);
        }
        state.publish_spine_compact();
        state.queue_pending_session_start_source(codex_hooks::SessionStartSource::Compact);
        Ok(())
    }
}
