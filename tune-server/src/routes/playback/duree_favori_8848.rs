//! Une durée manquante ne retient pas un ajout direct en file (#8848).
use super::{AppState, PlayQueueRepo, QueueInput, persist_queue_async};
use std::time::Duration;
use tune_core::db::playlist_repo::ServiceEntry;

pub(super) fn completer(state: &AppState, zone_id: i64, inputs: &[QueueInput]) {
    let titres: Vec<ServiceEntry> = inputs
        .iter()
        .filter_map(|entree| {
            if let QueueInput::Streaming {
                source,
                source_id,
                title,
                artist,
                album,
                cover_url,
                duration_ms,
                album_ref,
                ..
            } = entree
                && matches!(source.as_str(), "qobuz" | "tidal")
                && *duration_ms <= 0
            {
                Some(ServiceEntry {
                    source: source.clone(),
                    source_id: source_id.clone(),
                    title: title.clone(),
                    artist: Some(artist.clone()),
                    album: album.clone(),
                    cover_url: cover_url.clone(),
                    album_source_id: album_ref.clone(),
                    duration_ms: Some(*duration_ms),
                })
            } else {
                None
            }
        })
        .collect();
    if titres.is_empty() {
        return;
    }
    let state = state.clone();
    tokio::spawn(async move {
        effectuer(&state, zone_id, titres, Duration::from_secs(30)).await;
    });
}

pub(super) async fn effectuer(
    state: &AppState,
    zone_id: i64,
    mut titres: Vec<ServiceEntry>,
    delai: Duration,
) {
    // Le délai englobe aussi les verrous et la vérification de connexion.
    // Les résultats partiels acquis avant expiration restent utilisables.
    let _ = tokio::time::timeout(
        delai,
        crate::routes::playlists::metadonnees_7738::completer_durees(
            state,
            titres.iter_mut().collect(),
            delai,
        ),
    )
    .await;
    let repo = PlayQueueRepo::with_backend(state.backend.clone());
    let mut completees = 0usize;
    for titre in titres.iter().filter(|t| t.duration_ms.unwrap_or(0) > 0) {
        match repo.completer_duree_de_service(
            zone_id,
            &titre.source,
            &titre.source_id,
            titre.duration_ms.unwrap_or(0),
        ) {
            Ok(n) => completees += n,
            Err(e) => tracing::warn!(zone_id, error = %e, "file_duree_favori_non_reecrite"),
        }
    }
    if completees > 0 {
        persist_queue_async(state, zone_id);
        state.event_bus.emit(
            "playback.queue.updated",
            serde_json::json!({ "zone_id":zone_id, "durations_completed":completees }),
        );
    }
}
