use std::sync::Arc;
use std::time::Instant;

use serde::Serialize;
use tracing::{debug, info, warn};

use crate::cloud::rate_limit::{self, CloudScope};
use crate::db::backend::{DbBackend, ToSqlValue};
use crate::db::settings_repo::SettingsRepo;

pub const CLOUD_LIBRARY_API: &str = "https://mozaiklabs.fr/api/v1/cloud-library";
const SYNC_BATCH_SIZE: i64 = 200;

// ---------------------------------------------------------------------------
// Tune Circle T2 (#5325) — qui pousse, et ce qui ne part jamais
// ---------------------------------------------------------------------------

/// Réglage tenu par le greffon `circle` : `"true"` quand CE serveur partage sa
/// bibliothèque avec au moins un cercle (vu par le cloud au dernier relais de
/// `GET /`, `PUT` ou `DELETE …/sharing/library`).
///
/// Décision de Bertrand du 28/09/2026 (#5325) : un compte GRATUIT qui partage
/// sa bibliothèque avec un cercle pousse son catalogue — sans copie en ligne,
/// ses contacts n'auraient rien à parcourir. L'accès du propriétaire à sa
/// propre copie en ligne, lui, reste Premium (côté cloud).
pub const CLE_PARTAGE_DE_CERCLE: &str = "circle_library_sharing";

/// CE serveur partage-t-il sa bibliothèque avec un cercle ?
pub fn partage_de_cercle_actif(settings: &SettingsRepo) -> bool {
    settings
        .get(CLE_PARTAGE_DE_CERCLE)
        .ok()
        .flatten()
        .is_some_and(|v| v.trim() == "true")
}

/// La synchronisation de la copie en ligne doit-elle tourner ? Premium, ou un
/// partage de bibliothèque actif avec un cercle (#5325), même en gratuit.
pub fn synchro_autorisee(premium: bool, settings: &SettingsRepo) -> bool {
    premium || partage_de_cercle_actif(settings)
}

/// Une valeur qui a la forme d'un chemin de fichier (POSIX, Windows, UNC,
/// `~`, ou une adresse de partage de fichiers).
///
/// La copie en ligne ne porte JAMAIS de chemin (#5325) : les contacts d'un
/// cercle la parcourent. `file_path` n'est lu par aucune requête de ce module ;
/// `source_id`, lui, est un identifiant libre posé par chaque source, et rien
/// n'interdit qu'une source y range un chemin. Il ne part donc que s'il n'en a
/// pas la forme.
pub fn ressemble_a_un_chemin(valeur: &str) -> bool {
    let v = valeur.trim();
    let minuscule = v.to_ascii_lowercase();
    let octets = v.as_bytes();
    v.starts_with('/')
        || v.starts_with('\\')
        || v.starts_with('~')
        || ["file:", "smb:", "nfs:", "afp:", "ftp:", "sftp:"]
            .iter()
            .any(|p| minuscule.starts_with(p))
        || (octets.len() >= 3
            && octets[0].is_ascii_alphabetic()
            && octets[1] == b':'
            && (octets[2] == b'\\' || octets[2] == b'/'))
}

fn sans_chemin(valeur: Option<String>) -> Option<String> {
    valeur.filter(|v| !ressemble_a_un_chemin(v))
}

// ---------------------------------------------------------------------------
// Liaison du serveur au compte (#5325, contrat de site-mozaiklabs#233)
// ---------------------------------------------------------------------------

/// Réglage où vit le jeton de liaison délivré par le cloud. Un secret : jamais
/// journalisé, jamais rendu au client web.
pub const CLE_JETON_DE_LIAISON: &str = "cloud_server_link_token";

/// En-tête qui présente le jeton de liaison sur chaque `sync`.
pub const EN_TETE_JETON_DE_SERVEUR: &str = "X-Tune-Server-Token";

fn jeton_de_liaison(settings: &SettingsRepo) -> Option<String> {
    settings
        .get(CLE_JETON_DE_LIAISON)
        .ok()
        .flatten()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// Le motif `{"error": "…"}` d'un refus du cloud.
fn motif_du_refus(corps: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(corps)
        .ok()?
        .get("error")?
        .as_str()
        .map(str::to_string)
}

/// Lie ce serveur au compte connecté : `POST {api}/{server_id}/link`, jeton
/// OAuth de l'utilisateur, sans corps. Le jeton rendu est rangé dans
/// [`CLE_JETON_DE_LIAISON`] ; chaque appel le renouvelle, le précédent cesse
/// de valoir. L'erreur rendue ne porte que le statut, jamais un corps.
pub async fn lier_le_serveur(
    backend: &Arc<dyn DbBackend>,
    http_client: &reqwest::Client,
    api: &str,
    server_id: &str,
    access_token: &str,
) -> Result<(), String> {
    let reponse = http_client
        .post(format!("{api}/{server_id}/link"))
        .bearer_auth(access_token)
        .header(reqwest::header::ACCEPT, "application/json")
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await
        .map_err(|e| {
            let msg = format!("cloud link request: {}", e.without_url());
            warn!(error = %msg, "cloud_server_link_failed");
            msg
        })?;
    let statut = reponse.status();
    if !statut.is_success() {
        let msg = format!("cloud link HTTP {}", statut.as_u16());
        warn!(error = %msg, "cloud_server_link_refused");
        return Err(msg);
    }
    let corps: serde_json::Value = reponse
        .json()
        .await
        .map_err(|_| "cloud link: réponse illisible".to_string())?;
    let jeton = corps
        .get("token")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|v| !v.is_empty());
    let meme_serveur = corps.get("server_id").and_then(|v| v.as_str()) == Some(server_id);
    let Some(jeton) = jeton.filter(|_| meme_serveur) else {
        warn!("cloud_server_link_unexpected_response");
        return Err("cloud link: réponse inattendue".into());
    };
    SettingsRepo::with_backend(backend.clone())
        .set(CLE_JETON_DE_LIAISON, jeton)
        .map_err(|e| format!("cloud link: réglage non écrit: {e}"))?;
    info!("cloud_server_linked");
    Ok(())
}

/// [`lier_le_serveur`] avec ce que les réglages portent : `server_id` et
/// session SSO. Rien si l'un manque. Pour la connexion SSO et le battement de
/// compte, qui n'ont pas à connaître la racine de l'API.
pub async fn lier_depuis_les_reglages(
    backend: &Arc<dyn DbBackend>,
    http_client: &reqwest::Client,
) -> Result<(), String> {
    let settings = SettingsRepo::with_backend(backend.clone());
    let lire = |cle: &str| {
        settings
            .get(cle)
            .ok()
            .flatten()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    };
    let (Some(server_id), Some(jeton)) = (lire("server_id"), lire("mozaik_access_token")) else {
        return Err("cloud link: pas de server_id ou pas de session".into());
    };
    lier_le_serveur(backend, http_client, CLOUD_LIBRARY_API, &server_id, &jeton).await
}

