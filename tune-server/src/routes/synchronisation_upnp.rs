//! Durable subscriptions to chosen UPnP containers. Only a complete Browse
//! authorizes reconciliation; overlapping subscriptions retain their tracks.
use super::indexation_upnp::{DemandeIndexation, indexer};
use crate::state::AppState;
use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tune_core::db::backend::{DbBackend, DbTxHandle};

fn now_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

type ApiResult = Result<Json<Value>, (StatusCode, Json<Value>)>;
fn error(e: impl ToString) -> (StatusCode, Json<Value>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({"error": e.to_string()})),
    )
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Source {
    key: String,
    udn: String,
    container: String,
    name: String,
    enabled: bool,
    status: String,
    last_attempt: i64,
    last_success: Option<i64>,
    report: Value,
    generation: String,
    pending: Vec<i64>,
}

fn sources(db: &dyn DbBackend) -> Result<Vec<Source>, String> {
    db.query_many_strong(
        "SELECT state_json FROM upnp_library_sources ORDER BY source_key",
        &[],
    )?
    .iter()
    .map(|r| {
        serde_json::from_str(r[0].as_str().ok_or("source invalide")?).map_err(|e| e.to_string())
    })
    .collect()
}
fn save(db: &dyn DbBackend, s: &Source) -> Result<(), String> {
    let body = serde_json::to_string(s).map_err(|e| e.to_string())?;
    db.execute("INSERT INTO upnp_library_sources (source_key, udn, container, state_json) VALUES (?, ?, ?, ?) ON CONFLICT(source_key) DO UPDATE SET state_json = excluded.state_json",
        &[&s.key, &s.udn, &s.container, &body])?;
    Ok(())
}
fn public(s: &Source) -> Value {
    json!({"key": s.key, "udn": s.udn, "container": s.container, "name": s.name,
        "enabled": s.enabled, "status": s.status, "last_attempt": s.last_attempt,
        "last_success": s.last_success, "report": s.report, "generation": s.generation,
        "pending_count": s.pending.len()})
}

