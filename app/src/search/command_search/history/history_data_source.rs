use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::Local;
use futures_lite::future::yield_now;
use ordered_float::OrderedFloat;
use warp_core::features::FeatureFlag;
use warpui::{AppContext, SingletonEntity};

use super::HistorySearchItem;
use super::atuin;
use super::rank::{self, RankInputs};
use crate::search::async_snapshot_data_source::AsyncSnapshotDataSource;
use crate::search::command_search::searcher::CommandSearchItemAction;
use crate::search::data_source::{Query, QueryResult};
use crate::search::mixer::{BoxFuture, DataSourceRunErrorWrapper};
use crate::settings::AISettings;
use crate::terminal;
use crate::terminal::HistoryEntry;
use crate::terminal::model::session::SessionId;

const CHUNK_SIZE: usize = 512;

pub(crate) struct HistorySnapshot {
    commands: Arc<[Arc<HistoryEntry>]>,
    query_text: String,
    current_session_id: SessionId,
}

/// Creates an async data source for shell history commands.
#[cfg(test)]
pub fn history_data_source(
    commands: Vec<HistoryEntry>,
) -> AsyncSnapshotDataSource<HistorySnapshot, CommandSearchItemAction> {
    let commands: Arc<[Arc<HistoryEntry>]> = commands.into_iter().map(Arc::new).collect();
    AsyncSnapshotDataSource::new(
        move |query: &Query, _app: &AppContext| HistorySnapshot {
            commands: commands.clone(),
            query_text: query.text.clone(),
            current_session_id: SessionId::from(0),
        },
        fuzzy_match_history,
    )
}

/// How long atuin's history is reused before it is read from disk again.
///
/// Reading it is fast but not free, and command search is opened often. A
/// command run in another terminal shows up within this window; one run in Warp
/// is in Warp's own history immediately and does not wait on this.
const ATUIN_CACHE_TTL: Duration = Duration::from_secs(30);

/// Cached result of the last read of atuin's history.
///
/// `None` inside the option means atuin is not set up on this machine, which is
/// cached like any other answer so that a missing database is not looked for on
/// every keystroke.
static ATUIN_ENTRIES: Mutex<Option<(Instant, Option<Arc<[Arc<HistoryEntry>]>>)>> = Mutex::new(None);

/// Creates a data source over atuin's history, if atuin is set up on this
/// machine.
///
/// This is a second source rather than something merged into the session's
/// history because the two answer different questions: Warp's history is what
/// this machine ran in Warp, atuin's is everything the user's shells have run,
/// including on other machines.
pub(crate) fn atuin_history_data_source()
-> Option<AsyncSnapshotDataSource<HistorySnapshot, CommandSearchItemAction>> {
    let mut cached = ATUIN_ENTRIES.lock().unwrap_or_else(|err| err.into_inner());

    let is_stale = cached
        .as_ref()
        .is_none_or(|(loaded_at, _)| loaded_at.elapsed() >= ATUIN_CACHE_TTL);
    if is_stale {
        *cached = Some((Instant::now(), atuin::load_shared_entries()));
    }

    let commands = cached.as_ref().and_then(|(_, entries)| entries.clone())?;
    Some(AsyncSnapshotDataSource::new(
        move |query: &Query, _app: &AppContext| HistorySnapshot {
            // Historical commands are all stored as Arcs (with COW semantics and
            // very infrequent writes), so cloning them for the async sort is a
            // negligible cost.
            commands: commands.clone(),
            query_text: query.text.clone(),
            // Atuin entries come from the user's shells rather than a Warp
            // session, so none of them can match the current one. Session 0 is
            // never a live session, which leaves ranking's same-session boost
            // switched off for this source.
            current_session_id: SessionId::from(0),
        },
        fuzzy_match_history,
    ))
}

pub(crate) fn history_data_source_for_session(
    session_id: SessionId,
) -> AsyncSnapshotDataSource<HistorySnapshot, CommandSearchItemAction> {
    AsyncSnapshotDataSource::new(
        move |query: &Query, app: &AppContext| {
            let include_agent_commands = *AISettings::as_ref(app).include_agent_commands_in_history;
            let commands: Arc<[Arc<HistoryEntry>]> = terminal::History::as_ref(app)
                .commands_shared(session_id)
                .unwrap_or_default()
                .into_iter()
                .filter(|entry| include_agent_commands || !entry.is_agent_executed)
                .collect();
            HistorySnapshot {
                commands,
                query_text: query.text.clone(),
                current_session_id: session_id,
            }
        },
        fuzzy_match_history,
    )
}

pub(crate) fn fuzzy_match_history(
    snapshot: HistorySnapshot,
) -> BoxFuture<'static, Result<Vec<QueryResult<CommandSearchItemAction>>, DataSourceRunErrorWrapper>>
{
    if !FeatureFlag::HistorySearchRankingV2.is_enabled() {
        return fuzzy_match_history_legacy(snapshot);
    }

    Box::pin(async move {
        let mut results = Vec::new();
        let now = Local::now();
        let is_blank_query = snapshot.query_text.trim().is_empty();
        let tokens = rank::tokenize_query(&snapshot.query_text);

        for chunk in snapshot.commands.chunks(CHUNK_SIZE) {
            for entry in chunk {
                let Some((match_result, match_quality)) =
                    rank::match_history_command(entry.command.as_str(), &tokens)
                else {
                    continue;
                };

                let Some(score) = rank::rank(RankInputs {
                    entry: entry.as_ref(),
                    match_quality,
                    now,
                    current_session_id: snapshot.current_session_id,
                    is_blank_query,
                }) else {
                    continue;
                };

                results.push(
                    HistorySearchItem {
                        entry: entry.clone(),
                        match_result,
                        score,
                    }
                    .into(),
                );
            }
            yield_now().await;
        }

        Ok(results)
    })
}

fn fuzzy_match_history_legacy(
    snapshot: HistorySnapshot,
) -> BoxFuture<'static, Result<Vec<QueryResult<CommandSearchItemAction>>, DataSourceRunErrorWrapper>>
{
    Box::pin(async move {
        let mut results = Vec::new();

        for chunk in snapshot.commands.chunks(CHUNK_SIZE) {
            for entry in chunk {
                let Some(match_result) = fuzzy_match::match_indices_case_insensitive(
                    entry.command.as_str(),
                    snapshot.query_text.as_str(),
                ) else {
                    continue;
                };
                let score = OrderedFloat(match_result.score as f64);

                results.push(
                    HistorySearchItem {
                        entry: entry.clone(),
                        match_result,
                        score,
                    }
                    .into(),
                );
            }
            yield_now().await;
        }

        Ok(results)
    })
}