/// Défait la liaison du serveur au compte, chez le cloud :
/// `DELETE {api}/{server_id}/link`, jeton OAuth de l'utilisateur ET jeton de
/// liaison (`X-Tune-Server-Token`), sans corps. Appelé par la déconnexion SSO
/// AVANT qu'elle efface les deux jetons (décision de Bertrand du 28/09/2026).
///
/// Rien ne part s'il manque le `server_id`, la session ou le jeton de liaison :
/// il n'y a alors rien à défaire, ou rien pour le prouver. `Ok(true)` : le
/// cloud a délié (2xx) — et il a EFFACÉ la copie en ligne de ce serveur.
/// `Ok(false)` : rien n'est parti. L'erreur rendue ne porte que le statut ou
/// la nature de la panne — jamais un jeton, jamais l'adresse.
pub async fn delier_le_serveur(
    settings: &SettingsRepo,
    http_client: &reqwest::Client,
    api: &str,
) -> Result<bool, String> {
    let lire = |cle: &str| {
        settings
            .get(cle)
            .ok()
            .flatten()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    };
    let (Some(server_id), Some(acces), Some(jeton)) = (
        lire("server_id"),
        lire("mozaik_access_token"),
        jeton_de_liaison(settings),
    ) else {
        return Ok(false);
    };
    let reponse = http_client
        .delete(format!("{}/{server_id}/link", api.trim_end_matches('/')))
        .bearer_auth(acces)
        .header(EN_TETE_JETON_DE_SERVEUR, jeton)
        .header(reqwest::header::ACCEPT, "application/json")
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
        .map_err(|e| format!("cloud unlink request: {}", e.without_url()))?;
    let statut = reponse.status();
    if !statut.is_success() {
        return Err(format!("cloud unlink HTTP {}", statut.as_u16()));
    }
    info!("cloud_server_unlinked");
    Ok(true)
}

/// Après une déliaison RÉUSSIE, le cloud n'a plus rien de ce serveur : la
/// copie en ligne est effacée avec la liaison. Le journal des changements,
/// lui, se croit à jour. Oublier la dernière synchro et remettre toute la
/// bibliothèque dans le journal fait regarnir la copie dès la reconnexion,
/// au lieu d'une copie vide que seuls les changements futurs rempliraient.
pub fn tout_repousser_apres_deliaison(backend: &Arc<dyn DbBackend>) {
    SettingsRepo::with_backend(backend.clone())
        .delete("cloud_library_last_sync")
        .ok();
    populate_changelog_after_scan(backend);
}

// ---------------------------------------------------------------------------
// #5358 — version du format de synchro
// ---------------------------------------------------------------------------

/// Version du format de la copie en ligne que CE binaire pousse.
///
/// Le journal des changements ne renvoie une entité que si elle change : un
/// binaire qui enrichit ce qui part (nouvelle colonne, nouveau lien) laisse
/// donc la copie déjà poussée dans l'ancien format, indéfiniment. Constaté le
/// 28/09/2026 (#5358) : la copie du .18, poussée le 23/09 sans `album_id` ni
/// MBID, montrait aux contacts d'un cercle des albums sans titres ni pochettes.
///
/// - 1 : tout ce qui a été poussé avant ce réglage (absent = 1) ;
/// - 2 : `album_id` des pistes et `musicbrainz_release_group_id` des albums
///   (Tune Circle T2, #5325).
///
/// À INCRÉMENTER à chaque enrichissement de ce qui part vers le cloud : la
/// bibliothèque entière repart alors une fois, au débit du limiteur.
pub const FORMAT_SYNCHRO: i64 = 2;

/// Réglage local : version du format dans lequel la bibliothèque a été remise
/// dans le journal pour la dernière fois.
pub const CLE_FORMAT_SYNCHRO: &str = "cloud_library_sync_format";

/// Si la copie en ligne a été poussée dans un format plus ancien que
/// [`FORMAT_SYNCHRO`] (réglage absent, illisible ou inférieur) et que la
/// synchro est autorisée, remet toute la bibliothèque dans le journal — une
/// seule fois — puis note le format. Rend `true` quand il a remis.
///
/// Rien d'autre n'est forcé : les entrées partent par le cycle ordinaire, au
/// débit du limiteur. Synchro non autorisée ⇒ rien, et le réglage n'est pas
/// écrit : la remise se fera le jour où la synchro sera allumée.
pub fn resynchroniser_si_le_format_a_change(
    backend: &Arc<dyn DbBackend>,
    settings: &SettingsRepo,
    premium: bool,
) -> bool {
    if !synchro_autorisee(premium, settings) {
        return false;
    }
    let format_pousse = settings
        .get(CLE_FORMAT_SYNCHRO)
        .ok()
        .flatten()
        .and_then(|v| v.trim().parse::<i64>().ok())
        .unwrap_or(1);
    if format_pousse >= FORMAT_SYNCHRO {
        return false;
    }
    populate_changelog_after_scan(backend);
    settings
        .set(CLE_FORMAT_SYNCHRO, &FORMAT_SYNCHRO.to_string())
        .ok();
    info!(
        ancien = format_pousse,
        nouveau = FORMAT_SYNCHRO,
        en_attente = pending_count(backend),
        "cloud_library_sync_format_upgraded — bibliotheque remise dans le journal"
    );
    true
}

/// Le serveur a-t-il déjà un jeton de liaison ?
pub fn serveur_lie(settings: &SettingsRepo) -> bool {
    jeton_de_liaison(settings).is_some()
}

// ---------------------------------------------------------------------------
// SyncReport
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct SyncReport {
    pub tracks_synced: i64,
    pub albums_synced: i64,
    pub artists_synced: i64,
    pub errors: Vec<String>,
    pub duration_ms: u64,
}

// ---------------------------------------------------------------------------
// record_change — fire-and-forget changelog insertion
// ---------------------------------------------------------------------------

/// Insert a changelog entry so the next sync push picks up this entity.
/// entity_type: "track", "album", "artist", "playlist", "favorite", "rating", "history".
/// action: "upsert" or "delete".
///
/// This is fire-and-forget — it never fails the caller.
pub fn record_change(
    backend: &Arc<dyn DbBackend>,
    entity_type: &str,
    entity_id: i64,
    action: &str,
) {
    backend
        .execute(
            "INSERT INTO sync_changelog (entity_type, entity_id, action) VALUES (?, ?, ?)",
            &[
                &entity_type.to_string() as &dyn ToSqlValue,
                &entity_id as &dyn ToSqlValue,
                &action.to_string() as &dyn ToSqlValue,
            ],
        )
        .ok(); // fire-and-forget
}

// ---------------------------------------------------------------------------
// pending_count
// ---------------------------------------------------------------------------