/// L'état d'import des sources UPnP synchronisées, tel que `/library/stats`
/// le porte dans son champ additif `upnp_import`.
///
/// `library/stats` annonçait le nombre de pistes UPnP importées sans réserve,
/// alors que le dernier bilan de chaque source SAIT si l'import est complet :
/// mesuré sur le .18 le 24/09, 49 395 pistes importées sur 50 772, plafond de
/// 1 000 conteneurs atteint, 34 paginations interrompues — et l'écran n'en
/// disait rien. Rien n'est recalculé ici : c'est le bilan persisté de la
/// dernière passe (`report`), lu tel quel.
///
/// Une erreur de lecture ne fait pas tomber la route qui l'appelle : elle rend
/// `null`, et les compteurs existants continuent de répondre.
pub(crate) fn etat_d_import(db: &dyn DbBackend) -> Value {
    let Ok(toutes) = sources(db) else {
        return Value::Null;
    };
    let mut par_source = Vec::new();
    let mut raisons = Vec::new();
    let mut plafonds: Vec<String> = Vec::new();
    let (mut erreurs, mut interrompues, mut incompletes) = (0usize, 0usize, 0usize);
    let (mut distinctes, mut vus, mut visites, mut sans_url) = (0u64, 0u64, 0u64, 0u64);
    for s in &toutes {
        let r = &s.report;
        let complet = r["complet"] == true;
        let liste_erreurs: Vec<&str> = r["erreurs"]
            .as_array()
            .map(|e| e.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let nb_interrompues = liste_erreurs
            .iter()
            .filter(|e| e.contains("pagination interrompue"))
            .count();
        let plafond = r["parcours"]["plafond_atteint"].as_str();
        let n = |v: &Value| v.as_u64().unwrap_or(0);
        let ecartees = n(&r["pistes"]["ecartees_sans_url_de_lecture"]);
        let raison = if complet {
            None
        } else if let Some(e) = r["error"].as_str() {
            Some(e.to_string())
        } else if r["indexe"] == false {
            Some(
                r["detail"]
                    .as_str()
                    .or(r["raison"].as_str())
                    .unwrap_or("source indisponible")
                    .to_string(),
            )
        } else if r.get("complet").is_none() {
            Some("aucun import terminé pour cette source".to_string())
        } else {
            let mut morceaux = Vec::new();
            if let Some(message) = r["parcours"]["plafond"]["message"].as_str() {
                morceaux.push(message.to_string());
            } else if let Some(nature) = plafond {
                morceaux.push(format!("plafond de {nature} atteint"));
            }
            if nb_interrompues > 0 {
                morceaux.push(format!("{nb_interrompues} pagination(s) interrompue(s)"));
            }
            let autres = liste_erreurs.len() - nb_interrompues;
            if autres > 0 {
                morceaux.push(format!("{autres} autre(s) erreur(s) de parcours"));
            }
            if ecartees > 0 {
                morceaux.push(format!(
                    "{ecartees} piste(s) écartée(s) sans URL de lecture"
                ));
            }
            if morceaux.is_empty() {
                morceaux.push("dernier bilan incomplet".to_string());
            }
            Some(morceaux.join(" ; "))
        };
        if let Some(raison) = &raison {
            incompletes += 1;
            raisons.push(format!("{} : {raison}", s.name));
        }
        if let Some(nature) = plafond {
            if !plafonds.iter().any(|p| p == nature) {
                plafonds.push(nature.to_string());
            }
        }
        erreurs += liste_erreurs.len();
        interrompues += nb_interrompues;
        distinctes += n(&r["pistes"]["distinctes"]);
        vus += n(&r["parcours"]["items_vus"]);
        visites += n(&r["parcours"]["conteneurs_visites"]);
        sans_url += ecartees;
        par_source.push(json!({
            "key": s.key,
            "name": s.name,
            "udn": s.udn,
            "container": s.container,
            "status": s.status,
            "last_attempt": s.last_attempt,
            "last_success": s.last_success,
            "complet": raison.is_none(),
            "raison": raison,
            "plafond_atteint": plafond,
            "erreurs": liste_erreurs.len(),
            "paginations_interrompues": nb_interrompues,
            "pistes_distinctes": r["pistes"]["distinctes"],
            "items_vus": r["parcours"]["items_vus"],
            "conteneurs_visites": r["parcours"]["conteneurs_visites"],
        }));
    }
    json!({
        "sources": toutes.len(),
        // Sans source, il n'y a pas d'import à juger : ni complet ni incomplet.
        "complet": if toutes.is_empty() { Value::Null } else { json!(incompletes == 0) },
        "sources_incompletes": incompletes,
        "raisons": raisons,
        "plafonds_atteints": plafonds,
        "erreurs": erreurs,
        "paginations_interrompues": interrompues,
        "pistes_distinctes": distinctes,
        "items_vus": vus,
        "conteneurs_visites": visites,
        "ecartees_sans_url_de_lecture": sans_url,
        "par_source": par_source,
    })
}
pub async fn list(State(state): State<AppState>) -> ApiResult {
    Ok(Json(
        json!({"items": sources(state.backend.as_ref()).map_err(error)?.iter().map(public).collect::<Vec<_>>()}),
    ))
}

#[derive(Deserialize)]
pub struct Subscribe {
    container: String,
    name: Option<String>,
}
/// Registration returns immediately. The job and its report survive navigation.
pub async fn subscribe(
    State(state): State<AppState>,
    Path(udn): Path<String>,
    Json(request): Json<Subscribe>,
) -> ApiResult {
    if request.container.is_empty() || request.container.len() > 4096 {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "conteneur invalide"})),
        ));
    }
    let _guard = state.upnp_index_lock.try_lock().map_err(|_| {
        (
            StatusCode::CONFLICT,
            Json(json!({"error": "Une indexation est en cours. Réessayez après sa fin."})),
        )
    })?;
    let server = state
        .media_servers
        .lock()
        .await
        .get(&udn)
        .cloned()
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "serveur inconnu"})),
            )
        })?;
    let key = serde_json::to_string(&(&udn, &request.container)).map_err(error)?;
    let mut source = sources(state.backend.as_ref())
        .map_err(error)?
        .into_iter()
        .find(|s| s.key == key)
        .unwrap_or(Source {
            key,
            udn,
            container: request.container,
            name: request
                .name
                .filter(|n| !n.trim().is_empty())
                .map(|n| format!("{} · {}", server.name, n))
                .unwrap_or(server.name),
            enabled: true,
            status: "pending".into(),
            last_attempt: 0,
            last_success: None,
            report: json!({}),
            generation: String::new(),
            pending: vec![],
        });
    source.enabled = true;
    source.status = "pending".into();
    source.pending.clear();
    save(state.backend.as_ref(), &source).map_err(error)?;
    let result = public(&source);
    let next = state.clone();
    tokio::spawn(async move {
        run_one(next, source.key).await;
    });
    Ok(Json(result))
}

