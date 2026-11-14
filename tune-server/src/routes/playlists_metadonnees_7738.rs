//! Les M3U (y compris ceux du scan) ne portent que l'identité Qobuz.
//! Compléter à la lecture répare aussi les playlists déjà importées.
use crate::state::AppState;
use futures_util::{StreamExt, stream};
use serde_json::Value;
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tune_core::db::play_queue_repo::QueueInput;
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

/// Recette du 14/11 (rc12) — « À suivre » et la file montraient 0:00 sur les
/// titres Qobuz d'une playlist locale (« Caipirinha », « I Can't Go For
/// That », « Giorgio by Moroder »). Leur ligne de playlist ne porte aucune
/// durée (`playlist_tracks.duration_ms` vide ou 0) : la fiche
/// `GET /playlists/{id}/tracks` la complète à la lecture ([`completer`]), mais
/// la FILE est écrite depuis les lignes brutes (`entrees_de_file`) et gardait
/// ce zéro.
///
/// Les lignes Qobuz de la file sans durée sont donc complétées ici, depuis le
/// même cache que la fiche (la playlist vient presque toujours d'être
/// affichée : aucune requête), puis depuis le catalogue dans un budget court —
/// la lecture ne doit pas attendre. Une ligne que le catalogue ne rend pas
/// garde ce qu'elle avait. Seule une ligne SANS DURÉE est cherchée ; on en
/// profite pour remplir ses autres cases vides (pochette, album), jamais pour
/// réécrire ce que la playlist porte déjà.
pub(crate) async fn completer_file(state: &AppState, items: &mut [QueueInput]) {
    const NOM: &str = "qobuz";
    let a_completer = |item: &QueueInput| match item {
        QueueInput::Streaming {
            source,
            duration_ms,
            ..
        } => source == NOM && *duration_ms <= 0,
        QueueInput::Local { .. } => false,
    };
    let ids: Vec<String> = items
        .iter()
        .filter(|i| a_completer(i))
        .filter_map(|i| match i {
            QueueInput::Streaming { source_id, .. } => Some(source_id.clone()),
            QueueInput::Local { .. } => None,
        })
        .collect();
    if ids.is_empty() {
        return;
    }
    let connus = pistes_du_service(state, NOM, ids, Duration::from_secs(5)).await;
    for item in items.iter_mut() {
        if !a_completer(item) {
            continue;
        }
        let QueueInput::Streaming {
            source_id,
            title,
            artist,
            album,
            cover_url,
            duration_ms,
            ..
        } = item
        else {
            continue;
        };
        let Some(piste) = connus.get(source_id.as_str()) else {
            continue;
        };
        if *duration_ms <= 0 && piste.duration_ms > 0 {
            *duration_ms = i64::try_from(piste.duration_ms).unwrap_or(i64::MAX);
        }
        if cover_url.as_deref().unwrap_or("").is_empty()
            && let Some(c) = piste.cover_path.as_deref().filter(|c| !c.is_empty())
        {
            *cover_url = Some(c.to_owned());
        }
        if album.as_deref().unwrap_or("").is_empty()
            && let Some(a) = piste.album.as_deref().filter(|a| !a.is_empty())
        {
            *album = Some(a.to_owned());
        }
        if artist.is_empty() && !piste.artist.is_empty() {
            *artist = piste.artist.clone();
        }
        if title.is_empty() && !piste.title.is_empty() {
            *title = piste.title.clone();
        }
    }
}

/// Les fiches catalogue de `ids` chez le service `nom` : le cache de cinq
/// minutes d'abord, puis le service, quatre requêtes au plus, dans la limite
/// de `budget`. Un service absent, désactivé ou déconnecté ne rend rien.
async fn pistes_du_service(
    state: &AppState,
    nom: &'static str,
    mut ids: Vec<String>,
    budget: Duration,
) -> HashMap<String, StreamTrack> {
    // Ne jamais tenir le registre pendant les échanges réseau.
    let service = { state.services.lock().await.get(nom) };
    let Some(service) = service else {
        return HashMap::new();
    };
    let service = service.read().await;
    if !service.enabled() || !service.auth_status().await.authenticated {
        return HashMap::new();
    }
    ids.sort();
    ids.dedup();
    let mut connus = HashMap::new();
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
    let limite = tokio::time::Instant::now() + budget;
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

/// La clé du cache : le service ET l'identifiant (#8066 — deux services
/// peuvent avoir le même identifiant numérique).
fn cle(service: &str, id: &str) -> String {
    format!("{service}:{id}")
}