/// Count unsynced changelog entries.
/// Combien d'albums la synchro aurait DU pousser, et ne peut plus rattraper.
///
/// `referenced` = albums distincts cites par les pistes locales. `pushed` =
/// albums reellement envoyes pendant ce `full_sync`. `pending` = ce qui reste
/// en file d'attente et repartira au prochain cycle.
///
/// L'ecart qui compte est ce qui n'est ni pousse ni en attente : ces
/// albums-la ne sont dans aucune file et rien ne les renverra jamais. C'est
/// exactement le motif mesure en production le 11/08/2026 — 18 506 albums
/// cites par des pistes, jamais recus par le cloud, et 156 296 pistes
/// orphelines cote utilisateur (soit un quart de la navigation
/// artiste -> album -> pistes).
///
/// Ce qui est en attente n'est PAS compte comme perdu : une synchro
/// interrompue est un etat normal, pas une anomalie a signaler.
pub fn completeness_gap(referenced: i64, pushed: i64, pending: i64) -> i64 {
    (referenced - pushed - pending).max(0)
}

/// Albums distincts cites par les pistes locales — le denominateur honnete
/// d'un controle de complétude. On compte ce que les pistes REFERENCENT, pas
/// la table `albums` : c'est le lien piste -> album qui se retrouve troue cote
/// utilisateur quand un album manque.
fn albums_referenced_by_tracks(backend: &Arc<dyn DbBackend>) -> i64 {
    backend
        .query_one(
            "SELECT COUNT(DISTINCT album_id) FROM tracks WHERE album_id IS NOT NULL",
            &[],
        )
        .ok()
        .flatten()
        .and_then(|row| row.first().and_then(|v| v.as_i64()))
        .unwrap_or(0)
}

pub fn pending_count(backend: &Arc<dyn DbBackend>) -> i64 {
    backend
        .query_one("SELECT COUNT(*) FROM sync_changelog WHERE synced = 0", &[])
        .ok()
        .flatten()
        .and_then(|row| row.first().and_then(|v| v.as_i64()))
        .unwrap_or(0)
}

/// Ce qui reste a pousser, par type : (artistes, albums, pistes).
///
/// Un total seul ne dit rien de la coherence de ce qui est arrive en face.
/// « Il reste 18 000 entrees » est une information ; « il reste 18 000 albums
/// alors que toutes les pistes sont parties » est un diagnostic.
pub fn pending_by_type(backend: &Arc<dyn DbBackend>) -> (i64, i64, i64) {
    let lire = |etype: &str| -> i64 {
        backend
            .query_one(
                "SELECT COUNT(*) FROM sync_changelog WHERE synced = 0 AND entity_type = ?",
                &[&etype as &dyn ToSqlValue],
            )
            .ok()
            .flatten()
            .and_then(|row| row.first().and_then(|v| v.as_i64()))
            .unwrap_or(0)
    };
    (lire("artist"), lire("album"), lire("track"))
}

// ---------------------------------------------------------------------------
// push_changes — incremental sync
// ---------------------------------------------------------------------------

/// Read unsynced changelog entries, load entity data, POST to cloud API,
/// and mark entries as synced.  Returns a report.
pub async fn push_changes(
    backend: &Arc<dyn DbBackend>,
    http_client: &reqwest::Client,
    server_id: &str,
    access_token: &str,
) -> Result<SyncReport, String> {
    push_changes_vers(
        backend,
        http_client,
        CLOUD_LIBRARY_API,
        server_id,
        access_token,
    )
    .await
}