#[derive(Deserialize)]
pub struct Action {
    key: String,
    action: String,
    generation: Option<String>,
    count: Option<usize>,
}
pub async fn act(State(state): State<AppState>, Json(request): Json<Action>) -> ApiResult {
    let _guard = state.upnp_index_lock.try_lock().map_err(|_| {
        (
            StatusCode::CONFLICT,
            Json(json!({"error": "Une indexation est en cours. Réessayez après sa fin."})),
        )
    })?;
    let mut source = sources(state.backend.as_ref())
        .map_err(error)?
        .into_iter()
        .find(|s| s.key == request.key)
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "source inconnue"})),
            )
        })?;
    match request.action.as_str() {
        "pause" => {
            source.enabled = false;
        }
        "sync" => {
            source.enabled = true;
            source.status = "pending".into();
            source.pending.clear();
        }
        "confirm" => {
            if source.status != "confirmation"
                || request.generation.as_deref() != Some(source.generation.as_str())
                || request.count != Some(source.pending.len())
                || source.pending.is_empty()
            {
                return Err((
                    StatusCode::CONFLICT,
                    Json(
                        json!({"error": "Le bilan a changé. Relisez le nombre de suppressions avant de confirmer."}),
                    ),
                ));
            }
            // Confirmation is bound to this exact complete snapshot, and expires
            // after an hour: a stale browser cannot purge a later catalogue.
            if now_seconds() - source.last_attempt > 3600 {
                return Err((
                    StatusCode::CONFLICT,
                    Json(json!({"error": "Bilan expiré : relancez la synchronisation."})),
                ));
            }
            let removed = remove_missing(&state, &source, true).map_err(error)?;
            source.report["supprimees"] = json!(removed);
            source.pending.clear();
            source.status = "ready".into();
        }
        _ => {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "action inconnue"})),
            ));
        }
    }
    save(state.backend.as_ref(), &source).map_err(error)?;
    let result = public(&source);
    if request.action == "sync" {
        let next = state.clone();
        tokio::spawn(async move {
            run_one(next, source.key).await;
        });
    }
    Ok(Json(result))
}

fn member_ids(tx: &dyn DbTxHandle, key: &str) -> Result<Vec<i64>, String> {
    Ok(tx
        .query_many(
            "SELECT track_id FROM upnp_library_members WHERE source_key = ?",
            &[&key],
        )?
        .iter()
        .filter_map(|r| r[0].as_i64())
        .collect())
}

/// Every candidate is scoped by both membership and the track's source/UDN.
/// A second subscription still owning the track prevents its deletion.
fn liens_utilisateur_sql(ids: &str) -> String {
    // ids est construit exclusivement depuis des i64, jamais depuis une requête.
    format!(
        "SELECT t.id FROM tracks t WHERE t.id IN ({ids}) AND (
        EXISTS (SELECT 1 FROM playlist_tracks pt WHERE pt.track_id = t.id)
        OR EXISTS (SELECT 1 FROM favorites f WHERE
            (f.item_type = 'track' AND f.item_id = CAST(t.id AS TEXT))
            OR (f.item_type = 'album' AND f.item_id = CAST(t.album_id AS TEXT)))) LIMIT 1"
    )
}

