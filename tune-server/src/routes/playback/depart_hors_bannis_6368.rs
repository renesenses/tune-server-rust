//! #6368 — « Lire l'album » jouait le titre banni quand il ouvre l'album
//! (fil 2281, Meddle, titre banni : Seamus).
//!
//! L'enjambée des bannis (`enjamber_les_pistes_bannies`) ne servait qu'à
//! « Suivant », à la fin de piste et à l'assistant ; la mise en file d'un
//! contenant partait de la ligne 0 telle quelle. Désormais, quand le départ
//! n'est PAS choisi (ni `start_index`, ni `track_id`), la lecture part de la
//! première ligne non bannie ; un départ choisi se joue même banni.

use axum::body::Body;
use axum::http::Request;
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::db::hidden_repo::HiddenRepo;
use tune_core::db::play_queue_repo::PlayQueueRepo;
use tune_core::db::zone_repo::ZoneRepo;

/// Profil des sélections automatiques d'une base vierge : le profil global.
const PROFIL: i64 = 1;

struct Banc {
    app: axum::Router,
    state: crate::state::AppState,
    zone_id: i64,
    pistes: Vec<i64>,
}

/// Une zone et trois pistes ; la première est bannie.
fn monter() -> Banc {
    let state = crate::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    let zone_id = ZoneRepo::with_backend(state.backend.clone())
        .create("Salon", Some("local"), None)
        .unwrap();
    let repo = tune_core::db::track_repo::TrackRepo::with_backend(state.backend.clone());
    let mut pistes = Vec::new();
    for (i, titre) in ["Seamus", "Echoes", "One of These Days"].iter().enumerate() {
        let mut piste = tune_core::db::models::Track::new(titre.to_string());
        piste.file_path = Some(format!("/aucun/chemin/6368/piste{i}.flac"));
        piste.track_number = i as i32 + 1;
        piste.duration_ms = 180_000;
        pistes.push(repo.create(&piste).unwrap());
    }
    HiddenRepo::with_backend(state.backend.clone())
        .ban_track(PROFIL, pistes[0])
        .unwrap();
    Banc {
        app: crate::routes::router(state.clone()),
        state,
        zone_id,
        pistes,
    }
}

impl Banc {
    async fn lire(&self, corps: Value) {
        let requete = Request::builder()
            .method("POST")
            .uri(format!("/api/v1/zones/{}/play", self.zone_id))
            .header("Content-Type", "application/json")
            .body(Body::from(corps.to_string()))
            .unwrap();
        // La zone n'a pas d'appareil : la lecture elle-même échoue, et ce
        // n'est pas l'objet du banc. La file et son curseur sont écrits avant.
        let _ = self.app.clone().oneshot(requete).await.unwrap();
    }

    fn piste_courante(&self) -> Option<i64> {
        PlayQueueRepo::with_backend(self.state.backend.clone())
            .get_ordered(self.zone_id)
            .unwrap()
            .into_iter()
            .find(|e| e.is_current)
            .and_then(|e| e.track_id)
    }
}

/// 🔴 Le défaut : un contenant lu « depuis le début » part de la première
/// ligne NON bannie.
#[tokio::test]
async fn lire_le_contenant_enjambe_le_banni_qui_l_ouvre() {
    let banc = monter();
    banc.lire(json!({ "track_ids": banc.pistes })).await;
    assert_eq!(
        banc.piste_courante(),
        Some(banc.pistes[1]),
        "la première piste ({}) est bannie : la lecture de l'album doit partir \
         de la suivante ({}), comme « Suivant » et la fin de piste",
        banc.pistes[0],
        banc.pistes[1]
    );
}

/// Un départ CHOISI se joue même banni : la règle de #4806 tient.
#[tokio::test]
async fn un_depart_choisi_se_joue_meme_banni() {
    let banc = monter();
    banc.lire(json!({ "track_ids": banc.pistes, "start_index": 0 }))
        .await;
    assert_eq!(banc.piste_courante(), Some(banc.pistes[0]), "start_index");

    let banc = monter();
    banc.lire(json!({ "track_ids": banc.pistes, "track_id": banc.pistes[0] }))
        .await;
    assert_eq!(banc.piste_courante(), Some(banc.pistes[0]), "track_id");
}