/// [`push_changes`] vers une racine d'API donnée — celle de production pour
/// l'appelant réel, un faux cloud pour les bancs (#5325 : la charge qui PART
/// est éprouvée, pas une charge reconstruite à côté).
#[doc(hidden)]
pub async fn push_changes_vers(
    backend: &Arc<dyn DbBackend>,
    http_client: &reqwest::Client,
    api: &str,
    server_id: &str,
    access_token: &str,
) -> Result<SyncReport, String> {
    let start = Instant::now();
    let mut report = SyncReport {
        tracks_synced: 0,
        albums_synced: 0,
        artists_synced: 0,
        errors: Vec::new(),
        duration_ms: 0,
    };
    // Une seule liaison par passage : un cloud qui refuse le jeton neuf ne
    // doit pas faire tourner la synchro en rond.
    let mut liaison_tentee = false;

    loop {
        // 1. Read a batch of unsynced changelog entries
        let batch_limit = SYNC_BATCH_SIZE;
        // Les parents AVANT les enfants, et cet ordre est explicite : sans le
        // CASE, `changed_at` seul departageait mal les entrees inserees dans la
        // meme seconde, et le tri retombait sur l'ordre d'insertion — toutes
        // les pistes, puis tous les albums.
        //
        // Ce que ca a coute (mesure en production le 2026-08-12) : une
        // bibliotheque de 584 142 pistes a pousse ses pistes pendant 25 heures,
        // par lots de 200, sans jamais atteindre ses albums. Elle s'est
        // interrompue et n'a jamais repris : 0 album, 0 artiste, et 584 142
        // pistes orphelines cote cloud. Les bibliotheques plus petites vidaient
        // leur phase « pistes » en quelques heures et n'ont rien vu.
        //
        // Le defaut etait donc latent pour tout le monde et fatal au-dessus
        // d'une certaine taille. Avec les parents d'abord, toute interruption
        // laisse un etat coherent : des albums sans toutes leurs pistes, jamais
        // des pistes sans album.
        let rows = backend
            .query_many(
                "SELECT id, entity_type, entity_id, action FROM sync_changelog \
                 WHERE synced = 0 \
                 ORDER BY CASE entity_type \
                              WHEN 'artist' THEN 0 \
                              WHEN 'album'  THEN 1 \
                              WHEN 'track'  THEN 2 \
                              ELSE 3 END, \
                          changed_at ASC, id ASC \
                 LIMIT ?",
                &[&batch_limit as &dyn ToSqlValue],
            )
            .map_err(|e| format!("changelog query: {e}"))?;

        if rows.is_empty() {
            break;
        }

        // Collect changelog entries
        let mut entries: Vec<(i64, String, i64, String)> = Vec::new();
        for row in &rows {
            let id = row.first().and_then(|v| v.as_i64()).unwrap_or(0);
            let etype = row.get(1).and_then(|v| v.as_string()).unwrap_or_default();
            let eid = row.get(2).and_then(|v| v.as_i64()).unwrap_or(0);
            let action = row.get(3).and_then(|v| v.as_string()).unwrap_or_default();
            entries.push((id, etype, eid, action));
        }

        // 2. Group by entity_type
        let mut track_ids: Vec<i64> = Vec::new();
        let mut album_ids: Vec<i64> = Vec::new();
        let mut artist_ids: Vec<i64> = Vec::new();
        let mut changelog_ids: Vec<i64> = Vec::new();
        let mut changes: Vec<serde_json::Value> = Vec::new();

        for (cl_id, etype, eid, action) in &entries {
            changelog_ids.push(*cl_id);
            match etype.as_str() {
                "track" => track_ids.push(*eid),
                "album" => album_ids.push(*eid),
                "artist" => artist_ids.push(*eid),
                _ => {
                    // For non-entity types (playlist, favorite, rating, history),
                    // just record the change with no data payload
                    changes.push(serde_json::json!({
                        "type": etype,
                        "action": action,
                        "id": eid,
                        "data": null,
                    }));
                }
            }
        }

        // 3. Load full entity data for tracks
        if !track_ids.is_empty() {
            let placeholders = track_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            let sql = format!(
                "SELECT t.id, t.title, ar.name, al.title, t.format, t.sample_rate, t.bit_depth, \
                 t.duration_ms, t.genre, t.track_number, t.disc_number, t.source, t.source_id, \
                 t.isrc, t.album_id \
                 FROM tracks t \
                 LEFT JOIN artists ar ON t.artist_id = ar.id \
                 LEFT JOIN albums al ON t.album_id = al.id \
                 WHERE t.id IN ({placeholders})"
            );
            let params: Vec<&dyn ToSqlValue> =
                track_ids.iter().map(|id| id as &dyn ToSqlValue).collect();
            if let Ok(trows) = backend.query_many(&sql, &params) {
                for r in &trows {
                    let tid = r.first().and_then(|v| v.as_i64()).unwrap_or(0);
                    let action = entries
                        .iter()
                        .find(|(_, et, eid, _)| et == "track" && *eid == tid)
                        .map(|(_, _, _, a)| a.as_str())
                        .unwrap_or("upsert");
                    changes.push(serde_json::json!({
                        "type": "track",
                        "action": action,
                        "id": tid,
                        "data": {
                            "title": r.get(1).and_then(|v| v.as_string()),
                            "artist_name": r.get(2).and_then(|v| v.as_string()),
                            "album_title": r.get(3).and_then(|v| v.as_string()),
                            // #5325 : relie la piste à son album sans passer
                            // par le titre — l'`id` de l'album poussé plus bas.
                            "album_id": r.get(14).and_then(|v| v.as_i64()),
                            "format": r.get(4).and_then(|v| v.as_string()),
                            "sample_rate": r.get(5).and_then(|v| v.as_i64()),
                            "bit_depth": r.get(6).and_then(|v| v.as_i64()),
                            "duration_ms": r.get(7).and_then(|v| v.as_i64()),
                            "genre": r.get(8).and_then(|v| v.as_string()),
                            "track_number": r.get(9).and_then(|v| v.as_i64()),
                            "disc_number": r.get(10).and_then(|v| v.as_i64()),
                            "source": r.get(11).and_then(|v| v.as_string()),
                            // Jamais un chemin (#5325) : voir `ressemble_a_un_chemin`.
                            "source_id": sans_chemin(r.get(12).and_then(|v| v.as_string())),
                            // ISRC — an exact recording code the cloud resolves
                            // against MusicBrainz (metadata:resolve-isrc). Already
                            // extracted from local tags + Tidal/Qobuz.
                            "isrc": r.get(13).and_then(|v| v.as_string()),
                        }
                    }));
                    report.tracks_synced += 1;
                }
            }
            // Handle deletes — tracks that no longer exist in DB
            for tid in &track_ids {
                let already_in_changes = changes
                    .iter()
                    .any(|c| c["type"] == "track" && c["id"].as_i64() == Some(*tid));
                if !already_in_changes {
                    let action = entries
                        .iter()
                        .find(|(_, et, eid, _)| et == "track" && *eid == *tid)
                        .map(|(_, _, _, a)| a.as_str())
                        .unwrap_or("delete");
                    changes.push(serde_json::json!({
                        "type": "track",
                        "action": action,
                        "id": tid,
                        "data": null,
                    }));
                    report.tracks_synced += 1;
                }
            }
        }

        // 4. Load full entity data for albums
        if !album_ids.is_empty() {
            let placeholders = album_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            let sql = format!(
                "SELECT a.id, a.title, ar.name, a.year, a.genre, a.source, \
                 (SELECT COUNT(*) FROM tracks t WHERE t.album_id = a.id) AS track_count, \
                 a.musicbrainz_release_group_id \
                 FROM albums a \
                 LEFT JOIN artists ar ON a.artist_id = ar.id \
                 WHERE a.id IN ({placeholders})"
            );
            let params: Vec<&dyn ToSqlValue> =
                album_ids.iter().map(|id| id as &dyn ToSqlValue).collect();
            if let Ok(arows) = backend.query_many(&sql, &params) {
                for r in &arows {
                    let aid = r.first().and_then(|v| v.as_i64()).unwrap_or(0);
                    let action = entries
                        .iter()
                        .find(|(_, et, eid, _)| et == "album" && *eid == aid)
                        .map(|(_, _, _, a)| a.as_str())
                        .unwrap_or("upsert");
                    changes.push(serde_json::json!({
                        "type": "album",
                        "action": action,
                        "id": aid,
                        "data": {
                            "title": r.get(1).and_then(|v| v.as_string()),
                            "artist_name": r.get(2).and_then(|v| v.as_string()),
                            "year": r.get(3).and_then(|v| v.as_i64()),
                            "genre": r.get(4).and_then(|v| v.as_string()),
                            "source": r.get(5).and_then(|v| v.as_string()),
                            "track_count": r.get(6).and_then(|v| v.as_i64()),
                            // #5325 (décision du 28/09) : la pochette publique
                            // se retrouve par MusicBrainz, rien d'autre ne part.
                            "musicbrainz_release_group_id": r
                                .get(7)
                                .and_then(|v| v.as_string())
                                .filter(|v| !v.trim().is_empty()),
                        }
                    }));
                    report.albums_synced += 1;
                }
            }
            for aid in &album_ids {
                let already = changes
                    .iter()
                    .any(|c| c["type"] == "album" && c["id"].as_i64() == Some(*aid));
                if !already {
                    changes.push(serde_json::json!({
                        "type": "album",
                        "action": "delete",
                        "id": aid,
                        "data": null,
                    }));
                    report.albums_synced += 1;
                }
            }
        }

        // 5. Load full entity data for artists
        if !artist_ids.is_empty() {
            let placeholders = artist_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            let sql = format!(
                "SELECT id, name, bio, musicbrainz_id FROM artists WHERE id IN ({placeholders})"
            );
            let params: Vec<&dyn ToSqlValue> =
                artist_ids.iter().map(|id| id as &dyn ToSqlValue).collect();
            if let Ok(rrows) = backend.query_many(&sql, &params) {
                for r in &rrows {
                    let rid = r.first().and_then(|v| v.as_i64()).unwrap_or(0);
                    let action = entries
                        .iter()
                        .find(|(_, et, eid, _)| et == "artist" && *eid == rid)
                        .map(|(_, _, _, a)| a.as_str())
                        .unwrap_or("upsert");
                    changes.push(serde_json::json!({
                        "type": "artist",
                        "action": action,
                        "id": rid,
                        "data": {
                            "name": r.get(1).and_then(|v| v.as_string()),
                            "bio": r.get(2).and_then(|v| v.as_string()),
                            "musicbrainz_id": r.get(3).and_then(|v| v.as_string()),
                        }
                    }));
                    report.artists_synced += 1;
                }
            }
            for rid in &artist_ids {
                let already = changes
                    .iter()
                    .any(|c| c["type"] == "artist" && c["id"].as_i64() == Some(*rid));
                if !already {
                    changes.push(serde_json::json!({
                        "type": "artist",
                        "action": "delete",
                        "id": rid,
                        "data": null,
                    }));
                    report.artists_synced += 1;
                }
            }
        }

        // 6. POST batch to cloud API
        if !changes.is_empty() {
            // Un 429 du cloud est persisté EN BASE (CLD-1) : la synchro respecte
            // le `Retry-After` reçu, redémarrage compris, au lieu de le
            // redécouvrir à chaque cycle.
            let settings = SettingsRepo::with_backend(backend.clone());
            let payload = serde_json::json!({
                "server_id": server_id,
                "changes": changes,
            });

            // Liaison du serveur au compte (#5325) : le jeton accompagne
            // chaque `sync`. Absent, il est demandé une fois avant d'envoyer.
            let mut jeton = jeton_de_liaison(&settings);
            if jeton.is_none() && !liaison_tentee {
                liaison_tentee = true;
                if lier_le_serveur(backend, http_client, api, server_id, access_token)
                    .await
                    .is_ok()
                {
                    jeton = jeton_de_liaison(&settings);
                }
            }

            let mut echec = false;
            loop {
                let mut requete = http_client
                    .post(format!("{api}/{server_id}/sync"))
                    .bearer_auth(access_token)
                    .json(&payload)
                    .timeout(std::time::Duration::from_secs(30));
                if let Some(j) = &jeton {
                    requete = requete.header(EN_TETE_JETON_DE_SERVEUR, j.as_str());
                }
                // CLD-2 : un seul chemin d'appel borné — la portée retenue ne
                // part pas, un 429 mémorise son échéance avant d'être lu ici.
                let appel = rate_limit::appeler(&settings, CloudScope::LibrarySync, requete).await;
                match appel {
                    rate_limit::AppelCloud::Retenu(backoff) => {
                        debug!(
                            scope = backoff.scope,
                            until_epoch = backoff.until_epoch,
                            retry_after_seconds = backoff.retry_after_seconds,
                            "cloud_library_sync_deferred_rate_limit"
                        );
                        echec = true;
                    }
                    rate_limit::AppelCloud::Reponse(resp) if resp.status().is_success() => {
                        debug!(
                            batch_size = changes.len(),
                            "cloud_library_sync_batch_pushed"
                        );
                    }
                    rate_limit::AppelCloud::Reponse(resp) => {
                        let status = resp.status();
                        // 429 (throttled) and 5xx are expected transient conditions
                        // from the community cloud — not a failure. Stop this batch
                        // and retry next cycle quietly instead of spamming a scary
                        // "batch_failed" warning (Jean Valjean saw 429s in his log).
                        // Mirrors the bio_sync throttle handling. Le 429 est déjà
                        // mémorisé par `rate_limit::appeler`.
                        if status.as_u16() == 429 || status.is_server_error() {
                            debug!(status = %status, "cloud_library_sync_throttled — retry next cycle");
                            echec = true;
                            break;
                        }
                        let body = resp.text().await.unwrap_or_default();
                        let motif = motif_du_refus(&body);
                        // Jeton ancien ou faux, ou serveur pas encore lié :
                        // refaire la liaison, puis réessayer UNE fois.
                        if matches!(
                            (status.as_u16(), motif.as_deref()),
                            (401, Some("server_token_invalid")) | (403, Some("server_not_linked"))
                        ) && !liaison_tentee
                        {
                            liaison_tentee = true;
                            if lier_le_serveur(backend, http_client, api, server_id, access_token)
                                .await
                                .is_ok()
                            {
                                jeton = jeton_de_liaison(&settings);
                                continue;
                            }
                        }
                        let msg = format!("cloud sync HTTP {status}: {body}");
                        if motif.as_deref() == Some("premium_required") {
                            // Compte gratuit dont aucun cercle ne partage ce
                            // serveur : attendu, pas une panne.
                            debug!(error = %msg, "cloud_library_sync_premium_required");
                        } else {
                            warn!(error = %msg, "cloud_library_sync_batch_failed");
                        }
                        report.errors.push(msg);
                        echec = true;
                    }
                    rate_limit::AppelCloud::Erreur(e) => {
                        let msg = format!("cloud sync request: {e}");
                        warn!(error = %msg, "cloud_library_sync_request_failed");
                        report.errors.push(msg);
                        echec = true;
                    }
                }
                break;
            }
            // Don't mark as synced on failure — will retry next cycle
            if echec {
                break;
            }
        }

        // 7. Mark changelog entries as synced
        if !changelog_ids.is_empty() {
            let placeholders = changelog_ids
                .iter()
                .map(|_| "?")
                .collect::<Vec<_>>()
                .join(",");
            let sql = format!("UPDATE sync_changelog SET synced = 1 WHERE id IN ({placeholders})");
            let params: Vec<&dyn ToSqlValue> = changelog_ids
                .iter()
                .map(|id| id as &dyn ToSqlValue)
                .collect();
            backend.execute(&sql, &params).ok();
        }
    }

    report.duration_ms = start.elapsed().as_millis() as u64;
    Ok(report)
}

