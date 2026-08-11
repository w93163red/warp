use futures_lite::future::yield_now;
use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use warpui::{AppContext, SingletonEntity};

use super::atuin;

use crate::search::async_snapshot_data_source::AsyncSnapshotDataSource;
use crate::search::command_search::searcher::CommandSearchItemAction;
use crate::search::data_source::{Query, QueryResult};
use crate::search::mixer::{BoxFuture, DataSourceRunErrorWrapper};
use crate::settings::AISettings;
use crate::terminal;
use crate::terminal::model::session::SessionId;
use crate::terminal::HistoryEntry;

use super::HistorySearchItem;

pub(crate) struct HistorySnapshot {
    commands: Arc<[Arc<HistoryEntry>]>,
    query_text: String,
}

/// Creates an async data source for shell history commands.
#[cfg(test)]
pub fn history_data_source(
    commands: Vec<HistoryEntry>,
) -> AsyncSnapshotDataSource<HistorySnapshot, CommandSearchItemAction> {
    let commands: Arc<[Arc<HistoryEntry>]> = commands.into_iter().map(Arc::new).collect();
    history_data_source_from_shared(commands)
}

fn history_data_source_from_shared(
    commands: Arc<[Arc<HistoryEntry>]>,
) -> AsyncSnapshotDataSource<HistorySnapshot, CommandSearchItemAction> {
    AsyncSnapshotDataSource::new(
        move |query: &Query, _app: &AppContext| HistorySnapshot {
            // Historical commands are all stored as Arcs (with COW semantics and very infrequent writes),
            // so cloning the commands to pass them in to the async sort function is a negligible cost.
            commands: commands.clone(),
            query_text: query.text.clone(),
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
static ATUIN_ENTRIES: Mutex<Option<(Instant, Option<Arc<[Arc<HistoryEntry>]>>)>> =
    Mutex::new(None);

/// Creates a data source over atuin's history, if atuin is set up on this
/// machine.
///
/// This is a second source rather than something merged into the session's
/// history because the two answer different questions: Warp's history is what
/// this machine ran in Warp, atuin's is everything the user's shells have run,
/// including on other machines.
pub(crate) fn atuin_history_data_source(
) -> Option<AsyncSnapshotDataSource<HistorySnapshot, CommandSearchItemAction>> {
    let mut cached = ATUIN_ENTRIES.lock().unwrap_or_else(|err| err.into_inner());

    let is_stale = cached
        .as_ref()
        .is_none_or(|(loaded_at, _)| loaded_at.elapsed() >= ATUIN_CACHE_TTL);
    if is_stale {
        *cached = Some((Instant::now(), atuin::load_shared_entries()));
    }

    let commands = cached.as_ref().and_then(|(_, entries)| entries.clone())?;
    Some(history_data_source_from_shared(commands))
}

pub(crate) fn history_data_source_for_session(
    session_id: SessionId,
    history_model: &terminal::History,
    app: &AppContext,
) -> AsyncSnapshotDataSource<HistorySnapshot, CommandSearchItemAction> {
    let include_agent_commands = *AISettings::as_ref(app).include_agent_commands_in_history;
    let commands: Arc<[Arc<HistoryEntry>]> = history_model
        .commands_shared(session_id)
        .unwrap_or_default()
        .into_iter()
        .filter(|entry| include_agent_commands || !entry.is_agent_executed)
        .collect();
    history_data_source_from_shared(commands)
}

pub(crate) fn fuzzy_match_history(
    snapshot: HistorySnapshot,
) -> BoxFuture<'static, Result<Vec<QueryResult<CommandSearchItemAction>>, DataSourceRunErrorWrapper>>
{
    Box::pin(async move {
        let mut results = Vec::new();

        // History entries are cheap to match (single short string), so we use a large chunk
        // size to reduce yield overhead while still allowing cancellation of stale queries.
        for chunk in snapshot.commands.chunks(512) {
            for entry in chunk {
                if let Some(match_result) = fuzzy_match::match_indices_case_insensitive(
                    entry.command.as_str(),
                    snapshot.query_text.as_str(),
                ) {
                    results.push(
                        HistorySearchItem {
                            entry: entry.clone(),
                            match_result,
                        }
                        .into(),
                    );
                }
            }
            yield_now().await;
        }

        Ok(results)
    })
}
