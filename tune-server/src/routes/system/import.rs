use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::Deserialize;
use serde_json::{Value, json};

use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::artist_repo::ArtistRepo;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::db::track_repo::TrackRepo;

use crate::state::AppState;

/// DIRE que la bibliothèque a changé, à la fin d'un import.
///
/// Les trois imports (Roon, Plex, JRiver) écrivent des artistes, des albums et
/// des pistes DIRECTEMENT en base : ils ne passent ni par le scanner ni par le
/// surveillant de fichiers, donc aucun des deux ne parle pour eux. Ils
/// n'annonçaient rien — `import_jriver` émettait bien `import.completed`, mais
/// ce nom n'apparaît nulle part dans le client : le bundle web publié de la
/// v0.9.127 ne contient pas la chaîne. Les listes en mémoire restaient donc
/// telles quelles après un import, et il fallait changer le tri pour voir
/// arriver les albums — le contournement que Patatorz décrit (fil forum
/// #1517, issue #2186).
///
/// C'est le MÊME événement que celui du surveillant (`auto_scan.rs`), pour la
/// même raison : `library.scan.completed` ferait afficher au client une
/// bannière de fin de scan, alors qu'aucun scan n'a eu lieu. Ici on veut
/// seulement que les listes se rechargent.
///
/// Rien d'importé, rien à dire : un import qui n'a rien écrit ne doit pas
/// déclencher un rechargement complet de la grille côté client (même garde que
/// le `if had_changes` du surveillant).
fn annoncer_bibliotheque_modifiee(
    event_bus: &tune_core::event_bus::EventBus,
    source: &str,
    imported: i64,
) {
    if imported <= 0 {
        return;
    }
    event_bus.emit(
        tune_core::event_types::EventType::LibraryUpdated.as_str(),
        json!({ "source": source, "imported": imported }),
    );
    tracing::info!(source, imported, "import_library_updated_emis");
}

