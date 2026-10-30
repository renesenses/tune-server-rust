//! Reprise de lecture (#7280) : un instantané par contexte réellement démarré.
//! Les identifiants de service restent des chaînes ; aucune URL de flux résolue.
use super::backend::{DbBackend, ToSqlValue};
use super::play_queue_repo::{PlayQueueRepo, QueueEntry, QueueInput};
use crate::orchestrator::ContexteEcoute;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

pub const REWIND_KEY: &str = "continue_listening_rewind_seconds";
pub const DEFAULT_REWIND: u64 = 10;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Resume {
    pub resume_key: String,
    pub context_type: String,
    pub context_id: String,
    pub source: String,
    pub title: String,
    pub last_track_title: String,
    pub cover_path: Option<String>,
    pub album_id: Option<i64>,
    pub position: usize,
    #[serde(default)]
    pub shuffled: bool,
    #[serde(default)]
    pub queue_row_ids: Vec<i64>,
    pub position_ms: i64,
    pub updated_at: String,
    pub items: Vec<QueueInput>,
}

fn input(e: &QueueEntry) -> QueueInput {
    if let Some(track_id) = e.track_id {
        return QueueInput::Local { track_id };
    }
    QueueInput::Streaming {
        source: e.source.clone().unwrap_or_else(|| "local".into()),
        source_id: e.source_id.clone().unwrap_or_default(),
        title: e.title.clone().unwrap_or_default(),
        artist: e.artist_name.clone().unwrap_or_default(),
        album: e.album_title.clone(),
        cover_url: e.cover_path.clone(),
        duration_ms: e.duration_ms.unwrap_or(0),
        track_number: e.track_number,
        disc_number: e.disc_number,
        album_ref: e.album_ref.clone(),
        artist_ref: e.artist_ref.clone(),
    }
}

pub fn save(db: &Arc<dyn DbBackend>, zone_id: i64, r: &Resume) -> Result<(), String> {
    let payload = serde_json::to_string(r).map_err(|e| e.to_string())?;
    db.execute("INSERT INTO listening_resumes (resume_key, zone_id, context_type, context_id, source, payload, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?) ON CONFLICT(zone_id, context_type, context_id, source) DO UPDATE SET payload = excluded.payload, updated_at = excluded.updated_at",
        &[&r.resume_key as &dyn ToSqlValue, &zone_id, &r.context_type, &r.context_id, &r.source, &payload, &r.updated_at])?;
    Ok(())
}

pub fn load(db: &Arc<dyn DbBackend>, key: &str) -> Result<Option<Resume>, String> {
    db.query_one(
        "SELECT payload, resume_key FROM listening_resumes WHERE resume_key = ?",
        &[&key as &dyn ToSqlValue],
    )?
    .map(|row| decode(&row))
    .transpose()
}
fn decode(row: &[super::backend::SqlValue]) -> Result<Resume, String> {
    let mut r: Resume =
        serde_json::from_str(&row[0].as_string().unwrap_or_default()).map_err(|e| e.to_string())?;
    // L'upsert garde la clé opaque de la première écoute.
    r.resume_key = row[1].as_string().unwrap_or_default();
    Ok(r)
}
pub fn recent(
    db: &Arc<dyn DbBackend>,
    zone_id: Option<i64>,
    limit: i64,
) -> Result<Vec<Resume>, String> {
    let rows = match zone_id {
        Some(z) => db.query_many("SELECT payload, resume_key FROM listening_resumes WHERE zone_id = ? ORDER BY updated_at DESC, resume_key LIMIT ?", &[&z as &dyn ToSqlValue, &limit])?,
        None => db.query_many("SELECT payload, resume_key FROM listening_resumes ORDER BY updated_at DESC, resume_key LIMIT ?", &[&limit as &dyn ToSqlValue])?,
    };
    let mut seen = std::collections::HashSet::new();
    rows.iter()
        .map(|row| decode(row))
        .collect::<Result<Vec<_>, _>>()
        .map(|rows| {
            rows.into_iter()
                .filter(|r| {
                    seen.insert((
                        r.context_type.clone(),
                        r.context_id.clone(),
                        r.source.clone(),
                    ))
                })
                .collect()
        })
}