// ---------------------------------------------------------------------------
// full_sync — push entire library
// ---------------------------------------------------------------------------

/// Queue the entire library for cloud sync by inserting all tracks, albums,
/// and artists into sync_changelog as "upsert", then push changes in a loop.
pub async fn full_sync(
    backend: &Arc<dyn DbBackend>,
    http_client: &reqwest::Client,
    server_id: &str,
    access_token: &str,
) -> Result<SyncReport, String> {
    info!("cloud_library_full_sync_starting");

    // Bulk-insert all entities that aren't already pending
    backend
        .execute_batch(
            "INSERT INTO sync_changelog (entity_type, entity_id, action) \
             SELECT 'track', id, 'upsert' FROM tracks \
             WHERE id NOT IN (SELECT entity_id FROM sync_changelog WHERE entity_type='track' AND synced=0);\
             INSERT INTO sync_changelog (entity_type, entity_id, action) \
             SELECT 'album', id, 'upsert' FROM albums \
             WHERE id NOT IN (SELECT entity_id FROM sync_changelog WHERE entity_type='album' AND synced=0);\
             INSERT INTO sync_changelog (entity_type, entity_id, action) \
             SELECT 'artist', id, 'upsert' FROM artists \
             WHERE id NOT IN (SELECT entity_id FROM sync_changelog WHERE entity_type='artist' AND synced=0);",
        )
        .map_err(|e| format!("full_sync bulk insert: {e}"))?;
    // #5358 : toute la bibliothèque vient de repartir dans le format courant ;
    // le cycle suivant n'a pas à la remettre une seconde fois.
    SettingsRepo::with_backend(backend.clone())
        .set(CLE_FORMAT_SYNCHRO, &FORMAT_SYNCHRO.to_string())
        .ok();

    let total_pending = pending_count(backend);
    info!(pending = total_pending, "cloud_library_full_sync_queued");

    // Push in batches until done
    let mut combined = SyncReport {
        tracks_synced: 0,
        albums_synced: 0,
        artists_synced: 0,
        errors: Vec::new(),
        duration_ms: 0,
    };
    let start = Instant::now();

    loop {
        let remaining = pending_count(backend);
        if remaining == 0 {
            break;
        }

        match push_changes(backend, http_client, server_id, access_token).await {
            Ok(batch_report) => {
                combined.tracks_synced += batch_report.tracks_synced;
                combined.albums_synced += batch_report.albums_synced;
                combined.artists_synced += batch_report.artists_synced;
                combined.errors.extend(batch_report.errors);
            }
            Err(e) => {
                combined.errors.push(e);
                break;
            }
        }
    }

    combined.duration_ms = start.elapsed().as_millis() as u64;

    // Controle de completude. Une synchronisation qui s'arrete en laissant des
    // parents en attente produit des pistes sans album cote cloud — c'est ce
    // qui est arrive en juillet 2026, et rien ne l'a signale pendant six
    // semaines. Le dire ici le rend visible a la source.
    let (artistes, albums, pistes) = pending_by_type(backend);
    if artistes > 0 || albums > 0 {
        warn!(
            artists_pending = artistes,
            albums_pending = albums,
            tracks_pending = pistes,
            "cloud_library_full_sync_incomplete — des parents restent en attente, \
             la bibliotheque cloud sera trouee jusqu'au prochain passage"
        );
    }

    info!(
        tracks = combined.tracks_synced,
        albums = combined.albums_synced,
        artists = combined.artists_synced,
        errors = combined.errors.len(),
        duration_ms = combined.duration_ms,
        "cloud_library_full_sync_complete"
    );

    // Controle de complétude — signaler l'ecart A LA SOURCE.
    //
    // L'ecart de 18 506 albums a ete decouvert trois mois plus tard, dans un
    // back-office, en construisant autre chose. Rien, cote serveur, n'avait
    // signale que la moitie du travail manquait. Ces trois lignes rendent le
    // probleme visible au moment ou il se produit.
    let referenced = albums_referenced_by_tracks(backend);
    // `pending_by_type` vient de #1539 (parents avant enfants) : meme requete,
    // arrivee en parallele. On consomme la sienne plutot que d'en garder deux.
    let (_pending_artists, pending_albums, pending_tracks) = pending_by_type(backend);
    let gap = completeness_gap(referenced, combined.albums_synced, pending_albums);
    if gap > 0 {
        warn!(
            albums_referenced_by_tracks = referenced,
            albums_pushed = combined.albums_synced,
            albums_pending = pending_albums,
            albums_unaccounted = gap,
            tracks_pending = pending_tracks,
            "cloud_library_full_sync_incomplete"
        );
    } else {
        debug!(
            albums_referenced_by_tracks = referenced,
            albums_pushed = combined.albums_synced,
            albums_pending = pending_albums,
            "cloud_library_full_sync_complete_check_ok"
        );
    }

    Ok(combined)
}