#[derive(Deserialize)]
pub(super) struct ImportTrackEntry {
    title: String,
    artist: Option<String>,
    album: Option<String>,
    file_path: Option<String>,
    duration_ms: Option<i64>,
    track_number: Option<i32>,
    genre: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct ImportRoonRequest {
    roon_db_path: Option<String>,
    data: Option<Vec<ImportTrackEntry>>,
}

pub(super) async fn import_roon(
    State(state): State<AppState>,
    Json(body): Json<ImportRoonRequest>,
) -> impl IntoResponse {
    let task_id = uuid_v4();
    let backend = state.backend.clone();
    let event_bus = state.event_bus.clone();
    let tid = task_id.clone();

    // Store initial task status
    let settings = SettingsRepo::with_backend(backend.clone());
    settings
        .set(
            &format!("import_task_{tid}"),
            &json!({"status": "running", "imported": 0, "skipped": 0}).to_string(),
        )
        .ok();

    tokio::spawn(async move {
        let track_repo = TrackRepo::with_backend(backend.clone());
        let artist_repo = ArtistRepo::with_backend(backend.clone());
        let album_repo = AlbumRepo::with_backend(backend.clone());
        let settings = SettingsRepo::with_backend(backend.clone());

        let mut imported = 0i32;
        let mut skipped = 0i32;
        let mut errors = Vec::<String>::new();

        // --- Path A: direct JSON data ---
        if let Some(entries) = body.data {
            for entry in &entries {
                // Skip if file_path exists and already in DB
                if let Some(ref fp) = entry.file_path {
                    if track_repo.get_by_path(fp).ok().flatten().is_some() {
                        skipped += 1;
                        continue;
                    }
                }

                let artist_name = entry.artist.as_deref().unwrap_or("Unknown Artist");
                let artist = artist_repo.get_or_create(artist_name, None, None).ok();
                let artist_id = artist.as_ref().and_then(|a| a.id);

                let album = if let Some(ref album_title) = entry.album {
                    album_repo
                        .get_or_create(album_title, artist_id.unwrap_or(0), None)
                        .ok()
                } else {
                    None
                };
                let album_id = album.as_ref().and_then(|a| a.id);

                let mut track = tune_core::db::models::Track::new(entry.title.clone());
                track.artist_id = artist_id;
                track.artist_name = entry.artist.clone();
                track.album_id = album_id;
                track.album_title = entry.album.clone();
                track.duration_ms = entry.duration_ms.unwrap_or(0);
                track.track_number = entry.track_number.unwrap_or(0);
                track.genre = entry.genre.clone();
                track.file_path = entry.file_path.clone();
                track.source = "roon_import".to_string();

                match track_repo.create(&track) {
                    Ok(_) => imported += 1,
                    Err(e) => errors.push(format!("{}: {e}", entry.title)),
                }
            }
        }
        // --- Path B: SQLite database path ---
        else if let Some(ref db_path) = body.roon_db_path {
            match rusqlite::Connection::open_with_flags(
                db_path,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            ) {
                Ok(conn) => {
                    // Roon's DB schema is proprietary; try common table/column names
                    let query = "SELECT title, artist, album, path, duration, track_number, genre \
                                 FROM tracks";
                    match conn.prepare(query) {
                        Ok(mut stmt) => {
                            let rows = stmt.query_map([], |row| {
                                Ok((
                                    row.get::<_, String>(0).unwrap_or_default(),
                                    row.get::<_, Option<String>>(1).ok().flatten(),
                                    row.get::<_, Option<String>>(2).ok().flatten(),
                                    row.get::<_, Option<String>>(3).ok().flatten(),
                                    row.get::<_, Option<i64>>(4).ok().flatten(),
                                    row.get::<_, Option<i32>>(5).ok().flatten(),
                                    row.get::<_, Option<String>>(6).ok().flatten(),
                                ))
                            });
                            if let Ok(rows) = rows {
                                for row in rows.flatten() {
                                    let (
                                        title,
                                        artist,
                                        album,
                                        file_path,
                                        duration,
                                        track_num,
                                        genre,
                                    ) = row;

                                    if let Some(ref fp) = file_path {
                                        if track_repo.get_by_path(fp).ok().flatten().is_some() {
                                            skipped += 1;
                                            continue;
                                        }
                                    }

                                    let artist_name = artist.as_deref().unwrap_or("Unknown Artist");
                                    let art =
                                        artist_repo.get_or_create(artist_name, None, None).ok();
                                    let artist_id = art.as_ref().and_then(|a| a.id);

                                    let alb = if let Some(ref album_title) = album {
                                        album_repo
                                            .get_or_create(
                                                album_title,
                                                artist_id.unwrap_or(0),
                                                None,
                                            )
                                            .ok()
                                    } else {
                                        None
                                    };
                                    let album_id = alb.as_ref().and_then(|a| a.id);

                                    let mut track = tune_core::db::models::Track::new(title);
                                    track.artist_id = artist_id;
                                    track.artist_name = artist;
                                    track.album_id = album_id;
                                    track.album_title = album;
                                    track.duration_ms = duration.unwrap_or(0);
                                    track.track_number = track_num.unwrap_or(0);
                                    track.genre = genre;
                                    track.file_path = file_path;
                                    track.source = "roon_import".to_string();

                                    match track_repo.create(&track) {
                                        Ok(_) => imported += 1,
                                        Err(e) => errors.push(e.to_string()),
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            errors.push(format!("Roon DB query failed (schema may differ): {e}"));
                        }
                    }
                }
                Err(e) => {
                    errors.push(format!("Cannot open Roon DB: {e}"));
                }
            }
        }

        let status = if errors.is_empty() {
            "completed"
        } else {
            "completed_with_errors"
        };
        settings
            .set(
                &format!("import_task_{tid}"),
                &json!({
                    "status": status,
                    "imported": imported,
                    "skipped": skipped,
                    "errors": errors.len(),
                    "error_details": errors.iter().take(20).collect::<Vec<_>>(),
                })
                .to_string(),
            )
            .ok();
        tracing::info!(
            task_id = tid,
            imported,
            skipped,
            errors = errors.len(),
            "roon_import_complete"
        );
        annoncer_bibliotheque_modifiee(&event_bus, "roon_import", imported.into());
    });

    (
        StatusCode::ACCEPTED,
        Json(json!({
            "status": "accepted",
            "task_id": task_id,
        })),
    )
}

#[derive(Deserialize)]
pub(super) struct ImportPlexRequest {
    plex_url: String,
    plex_token: String,
    library_id: Option<String>,
}

pub(super) async fn import_plex(
    State(state): State<AppState>,
    Json(body): Json<ImportPlexRequest>,
) -> impl IntoResponse {
    let task_id = uuid_v4();
    let backend = state.backend.clone();
    let plex_url = body.plex_url.trim_end_matches('/').to_string();
    let token = body.plex_token.clone();
    let library_id = body.library_id.clone();
    let event_bus = state.event_bus.clone();
    let tid = task_id.clone();

    let settings = SettingsRepo::with_backend(backend.clone());
    settings
        .set(
            &format!("import_task_{tid}"),
            &json!({"status": "running", "imported": 0}).to_string(),
        )
        .ok();

    tokio::spawn(async move {
        // Client partagé : voir `tune_core::http::client`.
        let client = tune_core::http::client::shared();
        let settings = SettingsRepo::with_backend(backend.clone());
        let track_repo = TrackRepo::with_backend(backend.clone());
        let artist_repo = ArtistRepo::with_backend(backend.clone());
        let album_repo = AlbumRepo::with_backend(backend.clone());

        let mut imported = 0i32;
        let mut skipped = 0i32;
        let mut errors = Vec::<String>::new();

        // Determine which sections to import
        let section_keys: Vec<String> = if let Some(ref lid) = library_id {
            vec![lid.clone()]
        } else {
            // Fetch all library sections and filter music ones
            let sections_url = format!("{plex_url}/library/sections?X-Plex-Token={token}");
            match client
                .get(&sections_url)
                .header("Accept", "application/json")
                .send()
                .await
            {
                Ok(resp) => {
                    let data: Value = resp.json().await.unwrap_or_default();
                    data["MediaContainer"]["Directory"]
                        .as_array()
                        .map(|dirs| {
                            dirs.iter()
                                .filter(|d| d["type"].as_str() == Some("artist"))
                                .filter_map(|d| d["key"].as_str().map(|s| s.to_string()))
                                .collect()
                        })
                        .unwrap_or_default()
                }
                Err(e) => {
                    errors.push(format!("Failed to fetch Plex sections: {e}"));
                    vec![]
                }
            }
        };

        for sec_key in &section_keys {
            let tracks_url =
                format!("{plex_url}/library/sections/{sec_key}/all?type=10&X-Plex-Token={token}");
            let resp = match client
                .get(&tracks_url)
                .header("Accept", "application/json")
                .send()
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    errors.push(format!("Section {sec_key}: {e}"));
                    continue;
                }
            };

            let data: Value = resp.json().await.unwrap_or_default();
            let tracks = match data["MediaContainer"]["Metadata"].as_array() {
                Some(t) => t,
                None => continue,
            };

            for plex_track in tracks {
                let title = plex_track["title"].as_str().unwrap_or("").to_string();
                if title.is_empty() {
                    continue;
                }
                let artist_name = plex_track["grandparentTitle"]
                    .as_str()
                    .unwrap_or("Unknown Artist")
                    .to_string();
                let album_title = plex_track["parentTitle"].as_str().unwrap_or("").to_string();
                let duration = plex_track["duration"].as_u64().unwrap_or(0) as i64;
                let track_num = plex_track["index"].as_u64().unwrap_or(0) as i32;

                // Extract file_path from Media array if available
                let file_path = plex_track["Media"]
                    .as_array()
                    .and_then(|media| media.first())
                    .and_then(|m| m["Part"].as_array())
                    .and_then(|parts| parts.first())
                    .and_then(|p| p["file"].as_str())
                    .map(|s| s.to_string());

                // Skip if we already have this track by file_path
                if let Some(ref fp) = file_path {
                    if track_repo.get_by_path(fp).ok().flatten().is_some() {
                        skipped += 1;
                        continue;
                    }
                }

                let artist = artist_repo.get_or_create(&artist_name, None, None).ok();
                let artist_id = artist.as_ref().and_then(|a| a.id);

                let album = if !album_title.is_empty() {
                    album_repo
                        .get_or_create(&album_title, artist_id.unwrap_or(0), None)
                        .ok()
                } else {
                    None
                };
                let album_id = album.as_ref().and_then(|a| a.id);

                let mut new_track = tune_core::db::models::Track::new(title);
                new_track.artist_id = artist_id;
                new_track.artist_name = Some(artist_name);
                new_track.album_id = album_id;
                new_track.album_title = if album_title.is_empty() {
                    None
                } else {
                    Some(album_title)
                };
                new_track.duration_ms = duration;
                new_track.track_number = track_num;
                new_track.file_path = file_path;
                new_track.source = "plex_import".to_string();

                match track_repo.create(&new_track) {
                    Ok(_) => imported += 1,
                    Err(e) => errors.push(e.to_string()),
                }
            }
        }

        let status = if errors.is_empty() {
            "completed"
        } else {
            "completed_with_errors"
        };
        settings
            .set(
                &format!("import_task_{tid}"),
                &json!({
                    "status": status,
                    "imported": imported,
                    "skipped": skipped,
                    "errors": errors.len(),
                    "error_details": errors.iter().take(20).collect::<Vec<_>>(),
                })
                .to_string(),
            )
            .ok();
        tracing::info!(
            task_id = tid,
            imported,
            skipped,
            errors = errors.len(),
            "plex_import_complete"
        );
        annoncer_bibliotheque_modifiee(&event_bus, "plex_import", imported.into());
    });

    (
        StatusCode::ACCEPTED,
        Json(json!({
            "status": "accepted",
            "task_id": task_id,
        })),
    )
}

pub(super) async fn import_playlists_file() -> Json<Value> {
    let task_id = uuid_v4();
    Json(json!({
        "status": "accepted",
        "message": "Playlist file import not yet implemented (M3U/CSV)",
        "task_id": task_id,
    }))
}

pub(super) async fn import_status(
    State(state): State<AppState>,
    Path(task_id): Path<String>,
) -> Json<Value> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let key = format!("import_task_{task_id}");
    if let Some(data) = settings.get(&key).ok().flatten() {
        if let Ok(parsed) = serde_json::from_str::<Value>(&data) {
            return Json(json!({
                "task_id": task_id,
                "status": parsed["status"],
                "imported": parsed["imported"],
                "skipped": parsed["skipped"],
                "errors": parsed["errors"],
                "error_details": parsed["error_details"],
            }));
        }
    }
    Json(json!({
        "task_id": task_id,
        "status": "unknown",
    }))
}

/// Simple UUID v4 generator (no external crate needed).
fn uuid_v4() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    // Pseudo-random but unique enough for task IDs
    let a = (seed & 0xFFFF_FFFF) as u32;
    let b = ((seed >> 32) & 0xFFFF) as u16;
    let c = ((seed >> 48) & 0x0FFF) as u16 | 0x4000; // version 4
    let d = ((seed >> 60) & 0x3FFF) as u16 | 0x8000; // variant
    let e = (seed.wrapping_mul(6364136223846793005) & 0xFFFF_FFFF_FFFF) as u64;
    format!("{a:08x}-{b:04x}-{c:04x}-{d:04x}-{e:012x}")
}

