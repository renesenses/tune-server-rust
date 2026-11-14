//! Les M3U (y compris ceux du scan) ne portent que l'identité Qobuz.
//! Compléter à la lecture répare aussi les playlists déjà importées.
use crate::state::AppState;
use futures_util::{StreamExt, stream};
use serde_json::Value;
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tune_core::streaming::traits::StreamTrack;

#[derive(Default)]
pub(crate) struct Cache(Mutex<HashMap<String, (Instant, StreamTrack)>>);

pub(super) async fn completer(state: &AppState, items: &mut [Value]) {
    completer_service(state, items, "qobuz").await;
    // #8066 — TIDAL : seule la disponibilité est reportée sur la ligne, pour
    // que le client grise un titre indisponible (`disponible: false`) et
    // propose « Remplacer l'indisponible ». Le reste de la ligne ne change
    // pas.
    completer_service(state, items, "tidal").await;
}

async fn completer_service(state: &AppState, items: &mut [Value], nom: &'static str) {
    if !items.iter().any(|v| v["source"] == nom) {
        return;
    }
    let ids = items
        .iter()
        .filter(|v| v["source"] == nom)
        .filter_map(|v| v["source_id"].as_str().map(str::to_owned))
        .collect::<Vec<_>>();
    let connus = pistes_du_service(state, nom, ids, Duration::from_secs(30)).await;
    for ligne in items {
        if ligne["source"] != nom {
            continue;
        }
        let Some(piste) = ligne["source_id"].as_str().and_then(|id| connus.get(id)) else {
            continue;
        };
        if nom != "qobuz" {
            if let (Some(d), Some(objet)) = (piste.disponible, ligne.as_object_mut()) {
                objet.insert("disponible".into(), serde_json::json!(d));
            }
            continue;
        }
        let Ok(Value::Object(champs)) = serde_json::to_value(piste) else {
            continue;
        };
        let Some(objet) = ligne.as_object_mut() else {
            continue;
        };
        // La playlist locale est lue comme Track, sans le normaliseur des
        // routes streaming : QualityBadge lit ces champs à plat.
        if let Some(qualite) = &piste.quality {
            objet.insert("format".into(), serde_json::json!(qualite.codec));
            objet.insert("sample_rate".into(), serde_json::json!(qualite.sample_rate));
            objet.insert("bit_depth".into(), serde_json::json!(qualite.bit_depth));
        }
        for (cle, valeur) in champs {
            // Conserver les informations du fichier si le catalogue les omet.
            if !valeur.is_null() && valeur.as_str() != Some("") {
                objet.insert(cle, valeur);
            }
        }
    }
}

