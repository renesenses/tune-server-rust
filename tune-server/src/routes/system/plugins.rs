use axum::Json;
use axum::extract::State;
use serde_json::{Value, json};

use crate::state::AppState;

/// Alias historique de la liste `/plugins`, restreint aux fiches héritées de
/// la clef de réglages `plugins`.
///
/// Il lisait cette clef **directement**, donc sans le tri : c'était la seconde
/// porte par laquelle une fiche de l'ère Python revenait, y compris sur un
/// serveur où `/plugins` l'écarte. Les deux passent désormais par
/// [`crate::routes::plugins::fiches_locales_honorables`] — un seul tri, une
/// seule vérité (#2132).
pub(super) async fn list_system_plugins(State(state): State<AppState>) -> Json<Value> {
    let plugins = crate::routes::plugins::fiches_locales_honorables(&state).await;
    Json(json!(plugins))
}
