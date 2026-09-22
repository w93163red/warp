use uuid::Uuid;
use warp_core::user_preferences::GetUserPreferences;

/// Returns an in-memory anonymous id for compatibility with code paths that still expect one.
///
/// This value is intentionally not persisted, so it cannot act as a stable cross-session user
/// identifier in offline builds.
pub fn get_or_create_anonymous_id(_ctx: &dyn GetUserPreferences) -> Uuid {
    Uuid::new_v4()
}