/// Les fiches des titres `ids` chez le service `nom` : le cache d'abord (cinq
/// minutes), puis le service — quatre requêtes au plus en parallèle, trois
/// secondes chacune, `delai` en tout (`CACHE_SEUL` : le cache, sans aucun
/// appel). Un service absent, désactivé ou non connecté rend une table
/// vide : rien n'est inventé.
async fn pistes_du_service(
    state: &AppState,
    nom: &str,
    mut ids: Vec<String>,
    delai: Duration,
) -> HashMap<String, StreamTrack> {
    let mut connus = HashMap::new();
    // Ne jamais tenir le registre pendant les échanges réseau.
    let service = { state.services.lock().await.get(nom) };
    let Some(service) = service else {
        return connus;
    };
    let service = service.read().await;
    if !service.enabled() || !service.auth_status().await.authenticated {
        return connus;
    }
    ids.sort();
    ids.dedup();
    {
        let mut cache = state.metadonnees_playlists.0.lock().await;
        cache.retain(|_, (date, _)| date.elapsed() < Duration::from_secs(300));
        ids.retain(|id| {
            if let Some((_, piste)) = cache.get(&cle(nom, id)) {
                connus.insert(id.clone(), piste.clone());
                false
            } else {
                true
            }
        });
    }
    // Le cache seul (`CACHE_SEUL`) : aucun appel au service.
    if delai.is_zero() || ids.is_empty() {
        return connus;
    }
    // Quatre requêtes au plus ; un service en panne ne bloque pas indéfiniment
    // la fiche. Les succès partiels restent en cache pour la lecture suivante.
    let lectures = stream::iter(ids.into_iter().map(|id| {
        let service = &service;
        async move {
            let piste = tokio::time::timeout(Duration::from_secs(3), service.get_track(&id)).await;
            (id, piste)
        }
    }))
    .buffer_unordered(4);
    tokio::pin!(lectures);
    let limite = tokio::time::Instant::now() + delai;
    while let Ok(Some((id, resultat))) = tokio::time::timeout_at(limite, lectures.next()).await {
        let Ok(Ok(piste)) = resultat else { continue };
        if piste.id != id {
            continue;
        }
        let mut cache = state.metadonnees_playlists.0.lock().await;
        if cache.len() >= 4096
            && let Some(ancienne) = cache
                .iter()
                .min_by_key(|(_, (date, _))| *date)
                .map(|(id, _)| id.clone())
        {
            cache.remove(&ancienne);
        }
        cache.insert(cle(nom, &id), (Instant::now(), piste.clone()));
        connus.insert(id, piste);
    }
    connus
}

/// Les services dont la fiche d'un titre porte sa durée.
const SERVICES_A_DUREE: [&str; 2] = ["qobuz", "tidal"];

/// Le cache des fiches seul, sans appel au service : ce que la LECTURE
/// s'accorde avant de démarrer. Elle n'attend jamais le service.
pub(crate) const CACHE_SEUL: Duration = Duration::ZERO;

/// L'ajout à une playlist n'est pas une lecture : il peut attendre un peu la
/// fiche, jamais plus de deux secondes.
pub(crate) const DELAI_A_L_AJOUT: Duration = Duration::from_secs(2);

/// La complétion en tâche de fond, après le démarrage de la lecture.
const DELAI_EN_ARRIERE_PLAN: Duration = Duration::from_secs(30);

/// Bug du 14/11 (recette de Bertrand, « Lecture en cours ») — un titre de
/// service rangé dans une playlist SANS durée (`duration_ms` 0 ou nul : posé
/// depuis les Favoris, dont la table ne garde pas la durée, ou par un import
/// M3U) entrait dans la file avec 0, et « À suivre » affichait « 0:00 ».
/// `GET /playlists/{id}/tracks` complétait déjà ces lignes pour l'écran
/// (#7738), mais ni l'ajout ni la lecture ne le faisaient.
///
/// Complète la durée (et l'album chez le service, s'il manque) des titres
/// Qobuz et TIDAL qui n'en ont pas, par leur fiche. Un titre dont la fiche ne
/// vient pas garde sa ligne telle quelle.
pub(crate) async fn completer_durees(
    state: &AppState,
    titres: Vec<&mut tune_core::db::playlist_repo::ServiceEntry>,
    delai: Duration,
) {
    let mut titres: Vec<_> = titres
        .into_iter()
        .filter(|s| s.duration_ms.unwrap_or(0) <= 0)
        .filter(|s| SERVICES_A_DUREE.contains(&s.source.as_str()))
        .collect();
    if titres.is_empty() {
        return;
    }
    let debut = tokio::time::Instant::now();
    for nom in SERVICES_A_DUREE {
        let ids: Vec<String> = titres
            .iter()
            .filter(|s| s.source == nom)
            .map(|s| s.source_id.clone())
            .collect();
        if ids.is_empty() {
            continue;
        }
        let reste = delai.saturating_sub(debut.elapsed());
        let connus = pistes_du_service(state, nom, ids, reste).await;
        for s in titres.iter_mut().filter(|s| s.source == nom) {
            let Some(piste) = connus.get(&s.source_id) else {
                continue;
            };
            if piste.duration_ms > 0 {
                s.duration_ms = Some(piste.duration_ms as i64);
            }
            if s.album_source_id.is_none() {
                s.album_source_id = piste.album_id.clone().filter(|a| !a.is_empty());
            }
        }
    }
}