// ---------------------------------------------------------------------------
// populate_changelog_after_scan — bulk changelog population
// ---------------------------------------------------------------------------

/// After a library scan completes, bulk-insert changelog entries for all
/// tracks, albums, and artists that don't already have a pending entry.
/// This is more efficient than instrumenting every individual insert.
pub fn populate_changelog_after_scan(backend: &Arc<dyn DbBackend>) {
    let result = backend.execute_batch(
        "INSERT INTO sync_changelog (entity_type, entity_id, action) \
         SELECT 'track', id, 'upsert' FROM tracks \
         WHERE id NOT IN (SELECT entity_id FROM sync_changelog WHERE entity_type='track' AND synced=0);\
         INSERT INTO sync_changelog (entity_type, entity_id, action) \
         SELECT 'album', id, 'upsert' FROM albums \
         WHERE id NOT IN (SELECT entity_id FROM sync_changelog WHERE entity_type='album' AND synced=0);\
         INSERT INTO sync_changelog (entity_type, entity_id, action) \
         SELECT 'artist', id, 'upsert' FROM artists \
         WHERE id NOT IN (SELECT entity_id FROM sync_changelog WHERE entity_type='artist' AND synced=0);",
    );
    match result {
        Ok(()) => info!("sync_changelog_populated_after_scan"),
        Err(e) => warn!(error = %e, "sync_changelog_populate_failed"),
    }
}

// ---------------------------------------------------------------------------
// spawn — background sync task
// ---------------------------------------------------------------------------

/// Spawn the periodic cloud library sync task.  Runs every 5 minutes,
/// gated behind Premium tier OR an active circle library share (#5325),
/// plus an SSO access token.
pub fn spawn(backend: Arc<dyn DbBackend>, license: Arc<crate::license::LicenseManager>) {
    let client = match crate::http::client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .user_agent("Tune/2.0 (https://mozaiklabs.fr)")
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, "cloud_library_sync_client_build_failed");
            return;
        }
    };

    tokio::spawn(async move {
        // Wait 2 minutes after startup before the first sync
        tokio::time::sleep(std::time::Duration::from_secs(120)).await;

        loop {
            let premium = license.is_premium().await;
            cycle(&backend, &client, CLOUD_LIBRARY_API, premium).await;

            // Every 5 minutes
            tokio::time::sleep(std::time::Duration::from_secs(300)).await;
        }
    });
}

/// Un passage de la synchronisation périodique. `None` quand rien n'est parti :
/// ni Premium ni partage de cercle (#5325), pas de session SSO, pas de
/// `server_id`, ou rien en attente.
///
/// Sorti de [`spawn`] pour que la décision « qui pousse » s'éprouve sans
/// attendre deux minutes, et contre un faux cloud (`api`).
pub async fn cycle(
    backend: &Arc<dyn DbBackend>,
    client: &reqwest::Client,
    api: &str,
    premium: bool,
) -> Option<Result<SyncReport, String>> {
    let settings = SettingsRepo::with_backend(backend.clone());
    if !synchro_autorisee(premium, &settings) {
        return None;
    }
    // #5358 : un format de synchro enrichi remet la bibliothèque une fois.
    resynchroniser_si_le_format_a_change(backend, &settings, premium);
    let server_id = settings.get("server_id").ok().flatten().unwrap_or_default();
    let token = settings
        .get("mozaik_access_token")
        .ok()
        .flatten()
        .filter(|t| !t.trim().is_empty())?;
    if server_id.is_empty() {
        debug!("cloud_library_sync_skipped_no_server_id");
        return None;
    }
    let pending = pending_count(backend);
    if pending == 0 {
        return None;
    }
    info!(pending, premium, "cloud_library_sync_starting");
    let issue = push_changes_vers(backend, client, api, &server_id, &token).await;
    match &issue {
        Ok(report) => {
            // Store last sync time
            let now = chrono::Utc::now().to_rfc3339();
            settings.set("cloud_library_last_sync", &now).ok();
            info!(
                tracks = report.tracks_synced,
                albums = report.albums_synced,
                artists = report.artists_synced,
                errors = report.errors.len(),
                duration_ms = report.duration_ms,
                "cloud_library_sync_complete"
            );
        }
        Err(e) => {
            warn!(error = %e, "cloud_library_sync_failed");
        }
    }
    Some(issue)
}