#[derive(Deserialize)]
pub(super) struct ImportJriverRequest {
    xml_path: String,
}

pub(super) async fn import_jriver(
    State(state): State<AppState>,
    Json(body): Json<ImportJriverRequest>,
) -> impl IntoResponse {
    let task_id = uuid_v4();
    let backend = state.backend.clone();
    let xml_path = body.xml_path.clone();
    let event_bus = state.event_bus.clone();

    let settings = SettingsRepo::with_backend(backend.clone());
    let key = format!("import_task_{task_id}");
    settings.set(&key, "running").ok();

    tokio::spawn(async move {
        let result = parse_jriver_xml(&xml_path, &backend);
        let settings = SettingsRepo::with_backend(backend);
        match result {
            Ok((imported, skipped)) => {
                settings
                    .set(&key, &format!("completed:{imported}:{skipped}"))
                    .ok();
                // `import.completed` reste émis : c'est le contrat existant du
                // suivi de tâche. Mais AUCUN client ne l'écoute — la chaîne
                // n'apparaît pas dans le bundle web publié — donc il ne
                // rafraîchit rien. Le rechargement des listes passe par
                // `library.updated`, ci-dessous.
                event_bus.emit(
                    "import.completed",
                    json!({
                        "source": "jriver", "imported": imported, "skipped": skipped,
                    }),
                );
                annoncer_bibliotheque_modifiee(&event_bus, "jriver_import", imported as i64);
            }
            Err(e) => {
                settings.set(&key, &format!("error:{e}")).ok();
            }
        }
    });

    (
        StatusCode::ACCEPTED,
        Json(json!({
            "status": "accepted",
            "task_id": task_id,
            "source": "jriver",
        })),
    )
        .into_response()
}