fn retrait_avec_liens(state: &AppState, source: &Source) -> Result<bool, String> {
    for ids in source.pending.chunks(500) {
        let ids = ids.iter().map(i64::to_string).collect::<Vec<_>>().join(",");
        if state
            .backend
            .query_one(&liens_utilisateur_sql(&ids), &[])?
            .is_some()
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn remove_missing(state: &AppState, source: &Source, confirme: bool) -> Result<usize, String> {
    let mut removed = 0;
    state.backend.write_tx(&mut |tx| {
        let owned: std::collections::HashSet<i64> = member_ids(tx, &source.key)?.into_iter().collect();
        for id in &source.pending {
            if !owned.contains(id) { continue; }
            tx.execute("DELETE FROM upnp_library_members WHERE source_key = ? AND track_id = ? AND generation <> ?",
                &[&source.key, id, &source.generation])?;
            // A refreshed member must not be removed by an earlier proposal.
            if tx.query_one("SELECT track_id FROM upnp_library_members WHERE track_id = ?", &[id])?.is_some() { continue; }
            let row = tx.query_one("SELECT source, source_id FROM tracks WHERE id = ?", &[id])?;
            let ours = row.is_some_and(|r| r[0].as_str() == Some("upnp") && r[1].as_str().is_some_and(|s| s.starts_with(&format!("{}|", source.udn))));
            if ours {
                // Revalider dans la transaction : un favori/une playlist a pu
                // être ajouté depuis le bilan. Une erreur annule aussi le
                // retrait des appartenances effectué plus haut.
                if !confirme && tx.query_one(&liens_utilisateur_sql(&id.to_string()), &[])?.is_some() {
                    return Err("Une piste à retirer possède des favoris ou des liens de playlist ; relancez la synchronisation pour examiner le retrait".into());
                }
                removed += tx.execute("DELETE FROM tracks WHERE id = ? AND source = 'upnp'", &[id])?;
            }
        }
        // Only empty remote albums of THIS server. Local albums are untouched.
        let albums = tx.query_many("SELECT id, source_id FROM albums WHERE source = 'upnp' AND NOT EXISTS (SELECT 1 FROM tracks WHERE tracks.album_id = albums.id)", &[])?;
        for row in albums {
            if row[1].as_str().is_some_and(|s| s.starts_with(&format!("{}|", source.udn))) {
                let id = row[0].as_i64().ok_or("album sans identifiant")?;
                tx.execute("DELETE FROM albums WHERE id = ? AND source = 'upnp' AND NOT EXISTS (SELECT 1 FROM favorites f WHERE f.item_type = 'album' AND f.item_id = CAST(albums.id AS TEXT))", &[&id])?;
            }
        }
        Ok(())
    })?;
    Ok(removed)
}

async fn run_one(state: AppState, key: String) {
    let _guard = state.upnp_index_lock.lock().await;
    let result = async {
        let Some(mut source) = sources(state.backend.as_ref())?.into_iter().find(|s| s.key == key) else { return Ok::<(), String>(()); };
        if !source.enabled || source.status == "confirmation" { return Ok(()); }
        // A queued duplicate does not run again after the first has finished.
        let now = now_seconds();
        if source.status != "pending" && now - source.last_attempt < 3600 { return Ok(()); }
        source.status = "running".into();
        source.last_attempt = now;
        source.generation = uuid::Uuid::new_v4().to_string();
        source.pending.clear();
        save(state.backend.as_ref(), &source)?;
        super::network::synchroniser_le_registre(&state).await;
        let Json(mut report) = tokio::time::timeout(std::time::Duration::from_secs(1800), indexer(&state, &source.udn, DemandeIndexation {
            conteneur: Some(source.container.clone()), profondeur_max: None, max_conteneurs: None, max_pistes: None,
        })).await.map_err(|_| "Synchronisation interrompue après 30 minutes ; aucun retrait autorisé")?;
        let identities = report.as_object_mut().and_then(|r| r.remove("identites")).unwrap_or(json!([]));
        let complete = report["complet"] == true;
        let mut prior_count = 0;
        state.backend.write_tx(&mut |tx| {
            // The conversion schema temporarily uses TEXT track IDs, so the
            // membership table cannot declare that FK at creation time.
            // Clean up removed tracks explicitly before computing percentages.
            tx.execute("DELETE FROM upnp_library_members WHERE NOT EXISTS (SELECT 1 FROM tracks WHERE tracks.id = upnp_library_members.track_id)", &[])?;
            prior_count = member_ids(tx, &source.key)?.len();
            for identity in identities.as_array().ok_or("identités invalides")? {
                let identity = identity.as_str().ok_or("identité invalide")?;
                let row = tx.query_one("SELECT id FROM tracks WHERE source = 'upnp' AND source_id = ?", &[&identity])?
                    .ok_or("piste indexée introuvable")?;
                let id = row[0].as_i64().ok_or("identifiant de piste invalide")?;
                tx.execute("INSERT INTO upnp_library_members (source_key, track_id, generation) VALUES (?, ?, ?) ON CONFLICT(source_key, track_id) DO UPDATE SET generation = excluded.generation",
                    &[&source.key, &id, &source.generation])?;
            }
            if complete {
                source.pending = tx.query_many("SELECT track_id FROM upnp_library_members WHERE source_key = ? AND generation <> ?", &[&source.key, &source.generation])?
                    .iter().filter_map(|r| r[0].as_i64()).collect();
            }
            Ok(())
        })?;
        source.report = report;
        if complete {
            source.last_success = Some(now);
            let liens = retrait_avec_liens(&state, &source)?;
            source.report["retrait_avec_liens_utilisateur"] = json!(liens);
            if !source.pending.is_empty() && (liens || source.pending.len().saturating_mul(100) > prior_count.saturating_mul(20)) {
                source.status = "confirmation".into();
            } else {
                source.report["supprimees"] = json!(remove_missing(&state, &source, false)?);
                source.pending.clear();
                source.status = "ready".into();
            }
        } else {
            source.status = if source.report["indexe"] == true { "partial" } else { "unavailable" }.into();
        }
        save(state.backend.as_ref(), &source)?;
        Ok(())
    }.await;
    if let Err(e) = result {
        tracing::error!(source = %key, error = %e, "upnp_sync_failed");
        if let Ok(all) = sources(state.backend.as_ref()) {
            if let Some(mut s) = all.into_iter().find(|s| s.key == key) {
                s.status = "error".into();
                s.report = json!({"error": e});
                s.pending.clear();
                if let Err(e) = save(state.backend.as_ref(), &s) {
                    tracing::error!("upnp_sync_state_failed: {e}");
                }
            }
        }
    }
}

pub fn start(state: AppState) {
    tokio::spawn(async move {
        // Persisted running states belonged to the previous process. Retry
        // without treating the interrupted snapshot as deletion evidence.
        if let Ok(all) = sources(state.backend.as_ref()) {
            for mut s in all {
                if s.status == "running" {
                    s.status = "pending".into();
                    s.pending.clear();
                    let _ = save(state.backend.as_ref(), &s);
                }
            }
        }
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            match sources(state.backend.as_ref()) {
                Ok(all) => {
                    for s in all {
                        run_one(state.clone(), s.key).await;
                    }
                }
                Err(e) => tracing::error!("upnp_sync_sources_failed: {e}"),
            }
        }
    });
}

#[cfg(test)]
mod identites_upnp_tests {
    use super::*;
    use tune_core::db::{
        album_repo::AlbumRepo,
        models::{Album, Track},
        playlist_repo::PlaylistRepo,
        track_repo::TrackRepo,
    };

    #[test]
    fn identites_upnp_les_liens_interdisent_le_retrait_automatique_et_la_transaction_restitue_les_membres()
     {
        for nature in ["track", "album", "playlist"] {
            let state = AppState::new(":memory:", 0, Default::default()).unwrap();
            let albums = AlbumRepo::with_backend(state.backend.clone());
            let mut album = Album::new("Album protégé".into());
            album.source = "upnp".into();
            album.source_id = Some("u|album".into());
            let aid = albums.create(&album).unwrap();
            let tracks = TrackRepo::with_backend(state.backend.clone());
            let mut track = Track::new("Piste protégée".into());
            track.source = "upnp".into();
            track.source_id = Some("u|piste".into());
            track.album_id = Some(aid);
            let id = tracks.create(&track).unwrap();
            let source = Source {
                key: "s".into(),
                udn: "u".into(),
                container: "0".into(),
                name: "NAS".into(),
                enabled: true,
                status: "ready".into(),
                last_attempt: now_seconds(),
                last_success: None,
                report: json!({}),
                generation: "new".into(),
                pending: vec![id],
            };
            save(state.backend.as_ref(), &source).unwrap();
            state.backend.execute("INSERT INTO upnp_library_members (source_key,track_id,generation) VALUES ('s',?,'old')", &[&id]).unwrap();
            assert!(!retrait_avec_liens(&state, &source).unwrap());
            if nature == "playlist" {
                let playlists = PlaylistRepo::with_backend(state.backend.clone());
                let pid = playlists.create("Garder", None, 1).unwrap();
                playlists.add_tracks(pid, &[id], None).unwrap();
            } else {
                let item = if nature == "album" { aid } else { id };
                state
                    .backend
                    .execute(
                        "INSERT INTO favorites (profile_id,item_type,item_id) VALUES (1,?,?)",
                        &[&nature, &item.to_string()],
                    )
                    .unwrap();
            }
            assert!(
                retrait_avec_liens(&state, &source).unwrap(),
                "un lien {nature} impose une confirmation"
            );
            assert!(
                remove_missing(&state, &source, false).is_err(),
                "le retrait automatique doit refuser le lien {nature}"
            );
            assert!(tracks.get(id).unwrap().is_some());
            assert_eq!(
                state
                    .backend
                    .query_one("SELECT COUNT(*) FROM upnp_library_members", &[])
                    .unwrap()
                    .unwrap()[0]
                    .as_i64(),
                Some(1),
                "la transaction restitue l'appartenance"
            );
            assert_eq!(
                remove_missing(&state, &source, true).unwrap(),
                1,
                "la confirmation explicite reste possible"
            );
            if nature == "album" {
                assert!(
                    albums.get(aid).unwrap().is_some(),
                    "le nettoyage ne supprime pas un album favori vide"
                );
            }
        }
    }
}