/// Appelé uniquement par l'annonce d'un démarrage, jamais par queue/add.
#[allow(clippy::too_many_arguments)]
pub fn started(
    db: &Arc<dyn DbBackend>,
    zone_id: i64,
    c: &ContexteEcoute<'_>,
    source: &str,
    source_id: Option<&str>,
    title: &str,
    album: Option<&str>,
    cover: Option<&str>,
) -> Result<(), String> {
    let nature = c.nature.unwrap_or("track");
    if !matches!(nature, "album" | "playlist" | "track") {
        return Ok(());
    }
    let queue = PlayQueueRepo::with_backend(db.clone()).get_ordered(zone_id)?;
    let current = queue.iter().find(|e| e.is_current).or_else(|| {
        queue
            .iter()
            .find(|e| source_id.is_some() && e.source_id.as_deref() == source_id)
    });
    let Some(current) = current else {
        return Ok(());
    };
    let fallback = current
        .track_id
        .map(|id| id.to_string())
        .or_else(|| source_id.map(str::to_string));
    let Some(id) = c.id.or(fallback.as_deref()) else {
        return Ok(());
    };
    let service = c.service.unwrap_or(source);
    let mut selected = Vec::new();
    let mut queue_row_ids = Vec::new();
    let mut position = None;
    let contexts = db.query_many(
        "SELECT id, context_type, context_id, context_source FROM queue_items WHERE zone_id = ?",
        &[&zone_id as &dyn ToSqlValue],
    )?;
    let contexts: std::collections::HashMap<i64, _> = contexts
        .into_iter()
        .filter_map(|row| row[0].as_i64().map(|id| (id, row)))
        .collect();
    let current_explicit = contexts
        .get(&current.id)
        .is_some_and(|x| x[1].as_string().is_some());
    // Une tête lancée par /play hérite de la session ; sa suite ajoutée
    // par le client peut porter le même contexte explicitement. La tête
    // déjà prouvée dans CET instantané reste membre du conteneur lorsque
    // la lecture arrive sur une ligne explicite. Ne pas admettre les lignes
    // héritées d'un autre contexte (playlist ajoutée pendant un album).
    let inherited_rows = if current_explicit {
        db.query_one(
            "SELECT payload, resume_key FROM listening_resumes WHERE zone_id = ? AND context_type = ? AND context_id = ? AND source = ?",
            &[&zone_id as &dyn ToSqlValue, &nature, &id, &service],
        )?
        .map(|row| decode(&row))
        .transpose()?
        .map(|r| r.queue_row_ids.into_iter().collect::<std::collections::HashSet<_>>())
        .unwrap_or_default()
    } else {
        std::collections::HashSet::new()
    };
    for e in &queue {
        let explicit = contexts.get(&e.id);
        let belongs = explicit.is_none_or(|x| {
            x[1].as_string().map_or_else(
                || !current_explicit || inherited_rows.contains(&e.id),
                |n| n == nature,
            ) && x[2].as_string().is_none_or(|v| v == id)
                && x[3].as_string().is_none_or(|v| v == service)
        });
        // #7401 — un « titre » n'est pas forcément joué seul. Une tête de
        // liste lancée par /play (titre d'une playlist de service, « Tout
        // lire » de favoris, titre joué dans son album, #5372) laisse sa
        // suite en lignes SANS contexte explicite : elles suivent la session,
        // donc ce titre. Les garder, sinon la reprise rejoue le titre à sa
        // position puis s'arrête. Une ligne explicite n'entre que si elle
        // nomme CE titre (file déjà restaurée) : un titre ajouté seul
        // (`titre_seul`, sans identifiant) reste un autre geste.
        let belongs = if nature == "track" && e.id != current.id {
            explicit.is_some_and(|x| match x[1].as_string() {
                Some(n) => {
                    n == "track"
                        && x[2].as_string().as_deref() == Some(id)
                        && x[3].as_string().is_none_or(|v| v == service)
                }
                None => !current_explicit || inherited_rows.contains(&e.id),
            })
        } else {
            belongs
        };
        if !belongs {
            continue;
        }
        if e.id == current.id {
            position = Some(selected.len());
        }
        selected.push(input(e));
        queue_row_ids.push(e.id);
    }
    let Some(position) = position else {
        return Ok(());
    };
    let (container_title, container_cover) = match (nature, service, id.parse::<i64>()) {
        ("album", "local", Ok(a)) => db
            .query_one(
                "SELECT title, cover_path FROM albums WHERE id = ?",
                &[&a as &dyn ToSqlValue],
            )?
            .map(|r| (r[0].as_string(), r[1].as_string()))
            .unwrap_or_default(),
        ("playlist", "local", Ok(p)) => db
            .query_one(
                "SELECT name FROM playlists WHERE id = ?",
                &[&p as &dyn ToSqlValue],
            )?
            .map(|r| (r[0].as_string(), None))
            .unwrap_or_default(),
        _ => (None, None),
    };
    let pending_key = format!("continue_listening_shuffle_row_{zone_id}");
    let settings = super::settings_repo::SettingsRepo::with_backend(db.clone());
    let pending_shuffle = settings
        .get(&pending_key)?
        .and_then(|v| v.parse::<i64>().ok())
        .is_some_and(|row| queue_row_ids.contains(&row));
    let r = Resume {
        resume_key: uuid::Uuid::new_v4().to_string(),
        context_type: nature.into(),
        context_id: id.into(),
        source: service.into(),
        title: if nature == "track" {
            title.into()
        } else {
            c.titre
                .map(str::to_string)
                .or(container_title)
                .or_else(|| album.map(str::to_string))
                .unwrap_or_else(|| title.into())
        },
        last_track_title: title.into(),
        cover_path: c
            .pochette
            .map(str::to_string)
            .or(container_cover)
            .or_else(|| {
                (nature != "playlist")
                    .then(|| cover.map(str::to_string))
                    .flatten()
            }),
        album_id: (nature == "album" && service == "local")
            .then(|| id.parse().ok())
            .flatten(),
        position,
        shuffled: c.rang.is_none() || pending_shuffle || db.query_one(
            "SELECT payload, resume_key FROM listening_resumes WHERE zone_id = ? AND context_type = ? AND context_id = ? AND source = ?",
            &[&zone_id as &dyn ToSqlValue, &nature, &id, &service],
        )?.map(|row| decode(&row)).transpose()?.is_some_and(|r| r.shuffled && r.queue_row_ids.contains(&current.id)),
        queue_row_ids,
        position_ms: 0,
        updated_at: chrono::Utc::now().to_rfc3339(),
        items: selected,
    };
    save(db, zone_id, &r)?;
    if pending_shuffle {
        settings.delete(&pending_key)?;
    }
    Ok(())
}

