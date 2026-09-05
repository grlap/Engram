use super::*;

impl LocalWorkService {
    /// One count/page snapshot for the flat list word, without changing focus
    /// or draining the session's peer delivery cursor. The verb fits its final
    /// presentation (including total and hint) after projecting these rows.
    pub(crate) fn work_catalog_page(
        &self,
        query: &WorkCatalogQuery,
        now: DateTime<Utc>,
    ) -> Result<(Vec<ReadyWorkSummary>, usize, Vec<WorkClaim>), StoreError> {
        let store = self.store_at(now)?;
        let (page, total, claims) =
            store.query_work_catalog_listing(&self.project_id, now, query)?;
        Ok((
            page.items.into_iter().map(ready_work_summary).collect(),
            total,
            claims,
        ))
    }

    /// Returns current focus, ready candidates, and the next bounded project delta.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when work projections cannot be read or the
    /// ambient cursor cannot be advanced.
    pub fn work_next(
        &self,
        limit: u32,
        query: WorkNextQuery,
        now: DateTime<Utc>,
    ) -> Result<WorkNextView, StoreError> {
        self.work_next_with_delivery_token(limit, None, None, query, now)
    }

    /// Executes `work_next` with the opaque capability returned by a prior
    /// staged page. Callers cannot advance a pending cursor without it.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] under the same conditions as [`Self::work_next`],
    /// or when the acknowledgement capability does not bind the pending page.
    pub fn work_next_with_delivery_token(
        &self,
        limit: u32,
        acknowledge_through: Option<i64>,
        acknowledge_token: Option<&str>,
        query: WorkNextQuery,
        now: DateTime<Utc>,
    ) -> Result<WorkNextView, StoreError> {
        self.work_next_internal(
            limit,
            acknowledge_through,
            acknowledge_token,
            query,
            now,
            None,
        )
    }

    /// Builds an agent-rendered view while deferring the memory-signal
    /// acknowledgement until the outer renderer proves that it was delivered.
    pub(crate) fn work_next_for_agent(
        &self,
        limit: u32,
        list_limit: u32,
        query: WorkNextQuery,
        now: DateTime<Utc>,
    ) -> Result<WorkNextView, StoreError> {
        self.work_next_internal(limit, None, None, query, now, Some(list_limit))
    }