/// Controle de complétude de la synchro cloud (#1500).
///
/// Mesure en production le 11/08/2026 : 59 307 albums cites par les pistes,
/// 40 801 recus par le cloud, soit 18 506 jamais envoyes — et 156 296 pistes
/// orphelines cote utilisateur. Aucun signal cote serveur ; l'ecart a ete
/// decouvert trois mois plus tard en construisant autre chose.
#[cfg(test)]
mod completeness_tests {
    use super::completeness_gap;

    #[test]
    fn une_synchro_complete_ne_signale_rien() {
        assert_eq!(completeness_gap(40_801, 40_801, 0), 0);
    }

    #[test]
    fn ce_qui_reste_en_attente_nest_pas_perdu() {
        // Une synchro interrompue est un etat normal : ces albums repartiront
        // au prochain cycle. Les signaler ferait du bruit a chaque coupure,
        // et le bruit finit par ne plus etre lu.
        assert_eq!(completeness_gap(59_307, 40_801, 18_506), 0);
    }

    #[test]
    fn ni_pousse_ni_en_attente_est_perdu() {
        // Le motif reel : les albums ne sont dans AUCUNE file. Rien ne les
        // renverra jamais.
        assert_eq!(completeness_gap(59_307, 40_801, 0), 18_506);
    }

    #[test]
    fn un_ecart_partiel_est_compte_exactement() {
        assert_eq!(completeness_gap(59_307, 40_801, 10_000), 8_506);
    }