fn parse_jriver_xml(
    xml_path: &str,
    backend: &std::sync::Arc<dyn tune_core::db::backend::DbBackend>,
) -> Result<(usize, usize), String> {
    let content = std::fs::read_to_string(xml_path).map_err(|e| format!("read {xml_path}: {e}"))?;

    let artist_repo = ArtistRepo::with_backend(backend.clone());
    let album_repo = AlbumRepo::with_backend(backend.clone());
    let track_repo = TrackRepo::with_backend(backend.clone());

    let mut imported = 0;
    let mut skipped = 0;

    // Parse JRiver XML: <MPL><Item><Field Name="X">value</Field>...</Item></MPL>
    let mut in_item = false;
    let mut fields: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut reader = quick_xml::Reader::from_str(&content);
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(quick_xml::events::Event::Start(ref e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                if name == "Item" {
                    in_item = true;
                    fields.clear();
                } else if name == "Field" && in_item {
                    if let Some(attr) = e.attributes().flatten().find(|a| a.key.as_ref() == b"Name")
                    {
                        let field_name = String::from_utf8_lossy(&attr.value).to_string();
                        if let Ok(quick_xml::events::Event::Text(t)) =
                            reader.read_event_into(&mut buf)
                        {
                            let decoded = t.decode().unwrap_or_default();
                            let val = match quick_xml::escape::unescape(&decoded) {
                                Ok(s) => s.to_string(),
                                Err(_) => decoded.to_string(),
                            };
                            fields.insert(field_name, val);
                        }
                    }
                }
            }
            Ok(quick_xml::events::Event::End(ref e)) => {
                if String::from_utf8_lossy(e.name().as_ref()) == "Item" && in_item {
                    in_item = false;
                    let title = fields.get("Name").cloned().unwrap_or_default();
                    if title.is_empty() {
                        skipped += 1;
                        continue;
                    }
                    let artist_name = fields
                        .get("Artist")
                        .cloned()
                        .unwrap_or_else(|| "Unknown Artist".into());
                    let album_title = fields.get("Album").cloned();
                    let file_path = fields.get("Filename").cloned();

                    // Skip if already in DB by file_path
                    if let Some(ref fp) = file_path {
                        if track_repo.get_by_path(fp).ok().flatten().is_some() {
                            skipped += 1;
                            continue;
                        }
                    }

                    let artist_id = artist_repo
                        .get_or_create(&artist_name, None, None)
                        .ok()
                        .and_then(|a| a.id);
                    let album_id = album_title.as_deref().and_then(|t| {
                        album_repo
                            .get_or_create(t, artist_id.unwrap_or(0), None)
                            .ok()
                            .and_then(|a| a.id)
                    });

                    let mut track = tune_core::db::models::Track::new(title);
                    track.artist_id = artist_id;
                    track.album_id = album_id;
                    track.file_path = file_path;
                    track.genre = fields.get("Genre").cloned();
                    track.year = fields.get("Year").and_then(|y| y.parse().ok());
                    track.source = "jriver".into();
                    track_repo.create(&track).ok();
                    imported += 1;
                }
            }
            Ok(quick_xml::events::Event::Eof) => break,
            Err(e) => return Err(format!("xml parse: {e}")),
            _ => {}
        }
        buf.clear();
    }

    Ok((imported, skipped))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::AppState;
    use std::time::Duration;
    use tokio::sync::broadcast::Receiver;
    use tune_core::event_bus::TuneEvent;

    fn etat() -> AppState {
        AppState::new(":memory:", 0, Default::default()).unwrap()
    }

    /// Attendre `library.updated` sur le bus, ou renoncer.
    ///
    /// Les imports travaillent dans une tâche détachée : l'événement est le
    /// SEUL signal de fin observable de l'extérieur. Un import qui n'annonce
    /// rien fait donc expirer ce délai — c'est exactement ce que ces tests
    /// mesurent, et c'est ce qui les fait tomber quand on débranche l'émetteur.
    async fn attendre_library_updated(rx: &mut Receiver<TuneEvent>) -> Option<TuneEvent> {
        let fin = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let reste = fin.saturating_duration_since(tokio::time::Instant::now());
            if reste.is_zero() {
                return None;
            }
            match tokio::time::timeout(reste, rx.recv()).await {
                Ok(Ok(ev)) if ev.event_type == "library.updated" => return Some(ev),
                Ok(Ok(_)) => continue,
                Ok(Err(_)) | Err(_) => return None,
            }
        }
    }

    fn piste(titre: &str, chemin: &str) -> ImportTrackEntry {
        ImportTrackEntry {
            title: titre.into(),
            artist: Some("Sokratis Sinopoulos".into()),
            album: Some("Eight Winds".into()),
            file_path: Some(chemin.into()),
            duration_ms: Some(240_000),
            track_number: Some(1),
            genre: Some("Jazz".into()),
        }
    }

    /// Chemin d'ajout n° 1 : import Roon.
    ///
    /// Il écrit artiste, album et piste DIRECTEMENT en base, sans passer par le
    /// scanner ni par le surveillant de fichiers. Personne ne parle pour lui :
    /// s'il n'annonce pas lui-même, la grille du client garde ce qu'elle avait
    /// (#2186).
    #[tokio::test]
    async fn import_roon_annonce_la_bibliotheque_modifiee() {
        let state = etat();
        let mut rx = state.event_bus.subscribe();

        let body = ImportRoonRequest {
            roon_db_path: None,
            data: Some(vec![piste("Walking", "/music/eight-winds/01.flac")]),
        };
        let _ = import_roon(State(state.clone()), Json(body)).await;

        let ev = attendre_library_updated(&mut rx).await.expect(
            "l'import Roon doit annoncer `library.updated` : sans lui la grille \
                     du client ne recharge rien (#2186)",
        );
        assert_eq!(ev.data["source"], "roon_import");
        assert_eq!(ev.data["imported"], 1);
    }

    /// Chemin d'ajout n° 2 : import JRiver.
    ///
    /// Il émettait déjà `import.completed` — un nom que le client n'écoute
    /// nulle part. Émettre n'est pas annoncer : ce test exige l'événement que
    /// la vue bibliothèque consomme réellement.
    #[tokio::test]
    async fn import_jriver_annonce_la_bibliotheque_modifiee() {
        let state = etat();
        let mut rx = state.event_bus.subscribe();

        let dir = tempfile::tempdir().unwrap();
        let xml = dir.path().join("library.xml");
        std::fs::write(
            &xml,
            r#"<MPL>
                 <Item>
                   <Field Name="Name">Walking</Field>
                   <Field Name="Artist">Sokratis Sinopoulos</Field>
                   <Field Name="Album">Eight Winds</Field>
                   <Field Name="Filename">/music/eight-winds/01.flac</Field>
                 </Item>
               </MPL>"#,
        )
        .unwrap();

        let body = ImportJriverRequest {
            xml_path: xml.to_string_lossy().to_string(),
        };
        let _ = import_jriver(State(state.clone()), Json(body)).await;

        let ev = attendre_library_updated(&mut rx).await.expect(
            "l'import JRiver doit annoncer `library.updated` : `import.completed` \
                     n'est écouté par aucun client (#2186)",
        );
        assert_eq!(ev.data["source"], "jriver_import");
        assert_eq!(ev.data["imported"], 1);
    }

    /// Un import qui n'a rien écrit ne doit RIEN annoncer.
    ///
    /// Sans cette garde, un import à vide ferait recharger toute la grille du
    /// client pour rien — c'est la même retenue que le `if had_changes` du
    /// surveillant de fichiers.
    #[tokio::test]
    async fn un_import_vide_n_annonce_rien() {
        let state = etat();
        let mut rx = state.event_bus.subscribe();

        let body = ImportRoonRequest {
            roon_db_path: None,
            data: Some(vec![]),
        };
        let _ = import_roon(State(state.clone()), Json(body)).await;

        // Court délai : on cherche une ABSENCE, il suffit de laisser la tâche
        // détachée aller jusqu'au bout.
        let vu = tokio::time::timeout(Duration::from_millis(500), rx.recv()).await;
        assert!(
            !matches!(vu, Ok(Ok(ref ev)) if ev.event_type == "library.updated"),
            "un import sans piste ne doit pas déclencher de rechargement"
        );
    }

    /// Le corps d'un handler, isolé du fichier source.
    ///
    /// `include_str!` rend le fichier ENTIER, ce module de test compris — où
    /// les noms des trois handlers et celui de l'annonce apparaissent en toutes
    /// lettres. Sans la coupe à `#[cfg(test)]` ci-dessous, ce garde-fou se
    /// prouverait lui-même et resterait vert quel que soit le code de
    /// production.
    fn corps_du_handler(source: &str, nom: &str) -> String {
        let production = source
            .split_once("\n#[cfg(test)]\nmod tests {")
            .map(|(avant, _)| avant)
            .unwrap_or(source);
        let debut = production
            .find(&format!("pub(super) async fn {nom}("))
            .unwrap_or_else(|| panic!("handler `{nom}` introuvable dans le source"));
        let reste = &production[debut..];
        // Jusqu'au prochain élément de premier niveau, ou la fin.
        let fin = reste[1..]
            .find("\npub(super) async fn ")
            .or_else(|| reste[1..].find("\nfn "))
            .or_else(|| reste[1..].find("\n#[derive("))
            .map(|i| i + 1)
            .unwrap_or(reste.len());
        reste[..fin].to_string()
    }

    /// LES TROIS chemins d'import annoncent — y compris Plex.
    ///
    /// Les deux tests de comportement ci-dessus couvrent Roon et JRiver. Plex
    /// interroge un serveur distant : l'éprouver demanderait un serveur HTTP
    /// simulé, banni ici pour son instabilité. Ce garde-fou statique le couvre
    /// quand même — c'est le motif « un chemin corrigé, les autres nus » qu'on
    /// veut empêcher de revenir, et il tombe si l'annonce disparaît de N'IMPORTE
    /// LEQUEL des trois.
    #[test]
    fn les_trois_imports_annoncent_la_bibliotheque_modifiee() {
        let source = include_str!("import.rs");
        for nom in ["import_roon", "import_plex", "import_jriver"] {
            let corps = corps_du_handler(source, nom);
            assert!(
                corps.contains("annoncer_bibliotheque_modifiee(&event_bus"),
                "`{nom}` écrit des albums en base sans annoncer `library.updated` : \
                 la vue bibliothèque du client ne rechargera pas (#2186)"
            );
        }
    }

    /// Contre-épreuve du détecteur lui-même.
    ///
    /// Un garde-fou qui trouve l'annonce partout est un garde-fou de façade. On
    /// lui donne donc un handler nu, et un fichier dont SEUL le module de test
    /// contient l'annonce — les deux cas qui le feraient mentir.
    #[test]
    fn le_detecteur_de_handler_ne_se_prouve_pas_lui_meme() {
        let nu = "\npub(super) async fn import_roon(x: u8) -> u8 {\n    x\n}\n\
                  \npub(super) async fn import_plex(y: u8) -> u8 {\n    y\n}\n";
        assert!(
            !corps_du_handler(nu, "import_roon").contains("annoncer_bibliotheque_modifiee"),
            "un handler nu ne doit pas passer pour annoncant"
        );

        // Le corps s'arrête bien au handler suivant : sans cela, l'annonce de
        // `import_plex` couvrirait `import_roon` resté muet.
        let voisin = "\npub(super) async fn import_roon(x: u8) -> u8 {\n    x\n}\n\
                      \npub(super) async fn import_plex(y: u8) -> u8 {\n    \
                      annoncer_bibliotheque_modifiee(&event_bus, \"plex_import\", 1);\n    y\n}\n";
        assert!(
            !corps_du_handler(voisin, "import_roon").contains("annoncer_bibliotheque_modifiee"),
            "l'annonce du handler VOISIN ne doit pas compter pour celui-ci"
        );
        assert!(
            corps_du_handler(voisin, "import_plex").contains("annoncer_bibliotheque_modifiee"),
            "le handler qui annonce vraiment doit etre reconnu"
        );

        // Le module de test ne doit rien prouver.
        let seulement_en_test = "\npub(super) async fn import_roon(x: u8) -> u8 {\n    x\n}\n\
             \n#[cfg(test)]\nmod tests {\n    \
             fn t() { annoncer_bibliotheque_modifiee(&event_bus, \"roon_import\", 1); }\n}\n";
        assert!(
            !corps_du_handler(seulement_en_test, "import_roon")
                .contains("annoncer_bibliotheque_modifiee"),
            "le module de test ne doit pas servir de preuve au code de production"
        );
    }
}