/// L'ordre aléatoire d'un client est déjà matérialisé : il doit rester
/// mémorisé même si le drapeau d'aléatoire de la zone est faux.
/// Ne crée jamais de contexte qui n'a pas réellement démarré.
pub fn mark_current_shuffled(db: &Arc<dyn DbBackend>, zone_id: i64) -> Result<(), String> {
    let queue = PlayQueueRepo::with_backend(db.clone()).get_ordered(zone_id)?;
    // Le mélange fourni par le client démarre à sa tête. Les branches
    // de service historiques n'ont pas toujours posé is_current à ce stade.
    let Some(current) = queue
        .iter()
        .find(|e| e.is_current)
        .or_else(|| queue.first())
    else {
        return Ok(());
    };
    if let Some(mut r) = recent(db, Some(zone_id), 200)?
        .into_iter()
        .find(|r| r.queue_row_ids.contains(&current.id))
    {
        r.shuffled = true;
        save(db, zone_id, &r)?;
    } else {
        // Le navigateur confirme son démarrage plus tard : garder seulement
        // l'intention liée à cette file, sans publier de reprise prématurée.
        super::settings_repo::SettingsRepo::with_backend(db.clone()).set(
            &format!("continue_listening_shuffle_row_{zone_id}"),
            &current.id.to_string(),
        )?;
    }
    Ok(())
}

pub fn rewind(position_ms: i64, seconds: u64) -> u64 {
    (position_ms.max(0) as u64).saturating_sub(seconds.min(300) * 1000)
}

