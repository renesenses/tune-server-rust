//! rc15 (fil 3009, demande de Fabien) — **reculer / avancer de N secondes**.
//!
//! `POST /api/v1/zones/{id}/seek/relative`, corps
//! `{ "delta_ms": -15000, "position_ms": 61234 }` :
//!
//! - `delta_ms` (obligatoire) : le saut, SIGNÉ — négatif recule, positif
//!   avance. Sa valeur absolue va de 1 s à 99 s ([`SAUT_MIN_MS`],
//!   [`SAUT_MAX_MS`]), la plage réglable dans les clients ; hors plage, 400
//!   nommant le champ, et rien n'a bougé.
//! - `position_ms` (facultatif) : la position que JOUE l'onglet. Lue pour une
//!   zone NAVIGATEUR seulement, comme `POST /previous` (fil 1476) : le
//!   serveur n'y interroge aucun appareil. Ailleurs, la position du serveur
//!   fait foi et ce champ est ignoré.
//!
//! La cible est bornée à `[0, durée − 1 s]` ([`cible_du_saut`]) : avancer
//! près de la fin laisse l'épisode sur sa dernière seconde au lieu de le
//! déclarer fini sous le doigt. Elle part ensuite par le MÊME chemin que
//! `POST /seek` ([`super::seek_a_la_position`]) : garde du doublon
//! (ticket 193), confirmation de la sortie, relais aux suiveuses d'un groupe.
//!
//! La route ne regarde pas la SOURCE : c'est le client qui transforme les
//! boutons Précédent / Suivant en Reculer / Avancer pour un épisode de
//! podcast. Un titre de musique garde Précédent et Suivant (contre-épreuve
//! côté client, `transportPodcast.test.ts`).

use super::*;

/// Le plus petit saut accepté, en millisecondes (1 s).
pub(crate) const SAUT_MIN_MS: i64 = 1_000;
/// Le plus grand saut accepté, en millisecondes (99 s).
pub(crate) const SAUT_MAX_MS: i64 = 99_000;
/// Avancer ne mène jamais plus loin que cette marge avant la fin.
const MARGE_DE_FIN_MS: i64 = 1_000;

#[derive(Debug, Deserialize)]
pub(super) struct SautRelatifRequest {
    delta_ms: i64,
    #[serde(default)]
    position_ms: Option<i64>,
}

/// La position ABSOLUE visée par un saut de `delta_ms` depuis `position_ms`,
/// bornée à `[0, duree_ms − 1 s]`. Une durée inconnue (`<= 0`) ne borne que
/// le bas. Fonction pure.
pub(crate) fn cible_du_saut(position_ms: i64, delta_ms: i64, duree_ms: i64) -> u64 {
    let mut cible = position_ms.max(0).saturating_add(delta_ms);
    if duree_ms > 0 {
        cible = cible.min(duree_ms.saturating_sub(MARGE_DE_FIN_MS));
    }
    cible.max(0) as u64
}

pub(super) async fn seek_relatif(
    State(state): State<AppState>,
    Path(zone_id): Path<i64>,
    Json(body): Json<SautRelatifRequest>,
) -> axum::response::Response {
    let amplitude = body.delta_ms.unsigned_abs();
    if !(SAUT_MIN_MS as u64..=SAUT_MAX_MS as u64).contains(&amplitude) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "invalid_delta_ms",
                "detail": format!(
                    "delta_ms must be between {SAUT_MIN_MS} and {SAUT_MAX_MS} in absolute value (got {})",
                    body.delta_ms
                ),
            })),
        )
            .into_response();
    }
    let courant = state.playback.get_state(zone_id).await;
    let zone_navigateur = tune_core::db::zone_repo::ZoneRepo::with_backend(state.backend.clone())
        .get(zone_id)
        .ok()
        .flatten()
        .is_some_and(|z| z.output_type.as_deref() == Some("browser"));
    let depart = position_vue_par_precedent(zone_navigateur, courant.position_ms, body.position_ms);
    let duree_ms = courant
        .now_playing
        .as_ref()
        .map(|np| np.duration_ms)
        .unwrap_or(0);
    let cible = cible_du_saut(depart, body.delta_ms, duree_ms);
    info!(
        zone_id,
        depart,
        delta_ms = body.delta_ms,
        cible,
        "seek_relatif"
    );
    super::seek_a_la_position(&state, zone_id, cible).await
}