/// Après le démarrage d'une lecture (ou le versement dans la file) d'une
/// playlist : complète EN TÂCHE DE FOND les titres de service qui n'ont pas
/// de durée, depuis le cache puis le service. Chaque durée trouvée est
/// réécrite dans `playlist_tracks` — la playlist n'est corrigée qu'une fois —
/// et dans les lignes de la file de `zone_id` qui sont encore à 0 ; l'écran
/// est prévenu par `playback.queue.updated`, qui recharge la file.
pub(crate) fn completer_en_arriere_plan(state: &AppState, zone_id: i64, playlist_id: i64) {
    let state = state.clone();
    tokio::spawn(async move {
        completer_playlist_et_file(&state, zone_id, playlist_id, DELAI_EN_ARRIERE_PLAN).await;
    });
}

/// Le corps de `completer_en_arriere_plan`, attendu (pour les tests).
pub(crate) async fn completer_playlist_et_file(
    state: &AppState,
    zone_id: i64,
    playlist_id: i64,
    delai: Duration,
) {
    use tune_core::db::play_queue_repo::PlayQueueRepo;
    use tune_core::db::playlist_repo::{EntryContent, PlaylistRepo};
    let playlists = PlaylistRepo::with_backend(state.backend.clone());
    let Ok(lignes) = playlists.get_entries(playlist_id) else {
        return;
    };
    let mut titres: Vec<_> = lignes
        .into_iter()
        .filter_map(|e| match e.content {
            EntryContent::Service(s)
                if s.duration_ms.unwrap_or(0) <= 0
                    && SERVICES_A_DUREE.contains(&s.source.as_str()) =>
            {
                Some(s)
            }
            _ => None,
        })
        .collect();
    if titres.is_empty() {
        return;
    }
    completer_durees(state, titres.iter_mut().collect(), delai).await;
    let file = PlayQueueRepo::with_backend(state.backend.clone());
    let mut en_file = 0usize;
    for s in titres.iter().filter(|s| s.duration_ms.unwrap_or(0) > 0) {
        let duree = s.duration_ms.unwrap_or(0);
        if let Err(e) = playlists.completer_duree_de_service(
            playlist_id,
            &s.source,
            &s.source_id,
            duree,
            s.album_source_id.as_deref(),
        ) {
            tracing::warn!(playlist_id, error = %e, "playlist_duree_non_reecrite");
        }
        match file.completer_duree_de_service(zone_id, &s.source, &s.source_id, duree) {
            Ok(n) => en_file += n,
            Err(e) => tracing::warn!(zone_id, error = %e, "file_duree_non_reecrite"),
        }
    }
    if en_file > 0 {
        tracing::info!(
            zone_id,
            playlist_id,
            lignes = en_file,
            "file_durees_completees"
        );
        state.event_bus.emit(
            "playback.queue.updated",
            serde_json::json!({ "zone_id": zone_id, "durations_completed": en_file }),
        );
    }
}

/// Les titres de service d'une liste de lignes de playlist, à compléter.
pub(crate) fn titres_de_service(
    entries: &mut [tune_core::db::playlist_repo::PlaylistEntry],
) -> Vec<&mut tune_core::db::playlist_repo::ServiceEntry> {
    entries
        .iter_mut()
        .filter_map(|e| match &mut e.content {
            tune_core::db::playlist_repo::EntryContent::Service(s) => Some(s),
            _ => None,
        })
        .collect()
}

/// La clé du cache : le service ET l'identifiant (#8066 — deux services
/// peuvent avoir le même identifiant numérique).
fn cle(service: &str, id: &str) -> String {
    format!("{service}:{id}")
}