/// Ne met à jour qu'un titre seul réellement démarré et encore courant.
/// Le rang de file distingue aussi deux occurrences du même titre.
pub fn track_position(
    db: &Arc<dyn DbBackend>,
    state: &crate::playback::ZoneState,
) -> Result<(), String> {
    let Some(np) = state.now_playing.as_ref() else {
        return Ok(());
    };
    let queue = PlayQueueRepo::with_backend(db.clone()).get_ordered(state.zone_id)?;
    let correspond = |e: &&QueueEntry| match e.track_id {
        Some(id) => np.source == "local" && np.track_id == Some(id),
        None => {
            e.source.as_deref() == Some(np.source.as_str())
                && e.source_id
                    .as_deref()
                    .is_some_and(|id| np.source_id.as_deref() == Some(id))
        }
    };
    // Les anciennes branches de lancement de service ne posent pas toutes
    // is_current. Leur curseur public reste valable, à condition de vérifier
    // l'identité de la piste : ne jamais déplacer une autre reprise.
    let current = queue
        .iter()
        .filter(|e| e.is_current)
        .find(correspond)
        .or_else(|| {
            usize::try_from(state.queue_position)
                .ok()
                .and_then(|position| queue.get(position))
                .filter(correspond)
        });
    let Some(current) = current else {
        return Ok(());
    };
    // Les instantanés album/playlist peuvent contenir une très grande file :
    // ne pas les décoder à chaque relevé d'une position de titre seul.
    let rows = db.query_many(
        "SELECT payload, resume_key FROM listening_resumes WHERE zone_id = ? AND context_type = 'track' ORDER BY updated_at DESC LIMIT 200",
        &[&state.zone_id as &dyn ToSqlValue],
    )?;
    for row in rows {
        let mut r = decode(&row)?;
        if r.context_type != "track" || !r.queue_row_ids.contains(&current.id) {
            continue;
        }
        // #7401 — un titre suivi de sa suite : c'est le titre au rang
        // mémorisé qui joue, pas forcément le premier de l'instantané.
        if r.queue_row_ids.get(r.position) != Some(&current.id) {
            continue;
        }
        let matches = r.items.get(r.position).is_some_and(|i| match i {
            QueueInput::Local { track_id } => {
                np.source == "local" && np.track_id == Some(*track_id)
            }
            QueueInput::Streaming {
                source, source_id, ..
            } => np.source == *source && np.source_id.as_deref() == Some(source_id.as_str()),
        });
        if matches && r.position_ms / 1000 != state.position_ms.max(0) / 1000 {
            r.position_ms = state.position_ms.max(0);
            r.updated_at = chrono::Utc::now().to_rfc3339();
            save(db, state.zone_id, &r)?;
        }
    }
    Ok(())
}

/// Podcast : ne met à jour que l'épisode qui est encore celui de la zone.
pub fn podcast_position(
    db: &Arc<dyn DbBackend>,
    zone_id: i64,
    url: &str,
    position_ms: i64,
) -> Result<(), String> {
    let rows = recent(db, Some(zone_id), 80)?;
    if let Some(mut r) = rows.into_iter().find(|r| {
        r.context_type == "podcast"
            && r.items.first().is_some_and(
                |i| matches!(i, QueueInput::Streaming { source_id, .. } if source_id == url),
            )
    }) {
        if r.position_ms / 1000 != position_ms.max(0) / 1000 {
            r.position_ms = position_ms.max(0);
            r.updated_at = chrono::Utc::now().to_rfc3339();
            save(db, zone_id, &r)?;
        }
    }
    Ok(())
}

pub fn podcast_started(
    db: &Arc<dyn DbBackend>,
    zone_id: i64,
    np: &crate::playback::NowPlaying,
    position_ms: i64,
) -> Result<(), String> {
    podcast_started_in_context(db, zone_id, np, position_ms, None)
}
pub fn podcast_started_in_context(
    db: &Arc<dyn DbBackend>,
    zone_id: i64,
    np: &crate::playback::NowPlaying,
    position_ms: i64,
    context_id: Option<&str>,
) -> Result<(), String> {
    let Some(url) = np.source_id.as_deref().filter(|_| np.source == "podcast") else {
        return Ok(());
    };
    let r = Resume {
        resume_key: uuid::Uuid::new_v4().to_string(),
        context_type: "podcast".into(),
        context_id: context_id
            .map(str::to_string)
            .or_else(|| np.album_title.clone())
            .or_else(|| np.artist_name.clone())
            .unwrap_or_else(|| url.into()),
        source: "podcast".into(),
        title: np
            .album_title
            .clone()
            .or(np.artist_name.clone())
            .unwrap_or_else(|| np.title.clone()),
        last_track_title: np.title.clone(),
        cover_path: np.cover_path.clone(),
        album_id: None,
        position: 0,
        shuffled: false,
        queue_row_ids: Vec::new(),
        position_ms: position_ms.max(0),
        updated_at: chrono::Utc::now().to_rfc3339(),
        items: vec![QueueInput::Streaming {
            source: "podcast".into(),
            source_id: url.into(),
            title: np.title.clone(),
            artist: np.artist_name.clone().unwrap_or_default(),
            album: np.album_title.clone(),
            cover_url: np.cover_path.clone(),
            duration_ms: np.duration_ms,
            track_number: None,
            disc_number: None,
            album_ref: None,
            artist_ref: None,
        }],
    };
    save(db, zone_id, &r)
}