    #[allow(
        clippy::too_many_lines,
        reason = "section selection, exact delivery staging, and final byte fitting stay together so cursor advancement is auditable"
    )]
    fn work_next_internal(
        &self,
        limit: u32,
        acknowledge_through: Option<i64>,
        acknowledge_token: Option<&str>,
        query: WorkNextQuery,
        now: DateTime<Utc>,
        agent_list_limit: Option<u32>,
    ) -> Result<WorkNextView, StoreError> {
        let mut store = self.store_at(now)?;
        if let Some(through) = acknowledge_through {
            store.acknowledge_work_session_delivery(
                &self.project_id,
                &self.session_id,
                through,
                acknowledge_token,
                now,
            )?;
        } else if acknowledge_token.is_some() {
            return Err(StoreError::InvalidWork(
                "work delivery token requires acknowledge_through".into(),
            ));
        }
        let sections = selected_work_next_sections(&query.sections);
        let wants_focus = sections.contains(&WorkNextSection::Focus);
        let wants_ready = sections.contains(&WorkNextSection::Ready);
        let wants_catalog = sections.contains(&WorkNextSection::Catalog);
        let wants_changes = sections.contains(&WorkNextSection::Changes);
        let wants_memories = sections.contains(&WorkNextSection::Memories);
        // Validate and read the advisory memory signal before changing the
        // exact work-change delivery state. An advisory refusal must not make
        // an unseen tentative page look delivered on the caller's next try.
        let memory_advertisement = if wants_memories {
            Some(store.project_memory_advertisement_candidate(
                &self.project_id,
                &self.session_id,
                query.context_generation.as_deref(),
            )?)
        } else {
            None
        };
        let project_feed = FeedId::Project(self.project_id.clone());
        if acknowledge_through.is_none() && wants_changes {
            // The page returned by the previous call counts as delivered once
            // this session asks for the next one; an agent never acknowledges.
            let previous = store.work_session_state(&self.project_id, &self.session_id, now)?;
            if let Some(through) = previous.tentative_project_cursor {
                store.acknowledge_work_session_delivery(
                    &self.project_id,
                    &self.session_id,
                    through,
                    previous.tentative_delivery_token.as_deref(),
                    now,
                )?;
            }
        }
        let initial_session = store.work_session_state(&self.project_id, &self.session_id, now)?;
        let mut omissions = Vec::new();
        let (session, changes, delivered_through) = if wants_changes {
            let mut delivery_session = initial_session;
            let mut stage_retries = 0;
            #[cfg(test)]
            let mut stage_hook_used = false;
            loop {
                if let Some(through) = delivery_session.tentative_project_cursor {
                    let payload = store
                        .staged_work_session_delivery_payload(&self.project_id, &self.session_id)?
                        .ok_or_else(|| {
                            StoreError::InvalidWorkProjection(
                                "pending work delivery has no exact staged payload".into(),
                            )
                        })?;
                    let page: StagedWorkChangePage = payload.decode()?;
                    verify_staged_work_change_page(
                        &store,
                        &self.session_id,
                        &project_feed,
                        delivery_session.project_cursor,
                        through,
                        &page,
                    )?;
                    if page.omitted_count > 0 {
                        omissions.push(WorkSectionOmission {
                            section: WorkNextSection::Changes,
                            reason: WorkSectionOmissionReason::Staged,
                            omitted_count: page.omitted_count,
                        });
                    }
                    break (delivery_session, Some(page.changes), through);
                }
                let (focused_root_id, bound_task_id) = work_delivery_boundary(
                    &store,
                    &self.project_id,
                    &self.session_id,
                    delivery_session.focused_work_id,
                )?;
                let entries =
                    store.work_feed_after(&project_feed, delivery_session.project_cursor, limit)?;
                let candidate_count = entries.len();
                let changes = verified_bounded_work_changes(
                    &store,
                    &self.project_id,
                    &self.session_id,
                    focused_root_id,
                    bound_task_id,
                    entries,
                    delivery_session.project_cursor,
                    MAX_CHANGE_SECTION_BYTES,
                )?;
                let selected_through = changes
                    .last()
                    .map_or(delivery_session.project_cursor, |change| {
                        change.entry.position.position
                    });
                let omitted_count = candidate_count - changes.len();
                let payload = CanonicalObject::freeze(&StagedWorkChangePage {
                    schema_version: SCHEMA_VERSION,
                    changes: changes.clone(),
                    omitted_count,
                })?;
                let delivered_entries = changes
                    .iter()
                    .map(|change| change.entry.clone())
                    .collect::<Vec<_>>();
                #[cfg(test)]
                if !stage_hook_used && let Some(hook) = &self.delivery_stage_hook {
                    hook.entered.wait();
                    hook.release.wait();
                    stage_hook_used = true;
                }
                let staged = store.stage_work_session_delivery(
                    &self.project_id,
                    &self.session_id,
                    StageWorkSessionDelivery {
                        expected_confirmed_through: delivery_session.project_cursor,
                        expected_focused_work_id: delivery_session.focused_work_id,
                        expected_bound_task_id: bound_task_id,
                        delivered_through: selected_through,
                        delivered_entries: &delivered_entries,
                        delivery_payload: &payload,
                        now,
                    },
                )?;
                if let Some(staged) = staged {
                    if omitted_count > 0 {
                        omissions.push(WorkSectionOmission {
                            section: WorkNextSection::Changes,
                            reason: WorkSectionOmissionReason::Staged,
                            omitted_count,
                        });
                    }
                    break (staged, Some(changes), selected_through);
                }
                stage_retries += 1;
                if stage_retries >= MAX_DELIVERY_STAGE_RETRIES {
                    return Err(StoreError::InvalidWork(
                        "work delivery basis changed repeatedly; retry work_next".into(),
                    ));
                }
                delivery_session =
                    store.work_session_state(&self.project_id, &self.session_id, now)?;
            }
        } else {
            let confirmed = initial_session.project_cursor;
            (initial_session, None, confirmed)
        };
        #[cfg(test)]
        if let Some(hook) = &self.advisory_read_hook {
            hook.entered.wait();
            hook.release.wait();
        }
        let (focus, ready, catalog, discovery, agent_lists) =
            store.work_read_snapshot(|store| {
                let focus = if wants_focus {
                    // The staged session remains the delivery basis; advisory
                    // focus must bind the session visible inside this read cut.
                    store
                        .work_session_state(&self.project_id, &self.session_id, now)?
                        .focused_work_id
                        .map(|work_id| self.focus_view(store, work_id, true, false, now))
                        .transpose()?
                } else {
                    None
                };
                let ready = if wants_ready {
                    let source = store.ready_work(&self.project_id, now, limit)?;
                    let source_count = source.len();
                    let bounded = bounded_ready_prefix(
                        source.into_iter().map(ready_work_summary).collect(),
                        MAX_READY_SECTION_BYTES,
                    )?;
                    if source_count > bounded.len() {
                        omissions.push(WorkSectionOmission {
                            section: WorkNextSection::Ready,
                            reason: WorkSectionOmissionReason::ByteBudget,
                            omitted_count: source_count - bounded.len(),
                        });
                    }
                    Some(bounded)
                } else {
                    None
                };
                let after = query
                    .after
                    .as_deref()
                    .map(|work_ref| store.resolve_work_ref(&self.project_id, work_ref))
                    .transpose()?
                    .map(|work| work.work_id);
                let catalog = if wants_catalog {
                    let source = store.query_work_catalog(
                        &self.project_id,
                        now,
                        &WorkCatalogQuery {
                            search: query.search,
                            lifecycles: query.lifecycles,
                            availabilities: query.availabilities,
                            blocked_only: query.blocked_only,
                            assigned_to: query.assigned_to,
                            held_by: None,
                            label: query.label,
                            after,
                            limit,
                        },
                    )?;
                    let source_count = source.items.len();
                    let source_next_after = source.next_after;
                    let items = bounded_ready_prefix(
                        source.items.into_iter().map(ready_work_summary).collect(),
                        MAX_CATALOG_SECTION_BYTES,
                    )?;
                    if source_count > items.len() {
                        omissions.push(WorkSectionOmission {
                            section: WorkNextSection::Catalog,
                            reason: WorkSectionOmissionReason::ByteBudget,
                            omitted_count: source_count - items.len(),
                        });
                    }
                    let next_after = if source_count > items.len() {
                        items.last().map(|item| item.work.work_id)
                    } else {
                        source_next_after
                    };
                    Some(WorkCatalogSummaryPage { items, next_after })
                } else {
                    None
                };
                let mut discovery = WorkDiscoveryView::default();
                for (section, assigned) in [
                    (WorkNextSection::Assigned, true),
                    (WorkNextSection::Participated, false),
                ] {
                    if sections.contains(&section) {
                        let page = store.work_discovery(
                            &self.project_id,
                            &self.session_id,
                            &self.actor_id,
                            assigned,
                            now,
                        )?;
                        let rows = page
                            .items
                            .into_iter()
                            .map(|row| discovery_summary(row, &self.session_id, now))
                            .collect();
                        if assigned {
                            discovery.assigned = rows;
                            discovery.assigned_omitted = page.omitted;
                        } else {
                            discovery.participated = rows;
                            discovery.participated_omitted = page.omitted;
                        }
                    }
                }
                let agent_lists = agent_list_limit
                    .map(|limit| self.agent_next_lists(store, limit, now))
                    .transpose()?;
                Ok((focus, ready, catalog, discovery, agent_lists))
            })?;
        let memories = memory_advertisement
            .as_ref()
            .map(|advertisement| ProjectMemorySignal {
                count: advertisement.count,
                changed: advertisement.changed,
            });
        let mut response = WorkNextView {
            session: agent_work_session(&session),
            discovery,
            agent_lists,
            focus,
            ready,
            catalog,
            changes,
            memories,
            delivered_through: wants_changes.then_some(delivered_through),
            delivery_token: wants_changes
                .then(|| session.tentative_delivery_token.clone())
                .flatten(),
            omissions,
            memory_advertisement: None,
        };
        fit_work_next_response(&mut response)?;
        ensure_agent_response_budget(&response, "work_next")?;
        if response.memories.is_some()
            && let Some(advertisement) = memory_advertisement
            && advertisement.changed
        {
            if agent_list_limit.is_some() {
                response.memory_advertisement = Some(advertisement);
            } else {
                acknowledge_project_memory_advertisement_best_effort(
                    &mut store,
                    &self.project_id,
                    &self.session_id,
                    &advertisement,
                );
            }
        }
        Ok(response)
    }

    fn agent_next_lists(
        &self,
        store: &SqliteStore,
        limit: u32,
        now: DateTime<Utc>,
    ) -> Result<WorkAgentNextLists, StoreError> {
        let claims = store.live_work_claims(&self.project_id, now)?;
        let mine = claims
            .iter()
            .filter(|(_, holder, _)| *holder == self.session_id)
            .map(|(id, _, expiry)| (*id, *expiry))
            .collect::<std::collections::HashMap<_, _>>();
        let held = if mine.is_empty() {
            Vec::new()
        } else {
            self.agent_catalog(
                store,
                limit,
                vec![WorkAvailability::Claimed, WorkAvailability::Active],
                now,
            )?
            .into_iter()
            .filter_map(|item| mine.get(&item.work.work_id).map(|expiry| (item, *expiry)))
            .collect()
        };
        let ready = self.agent_catalog(store, limit, vec![WorkAvailability::Ready], now)?;
        Ok(WorkAgentNextLists {
            held,
            ready,
            claims,
        })
    }

    fn agent_catalog(
        &self,
        store: &SqliteStore,
        limit: u32,
        availabilities: Vec<WorkAvailability>,
        now: DateTime<Utc>,
    ) -> Result<Vec<ReadyWorkSummary>, StoreError> {
        let wanted = usize::try_from(limit).unwrap_or(usize::MAX).max(1);
        let mut items = Vec::new();
        let mut query = WorkCatalogQuery {
            lifecycles: vec![WorkLifecycle::Open],
            availabilities,
            ..WorkCatalogQuery::default()
        };
        loop {
            query.limit = u32::try_from(wanted - items.len()).unwrap_or(u32::MAX);
            let page = store.query_work_catalog(&self.project_id, now, &query)?;
            let page_len = page.items.len();
            items.extend(page.items.into_iter().map(ready_work_summary));
            match page.next_after {
                Some(next) if page_len > 0 && items.len() < wanted => {
                    query.after = Some(next);
                }
                _ => break,
            }
        }
        items.truncate(wanted);
        Ok(items)
    }

    /// Acknowledges the exact project-memory advisory candidate retained in an
    /// agent response after its final byte shedding and rendering pass.
    pub(crate) fn acknowledge_work_next_memories(&self, view: &WorkNextView, now: DateTime<Utc>) {
        let Some(advertisement) = &view.memory_advertisement else {
            return;
        };
        let Ok(mut store) = self.store_at(now) else {
            return;
        };
        acknowledge_project_memory_advertisement_best_effort(
            &mut store,
            &self.project_id,
            &self.session_id,
            advertisement,
        );
    }
}

fn discovery_summary(
    row: crate::storage::WorkDiscoveryRow,
    session: &SessionId,
    now: DateTime<Utc>,
) -> WorkDiscoverySummary {
    let holder = row
        .claim
        .as_ref()
        .filter(|claim| claim.state == WorkClaimState::Active && claim.expires_at > now)
        .map_or("unclaimed", |claim| {
            if claim.holder == *session {
                "you"
            } else {
                "another session"
            }
        });
    WorkDiscoverySummary {
        work_ref: row.work.short_ref,
        title: compact_text(&row.work.title),
        holder: holder.into(),
        note: row.note.map(|note| compact_text(&note)),
    }
}

#[cfg(test)]
mod tests;
