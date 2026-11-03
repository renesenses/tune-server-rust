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
    if !items.iter().any(|v| v["source"] == "qobuz") {
        return;
    }
    // Ne jamais tenir le registre pendant les échanges réseau.
    let service = { state.services.lock().await.get("qobuz") };
    let Some(service) = service else { return };
    let service = service.read().await;
    if !service.enabled() || !service.auth_status().await.authenticated {
        return;
    }
    let mut ids = items
        .iter()
        .filter(|v| v["source"] == "qobuz")
        .filter_map(|v| v["source_id"].as_str().map(str::to_owned))
        .collect::<Vec<_>>();
    ids.sort();
    ids.dedup();
    let mut connus = HashMap::new();
    {
        let mut cache = state.metadonnees_playlists.0.lock().await;
        cache.retain(|_, (date, _)| date.elapsed() < Duration::from_secs(300));
        ids.retain(|id| {
            if let Some((_, piste)) = cache.get(id) {
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
    let limite = tokio::time::Instant::now() + Duration::from_secs(30);
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
        cache.insert(id.clone(), (Instant::now(), piste.clone()));
        connus.insert(id, piste);
    }
    for ligne in items {
        if ligne["source"] != "qobuz" {
            continue;
        }
        let Some(piste) = ligne["source_id"].as_str().and_then(|id| connus.get(id)) else {
            continue;
        };
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