/// Remplacement atomique : une piste disparue refuse la reprise sans perdre
/// la file actuelle ni décaler la sélection sur un autre titre.
pub fn restore(db: &Arc<dyn DbBackend>, zone_id: i64, r: &Resume) -> Result<(), String> {
    if r.items.is_empty() || r.position >= r.items.len() {
        return Err("reprise sans titre".into());
    }
    use super::engine::{Engine, PostgresDialect, SqliteDialect};
    use super::play_queue_repo::sql;
    let insert_local = match db.engine() {
        Engine::Sqlite => sql::insert_local_at_if_exists(&SqliteDialect),
        Engine::Postgres => sql::insert_local_at_if_exists(&PostgresDialect),
    };
    let insert_stream = match db.engine() {
        Engine::Sqlite => sql::insert_streaming(&SqliteDialect),
        Engine::Postgres => sql::insert_streaming(&PostgresDialect),
    };
    db.write_tx(&mut |tx| {
        tx.execute("DELETE FROM queue_items WHERE zone_id = ?", &[&zone_id as &dyn ToSqlValue])?;
        for (i, item) in r.items.iter().enumerate() {
            let pos = i as i64;
            match item {
                QueueInput::Local { track_id } => {
                    if tx.execute(&insert_local, &[&zone_id as &dyn ToSqlValue, track_id, &pos, track_id])? != 1 { return Err(format!("piste de reprise disparue : {track_id}")); }
                }
                QueueInput::Streaming { source, source_id, title, artist, album, cover_url, duration_ms, track_number, disc_number, album_ref, artist_ref } => {
                    tx.execute(&insert_stream, &[&zone_id as &dyn ToSqlValue, &pos, source_id, title, artist, album, cover_url, duration_ms, source, track_number, disc_number, album_ref, artist_ref])?;
                }
            }
        }
        // Chaque ligne porte son origine, même si une autre file est ajoutée ensuite.
        tx.execute("UPDATE queue_items SET context_type = ?, context_id = ?, context_source = ?, context_title = ?, context_cover = ?, is_current = CASE WHEN position = ? THEN 1 ELSE 0 END WHERE zone_id = ?",
            &[&r.context_type as &dyn ToSqlValue, &r.context_id, &r.source, &r.title, &r.cover_path, &(r.position as i64), &zone_id])?;
        Ok(())
    })
}

/// Un ajout ou un mélange peut changer la suite après le démarrage de la
/// tête. Actualiser uniquement un contexte déjà prouvé par un démarrage.
pub fn refresh_started(
    db: &Arc<dyn DbBackend>,
    s: &crate::playback::ZoneState,
) -> Result<(), String> {
    if !matches!(
        s.state,
        crate::playback::PlayState::Playing | crate::playback::PlayState::Paused
    ) {
        return Ok(());
    }
    let Some(np) = s
        .now_playing
        .as_ref()
        .filter(|np| np.source != "podcast" && np.source != "radio")
    else {
        return Ok(());
    };
    let c = super::contexte_de_file::contexte_de_la_piste_jouee(
        db,
        s.zone_id,
        s.queue_position,
        np.track_id,
        np.source_id.as_deref(),
    );
    let nature = c
        .as_ref()
        .map(|c| c.nature.as_str())
        .or(s.session_context_type.as_deref())
        .unwrap_or("track");
    let fallback = np.track_id.map(|id| id.to_string());
    let id = c
        .as_ref()
        .and_then(|c| c.id.as_deref())
        .or(s.session_context_id.as_deref())
        .or(fallback.as_deref())
        .or(np.source_id.as_deref());
    let service = c
        .as_ref()
        .and_then(|c| c.service.as_deref())
        .or(s.session_context_source.as_deref())
        .unwrap_or(&np.source);
    if !recent(db, Some(s.zone_id), 200)?.iter().any(|r| {
        r.context_type == nature
            && Some(r.context_id.as_str()) == id
            && r.source == service
            && r.last_track_title == np.title
    }) {
        return Ok(());
    }
    started(
        db,
        s.zone_id,
        &ContexteEcoute {
            nature: Some(nature),
            id,
            rang: (!s.shuffle).then_some(s.queue_position),
            service: Some(service),
            titre: c
                .as_ref()
                .and_then(|c| c.titre.as_deref())
                .or(s.session_context_title.as_deref()),
            pochette: c
                .as_ref()
                .and_then(|c| c.pochette.as_deref())
                .or(s.session_context_cover.as_deref()),
        },
        &np.source,
        np.source_id.as_deref(),
        &np.title,
        np.album_title.as_deref(),
        np.cover_path.as_deref(),
    )?;
    track_position(db, s)
}

#[cfg(test)]
#[path = "listening_resume_7352_tests.rs"]
mod reprise_services_7352;