    #[test]
    fn jamais_de_negatif() {
        // Un album pousse sans piste qui le cite (album vide, piste supprimee
        // entre-temps) ne doit pas produire un ecart negatif.
        assert_eq!(completeness_gap(10, 12, 3), 0);
        assert_eq!(completeness_gap(0, 0, 0), 0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::migrations;
    use crate::db::sqlite::SqliteDb;

    fn setup() -> Arc<dyn DbBackend> {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();
        Arc::new(db)
    }

    /// Alimente le journal dans l'ordre historique — pistes d'abord — pour
    /// verifier que la LECTURE remet les parents devant, quel que soit l'ordre
    /// d'insertion.
    fn journal_desordonne(backend: &Arc<dyn DbBackend>) {
        backend
            .execute_batch(
                "INSERT INTO sync_changelog (entity_type, entity_id, action) VALUES \
                 ('track', 1, 'upsert'), ('track', 2, 'upsert'), ('track', 3, 'upsert'), \
                 ('album', 10, 'upsert'), ('album', 11, 'upsert'), \
                 ('artist', 20, 'upsert');",
            )
            .unwrap();
    }

    fn types_dans_l_ordre(backend: &Arc<dyn DbBackend>, limite: i64) -> Vec<String> {
        backend
            .query_many(
                "SELECT id, entity_type, entity_id, action FROM sync_changelog \
                 WHERE synced = 0 \
                 ORDER BY CASE entity_type \
                              WHEN 'artist' THEN 0 \
                              WHEN 'album'  THEN 1 \
                              WHEN 'track'  THEN 2 \
                              ELSE 3 END, \
                          changed_at ASC, id ASC \
                 LIMIT ?",
                &[&limite as &dyn ToSqlValue],
            )
            .unwrap()
            .iter()
            .filter_map(|r| r.get(1).and_then(|v| v.as_string()))
            .collect()
    }

    #[test]
    fn les_parents_partent_avant_les_enfants() {
        // Le defaut de juillet 2026 : 584 142 pistes poussees pendant 25 h sans
        // jamais atteindre les albums. Sur une petite bibliotheque la phase
        // « pistes » se vidait avant l'interruption, et personne ne voyait rien.
        let backend = setup();
        journal_desordonne(&backend);

        let ordre = types_dans_l_ordre(&backend, 100);

        assert_eq!(ordre[0], "artist", "l'artiste doit partir en premier");
        assert_eq!(&ordre[1..3], &["album", "album"], "puis les albums");
        assert_eq!(&ordre[3..], &["track", "track", "track"], "puis les pistes");
    }

    #[test]
    fn un_premier_lot_tronque_ne_contient_aucune_piste() {
        // Le cas qui compte vraiment : si la synchronisation s'arrete apres un
        // seul lot, ce lot doit etre fait de parents. C'est ce qui garantit
        // qu'une interruption laisse un etat coherent.
        let backend = setup();
        journal_desordonne(&backend);

        let premier_lot = types_dans_l_ordre(&backend, 3);

        assert!(
            !premier_lot.contains(&"track".to_string()),
            "un lot tronque ne doit pas emporter de pistes avant leurs parents : {premier_lot:?}"
        );
    }

    #[test]
    fn le_compte_par_type_distingue_parents_et_enfants() {
        let backend = setup();
        journal_desordonne(&backend);

        assert_eq!(pending_by_type(&backend), (1, 2, 3));
        assert_eq!(pending_count(&backend), 6);
    }

    #[test]
    fn les_entrees_deja_poussees_sortent_du_compte() {
        let backend = setup();
        journal_desordonne(&backend);
        backend
            .execute_batch("UPDATE sync_changelog SET synced = 1 WHERE entity_type = 'album';")
            .unwrap();

        assert_eq!(pending_by_type(&backend), (1, 0, 3));
    }

    /// #5325 : la forme d'un chemin, sur chaque système, est reconnue ; un
    /// identifiant de service ne l'est pas.
    #[test]
    fn un_chemin_est_reconnu_un_identifiant_ne_l_est_pas() {
        for chemin in [
            "/Users/x/Music/a.flac",
            "/volume1/music/a.flac",
            "  /mnt/nas/x.flac",
            "C:\\Musique\\a.flac",
            "d:/Musique/a.flac",
            "\\\\nas\\music\\a.flac",
            "~/Music/a.flac",
            "file:///home/x/a.flac",
            "SMB://nas/music/a.flac",
            "nfs://nas/export/a.flac",
        ] {
            assert!(ressemble_a_un_chemin(chemin), "{chemin}");
        }
        for identifiant in [
            "123456",
            "qobuz-1",
            "rf-sept-neuf",
            "spotify:track:4uLU6hMCjMI75M1A2tKUQC",
            "https://radio.example/flux.mp3",
            "AC/DC",
            "",
        ] {
            assert!(!ressemble_a_un_chemin(identifiant), "{identifiant}");
        }
    }

    #[test]
    fn la_synchro_tourne_pour_premium_ou_pour_un_partage_de_cercle() {
        let backend = setup();
        let s = SettingsRepo::with_backend(backend);
        assert!(!synchro_autorisee(false, &s), "gratuit sans partage");
        assert!(synchro_autorisee(true, &s), "Premium");
        s.set(CLE_PARTAGE_DE_CERCLE, "true").unwrap();
        assert!(synchro_autorisee(false, &s), "gratuit qui partage");
        s.set(CLE_PARTAGE_DE_CERCLE, "false").unwrap();
        assert!(!synchro_autorisee(false, &s), "partage coupé");
    }
}

/// #5358 : un binaire qui enrichit le format de synchro remet la bibliothèque
/// dans le journal UNE fois, et seulement si la synchro est autorisée.
#[cfg(test)]
mod format_synchro_tests {
    use super::*;
    use crate::db::migrations;
    use crate::db::sqlite::SqliteDb;

    fn ouvrir(chemin: Option<&str>) -> Arc<dyn DbBackend> {
        let db = match chemin {
            Some(c) => SqliteDb::open(c).unwrap(),
            None => SqliteDb::open_in_memory().unwrap(),
        };
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();
        Arc::new(db)
    }

    /// Une bibliothèque déjà poussée par l'ancien binaire : un artiste, un
    /// album, deux pistes, et un journal entièrement marqué `synced = 1`.
    fn bibliotheque_deja_poussee(backend: &Arc<dyn DbBackend>) {
        backend
            .execute_batch(
                "INSERT INTO artists (id, name) VALUES (20, 'Miles Davis');\
                 INSERT INTO albums (id, title, artist_id) VALUES (10, 'Kind of Blue', 20);\
                 INSERT INTO tracks (id, title, album_id, artist_id, file_path, format, source) \
                 VALUES (1, 'So What', 10, 20, '/m/01.flac', 'flac', 'local'), \
                        (2, 'Freddie Freeloader', 10, 20, '/m/02.flac', 'flac', 'local');",
            )
            .unwrap();
        populate_changelog_after_scan(backend);
        backend
            .execute_batch("UPDATE sync_changelog SET synced = 1;")
            .unwrap();
        assert_eq!(pending_count(backend), 0, "prémisse : rien en attente");
    }

    fn format_note(backend: &Arc<dyn DbBackend>) -> Option<String> {
        SettingsRepo::with_backend(backend.clone())
            .get(CLE_FORMAT_SYNCHRO)
            .unwrap()
    }

    #[test]
    fn le_format_courant_vaut_deux() {
        // album_id + MBID (#5325) = 2 ; tout ce qui précède vaut 1.
        assert_eq!(FORMAT_SYNCHRO, 2);
    }

    #[test]
    fn format_absent_toute_la_bibliotheque_repart_une_fois() {
        let backend = ouvrir(None);
        bibliotheque_deja_poussee(&backend);
        let s = SettingsRepo::with_backend(backend.clone());

        assert!(resynchroniser_si_le_format_a_change(&backend, &s, true));
        assert_eq!(pending_by_type(&backend), (1, 1, 2), "tout est remis");
        assert_eq!(format_note(&backend).as_deref(), Some("2"));

        // Un second passage ne remet rien, pas même en double.
        assert!(!resynchroniser_si_le_format_a_change(&backend, &s, true));
        assert_eq!(pending_count(&backend), 4);
    }

    #[test]
    fn format_inferieur_ou_illisible_repart_aussi() {
        for ancien in ["1", "0", "pas-un-nombre"] {
            let backend = ouvrir(None);
            bibliotheque_deja_poussee(&backend);
            let s = SettingsRepo::with_backend(backend.clone());
            s.set(CLE_FORMAT_SYNCHRO, ancien).unwrap();
            assert!(
                resynchroniser_si_le_format_a_change(&backend, &s, true),
                "format {ancien}"
            );
            assert_eq!(pending_count(&backend), 4, "format {ancien}");
        }
    }

    #[test]
    fn format_a_jour_rien_ne_repart() {
        let backend = ouvrir(None);
        bibliotheque_deja_poussee(&backend);
        let s = SettingsRepo::with_backend(backend.clone());
        s.set(CLE_FORMAT_SYNCHRO, &FORMAT_SYNCHRO.to_string())
            .unwrap();

        assert!(!resynchroniser_si_le_format_a_change(&backend, &s, true));
        assert_eq!(pending_count(&backend), 0);
    }

    #[test]
    fn synchro_non_autorisee_rien_et_le_reglage_n_est_pas_ecrit() {
        let backend = ouvrir(None);
        bibliotheque_deja_poussee(&backend);
        let s = SettingsRepo::with_backend(backend.clone());

        assert!(!resynchroniser_si_le_format_a_change(&backend, &s, false));
        assert_eq!(pending_count(&backend), 0);
        assert_eq!(format_note(&backend), None, "réglage non écrit");

        // Le jour où un partage de cercle s'allume, la remise a bien lieu.
        s.set(CLE_PARTAGE_DE_CERCLE, "true").unwrap();
        assert!(resynchroniser_si_le_format_a_change(&backend, &s, false));
        assert_eq!(pending_count(&backend), 4);
    }

    /// Le chemin réel : `cycle`, sur une base de FICHIER rouverte comme au
    /// redémarrage. Sans jeton SSO, `cycle` ne touche pas le réseau.
    #[tokio::test]
    async fn le_cycle_remet_une_fois_et_un_second_demarrage_ne_repousse_plus() {
        let dossier = tempfile::tempdir().unwrap();
        let chemin = dossier.path().join("tune.db");
        let chemin = chemin.to_str().unwrap();
        let client = crate::http::client::shared();
        let api = "http://127.0.0.1:9/api/v1/cloud-library";

        // Premier démarrage du nouveau binaire.
        {
            let backend = ouvrir(Some(chemin));
            bibliotheque_deja_poussee(&backend);

            // Gratuit sans partage : rien, réglage non écrit.
            assert!(cycle(&backend, client, api, false).await.is_none());
            assert_eq!(pending_count(&backend), 0);
            assert_eq!(format_note(&backend), None);

            // Premium : la bibliothèque repart.
            assert!(cycle(&backend, client, api, true).await.is_none());
            assert_eq!(pending_by_type(&backend), (1, 1, 2));
            assert_eq!(format_note(&backend).as_deref(), Some("2"));

            // Le limiteur finit par tout pousser.
            backend
                .execute_batch("UPDATE sync_changelog SET synced = 1;")
                .unwrap();
        }

        // Second démarrage : plus rien ne repart.
        let backend = ouvrir(Some(chemin));
        assert_eq!(format_note(&backend).as_deref(), Some("2"));
        assert!(cycle(&backend, client, api, true).await.is_none());
        assert_eq!(pending_count(&backend), 0, "rien ne repart au redémarrage");
    }
}
