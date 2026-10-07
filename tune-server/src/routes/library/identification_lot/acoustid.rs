//! `POST /library/identify-all?mode=acoustid` — la passe par empreinte
//! (#4805, idée 4 de l'analyse MetaRust).
//!
//! Idée : MetaRust. Le choix d'un enregistrement et le vote d'album viennent
//! de son code, repris dans [`tune_core::metadata::acoustid_picard`] ; ce
//! module n'en est que le pilote, sur le modèle de la passe « labels
//! seulement » du module parent.
//!
//! # Pour qui
//!
//! Les albums **restés introuvables** après la recherche MusicBrainz : la
//! marque `albums.identification_tentee_le` de #5763 (MusicBrainz a répondu,
//! sans pressage), et toujours aucun `musicbrainz_release_id`. Locaux, avec
//! au moins une piste qui a un fichier (une piste CUE n'en a pas : son
//! empreinte serait celle de l'image entière, la même pour toutes).
//!
//! # Ce qu'elle fait, album par album
//!
//! 1. `fpcalc` sur chaque piste (120 premières secondes, comme Picard). Une
//!    empreinte déjà en base (`tracks.acoustid_fingerprint`) n'est pas
//!    recalculée : une reprise ne redécode rien.
//! 2. Une requête AcoustID par piste, au limiteur partagé (3 requêtes/s).
//! 3. [`decider_l_album`] : plancher de score 0,5, durée à ±30 s, marge de
//!    0,05 sur le second, puis la release proposée par au moins la moitié des
//!    pistes. **Sinon, rien n'est écrit.**
//! 4. La release retenue est lue chez MusicBrainz (une requête, au limiteur
//!    partagé) et posée par [`apply_album_identification`] — la même écriture
//!    que le bouton « Ré-identifier » : les clés en remplacement, le
//!    descriptif en comblement seul. Rien n'est écrit dans les fichiers.
//!
//! # En fond, hors lecture, avec reprise
//!
//! * Tâche de fond enregistrée, même pause que l'identification
//!   ([`Tache::Identification`]), relue entre deux albums.
//! * `fpcalc` DÉCODE : comme les autres passes qui décodent, celle-ci cède
//!   **tout** à la lecture. Tant qu'une zone joue, elle attend, entre deux
//!   pistes, et le relevé de priorité la montre ralentie.
//! * Reprise : l'état garde le dernier album terminé ; une passe en pause ou
//!   arrêtée repart après lui. Un album interrompu au milieu est repris en
//!   entier, ses empreintes déjà en base.
//!
//! # Sans `fpcalc` ou sans clé
//!
//! La route refuse en `409` et le DIT (`fpcalc_absent`,
//! `acoustid_cle_absente`) ; l'écran Santé reçoit le même motif dans
//! `GET /system/background-tasks`, bloc `acoustid` ([`disponibilite`]). C'est
//! le cas de Tune OS aujourd'hui : l'image n'embarque pas `fpcalc`.
//!
//! La clé est un réglage SERVEUR : `acoustid_api_key` en base, à défaut
//! `TUNE_ACOUSTID_API_KEY` / `acoustid_api_key` de `tune.toml`. Aucune clé
//! n'est inscrite dans le code.

use std::time::Duration;

use axum::Json;
use axum::http::StatusCode;
use serde_json::{Value, json};
use tracing::{info, warn};

use tune_core::db::backend::ToSqlValue;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::db::track_repo::TrackRepo;
use tune_core::metadata::acoustid_picard::{
    DecisionAlbum, EnregistrementAcoustid, PisteAIdentifier, RefusAcoustid,
    completer_par_l_empreinte, decider_l_album, interroger,
};
use tune_core::metadata::fingerprint;
use tune_core::metadata::musicbrainz_release;
use tune_core::metadata::reidentify::{LocalTrack, apply_album_identification, map_recording_ids};
use tune_core::taches_de_fond::{Tache, est_en_pause, priorite};

use super::{ALBUMS_PAR_ECRITURE, CLE_ETAT, ECHECS_CONSECUTIFS_MAX};
use crate::state::AppState;

/// Le réglage qui porte la clé d'application AcoustID.
pub(crate) const REGLAGE_CLE: &str = "acoustid_api_key";

/// Le mode de la route, tel qu'écrit dans l'état.
const MODE: &str = "acoustid";

/// Un `fpcalc` qui ne rend rien en une minute (partage réseau qui ne répond
/// plus) est abandonné : la piste ne vote pas.
const DELAI_FPCALC: Duration = Duration::from_secs(60);

/// Pendant la lecture, la passe vérifie toutes les cinq secondes si elle peut
/// reprendre.
const ATTENTE_EN_LECTURE: Duration = Duration::from_secs(5);

/// Le coût annoncé d'une piste : `fpcalc` (120 s de décodage) plus une
/// requête à 3/s. **Estimé, non mesuré** — aucune passe réelle n'a tourné.
const SECONDES_PAR_PISTE: f64 = 1.5;

/// La clé AcoustID du serveur : le réglage en base, sinon la configuration
/// (`TUNE_ACOUSTID_API_KEY`, `tune.toml`). Vide compte comme absente.
pub(crate) fn cle_acoustid(state: &AppState) -> Option<String> {
    SettingsRepo::with_backend(state.backend.clone())
        .get(REGLAGE_CLE)
        .ok()
        .flatten()
        .map(|k| k.trim().to_string())
        .filter(|k| !k.is_empty())
        .or_else(|| {
            state
                .config
                .acoustid_api_key
                .as_deref()
                .map(str::trim)
                .filter(|k| !k.is_empty())
                .map(str::to_string)
        })
}

/// Pourquoi la passe ne peut pas tourner, ou `None`.
fn empechement(state: &AppState) -> Option<(&'static str, &'static str)> {
    if !fingerprint::fpcalc_disponible() {
        return Some((
            "fpcalc_absent",
            "Identification par empreinte désactivée : l'outil fpcalc (Chromaprint) \
             n'est pas installé sur ce serveur.",
        ));
    }
    if cle_acoustid(state).is_none() {
        return Some((
            "acoustid_cle_absente",
            "Identification par empreinte désactivée : aucune clé d'application \
             AcoustID n'est configurée (réglage acoustid_api_key, ou \
             TUNE_ACOUSTID_API_KEY).",
        ));
    }
    None
}

/// Le bloc `acoustid` de l'écran Santé (`GET /system/background-tasks`).
pub(crate) fn disponibilite(state: &AppState) -> Value {
    let fpcalc = fingerprint::fpcalc_disponible();
    let cle = cle_acoustid(state).is_some();
    let (raison, message) = match empechement(state) {
        Some((r, m)) => (Some(r), m),
        None => (None, "Identification par empreinte AcoustID disponible."),
    };
    json!({
        "available": raison.is_none(),
        "fpcalc": fpcalc,
        "api_key_configured": cle,
        "reason": raison,
        "message": message,
        "setting_key": REGLAGE_CLE,
    })
}

/// La sélection : albums locaux tentés sans succès par MusicBrainz (#5763),
/// toujours sans MBID, avec au moins une piste à fichier, après le curseur.
/// La troisième colonne compte ces pistes, pour la durée annoncée.
pub(super) fn sql_candidats_acoustid() -> &'static str {
    "SELECT al.id, al.title, \
       (SELECT COUNT(*) FROM tracks t WHERE t.album_id = al.id \
          AND COALESCE(t.source, 'local') = 'local' AND t.file_path IS NOT NULL) \
     FROM albums al \
     WHERE (al.musicbrainz_release_id IS NULL OR TRIM(al.musicbrainz_release_id) = '') \
       AND al.identification_tentee_le IS NOT NULL \
       AND COALESCE(al.source, 'local') = 'local' \
       AND EXISTS ( \
         SELECT 1 FROM tracks t \
         WHERE t.album_id = al.id AND COALESCE(t.source, 'local') = 'local' \
           AND t.file_path IS NOT NULL \
       ) \
       AND al.id > ? \
     ORDER BY al.id"
}

/// Les pistes d'un album soumises à la passe.
fn sql_pistes() -> &'static str {
    "SELECT t.id, t.title, ar.name, t.file_path, t.duration_ms, t.acoustid_fingerprint, \
       t.disc_number, t.track_number \
     FROM tracks t LEFT JOIN artists ar ON ar.id = t.artist_id \
     WHERE t.album_id = ? AND COALESCE(t.source, 'local') = 'local' \
       AND t.file_path IS NOT NULL \
     ORDER BY t.disc_number, t.track_number, t.id"
}

/// Une piste lue en base.
struct PisteEnBase {
    piste: PisteAIdentifier,
    chemin: String,
    empreinte: Option<String>,
    locale: LocalTrack,
}

fn charger_les_pistes(
    backend: &std::sync::Arc<dyn tune_core::db::backend::DbBackend>,
    album_id: i64,
) -> Result<Vec<PisteEnBase>, String> {
    let lignes = backend.query_many(sql_pistes(), &[&album_id as &dyn ToSqlValue])?;
    Ok(lignes
        .iter()
        .filter_map(|r| {
            let id = r.first()?.as_i64()?;
            let titre = r.get(1).and_then(|v| v.as_string()).unwrap_or_default();
            let duree_ms = r.get(4).and_then(|v| v.as_i64()).unwrap_or(0);
            Some(PisteEnBase {
                piste: PisteAIdentifier {
                    track_id: id,
                    titre: titre.clone(),
                    artiste: r.get(2).and_then(|v| v.as_string()),
                    duree_s: (duree_ms.max(0) as f64 / 1000.0).round() as u32,
                },
                chemin: r.get(3)?.as_string()?,
                empreinte: r
                    .get(5)
                    .and_then(|v| v.as_string())
                    .filter(|e| !e.trim().is_empty()),
                locale: LocalTrack {
                    id,
                    disc: r.get(6).and_then(|v| v.as_i64()).unwrap_or(0) as i32,
                    position: r.get(7).and_then(|v| v.as_i64()).unwrap_or(0) as i32,
                    title: titre,
                },
            })
        })
        .collect())
}

/// Les compteurs de la passe.
#[derive(Default)]
struct Compte {
    total: usize,
    pistes_a_traiter: usize,
    traites: usize,
    identifies: usize,
    sans_majorite: usize,
    pistes_identifiees: usize,
    empreintes_calculees: usize,
    empreintes_echouees: usize,
    refus_acoustid: usize,
    pannes_musicbrainz: usize,
    dernier_album_id: i64,
}

fn ecrire_etat(
    backend: &std::sync::Arc<dyn tune_core::db::backend::DbBackend>,
    task_id: &str,
    status: &str,
    c: &Compte,
    raison: Option<&str>,
) {
    SettingsRepo::with_backend(backend.clone())
        .set(
            CLE_ETAT,
            &json!({
                "status": status,
                "mode": MODE,
                "task_id": task_id,
                "total": c.total,
                "pistes_a_traiter": c.pistes_a_traiter,
                "traites": c.traites,
                "identifies": c.identifies,
                "sans_majorite": c.sans_majorite,
                "pistes_identifiees": c.pistes_identifiees,
                "empreintes_calculees": c.empreintes_calculees,
                "empreintes_echouees": c.empreintes_echouees,
                "refus_acoustid": c.refus_acoustid,
                "pannes_musicbrainz": c.pannes_musicbrainz,
                "dernier_album_id": c.dernier_album_id,
                "raison": raison,
            })
            .to_string(),
        )
        .ok();
}

/// Le curseur de reprise : le dernier album terminé d'une passe `acoustid`
/// en pause ou arrêtée ; 0 sinon.
fn curseur_de_reprise(deja: Option<&Value>) -> i64 {
    let Some(e) = deja else { return 0 };
    let reprenable = e["mode"].as_str() == Some(MODE)
        && matches!(e["status"].as_str(), Some("paused") | Some("stopped"));
    if reprenable {
        e["dernier_album_id"].as_i64().unwrap_or(0)
    } else {
        0
    }
}

/// Sélectionne et lance la passe. Droit, pause et passe déjà en cours ont été
/// vérifiés par l'appelant.
pub(super) async fn lancer(state: AppState, deja: Option<Value>) -> (StatusCode, Json<Value>) {
    if let Some((code, message)) = empechement(&state) {
        warn!(code, "identification_acoustid_desactivee");
        return (
            StatusCode::CONFLICT,
            Json(json!({
                "code": code,
                "error": code,
                "message": message,
                "setting_key": REGLAGE_CLE,
            })),
        );
    }
    let Some(cle) = cle_acoustid(&state) else {
        // `empechement` vient de la lire : ne peut pas arriver.
        return (
            StatusCode::CONFLICT,
            Json(json!({"code": "acoustid_cle_absente", "error": "acoustid_cle_absente"})),
        );
    };

    let apres = curseur_de_reprise(deja.as_ref());
    let backend_selection = state.backend.clone();
    let lignes = match tokio::task::spawn_blocking(move || {
        backend_selection.query_many(sql_candidats_acoustid(), &[&apres as &dyn ToSqlValue])
    })
    .await
    {
        Ok(Ok(rows)) => rows,
        erreur => {
            warn!(error = ?erreur, "acoustid_lot_selection_echouee");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({
                    "code": "identification_candidats_indisponibles",
                    "error": "identification_candidats_indisponibles",
                })),
            );
        }
    };
    let albums: Vec<(i64, String)> = lignes
        .iter()
        .filter_map(|r| {
            Some((
                r.first()?.as_i64()?,
                r.get(1).and_then(|v| v.as_string()).unwrap_or_default(),
            ))
        })
        .collect();
    let pistes: i64 = lignes
        .iter()
        .filter_map(|r| r.get(2).and_then(|v| v.as_i64()))
        .sum();
    let task_id = uuid::Uuid::new_v4().to_string();
    let duree_estimee_s = (pistes as f64 * SECONDES_PAR_PISTE).round() as i64;
    info!(task_id = %task_id, candidats = albums.len(), pistes, apres, duree_estimee_s, "acoustid_lot_demarre");

    let total = albums.len();
    let compte = Compte {
        total,
        pistes_a_traiter: pistes.max(0) as usize,
        dernier_album_id: apres,
        ..Default::default()
    };
    ecrire_etat(&state.backend, &task_id, "running", &compte, None);

    let etat_tache = state.clone();
    let task_id_tache = task_id.clone();
    let garde = state.background_tasks.begin(
        "identification_lot",
        "Identification par empreinte AcoustID…",
        "identification",
    );
    tokio::spawn(async move {
        let _garde = garde;
        executer(etat_tache, task_id_tache, cle, albums, compte).await;
    });

    (
        StatusCode::ACCEPTED,
        Json(json!({
            "status": "started",
            "mode": MODE,
            "task_id": task_id,
            "total": total,
            "pistes": pistes,
            "reprise_apres_album_id": apres,
            "duree_estimee_s": duree_estimee_s,
            "statut": "GET /library/identify-all/status",
            "arreter": "POST /system/taches-de-fond/identification/pause",
        })),
    )
}

/// Attend qu'aucune zone ne joue. `false` si une pause est demandée entre
/// temps : l'appelant sort.
async fn attendre_hors_lecture() -> bool {
    while priorite::lecture_en_cours() {
        if est_en_pause(Tache::Identification) {
            return false;
        }
        priorite::noter_cedee(Tache::Identification.id());
        tokio::time::sleep(ATTENTE_EN_LECTURE).await;
    }
    !est_en_pause(Tache::Identification)
}

/// Comment s'est terminée la boucle d'un album.
enum FinAlbum {
    Termine,
    Pause,
    Arret(&'static str),
}

async fn executer(
    state: AppState,
    task_id: String,
    cle: String,
    albums: Vec<(i64, String)>,
    mut c: Compte,
) {
    let mut refus_consecutifs = 0u32;
    for (album_id, titre) in albums {
        let fin = traiter_un_album(
            &state,
            &cle,
            album_id,
            &titre,
            &mut c,
            &mut refus_consecutifs,
        )
        .await;
        match fin {
            FinAlbum::Termine => {
                c.traites += 1;
                c.dernier_album_id = album_id;
                if c.traites.is_multiple_of(ALBUMS_PAR_ECRITURE) {
                    ecrire_etat(&state.backend, &task_id, "running", &c, None);
                }
            }
            FinAlbum::Pause => {
                info!(task_id = %task_id, traites = c.traites, "acoustid_lot_en_pause");
                ecrire_etat(
                    &state.backend,
                    &task_id,
                    "paused",
                    &c,
                    Some("pause_utilisateur"),
                );
                return;
            }
            FinAlbum::Arret(raison) => {
                warn!(task_id = %task_id, traites = c.traites, raison, "acoustid_lot_arrete");
                ecrire_etat(&state.backend, &task_id, "stopped", &c, Some(raison));
                return;
            }
        }
    }
    info!(
        task_id = %task_id,
        total = c.total,
        identifies = c.identifies,
        sans_majorite = c.sans_majorite,
        "acoustid_lot_termine"
    );
    ecrire_etat(&state.backend, &task_id, "done", &c, None);
}

/// L'empreinte d'une piste : celle déjà en base, sinon `fpcalc`. `None` si
/// `fpcalc` échoue ou dépasse [`DELAI_FPCALC`].
async fn empreinte_de(p: &PisteEnBase, c: &mut Compte) -> Option<(String, u32)> {
    if let Some(e) = &p.empreinte
        && p.piste.duree_s > 0
    {
        return Some((e.clone(), p.piste.duree_s));
    }
    match tokio::time::timeout(DELAI_FPCALC, fingerprint::generate_fingerprint(&p.chemin)).await {
        Ok(Ok(fp)) => {
            c.empreintes_calculees += 1;
            Some((fp.fingerprint, fp.duration.round().max(0.0) as u32))
        }
        Ok(Err(e)) => {
            c.empreintes_echouees += 1;
            warn!(track_id = p.piste.track_id, error = %e, "acoustid_empreinte_echouee");
            None
        }
        Err(_) => {
            c.empreintes_echouees += 1;
            warn!(
                track_id = p.piste.track_id,
                "acoustid_empreinte_trop_longue"
            );
            None
        }
    }
}

async fn traiter_un_album(
    state: &AppState,
    cle: &str,
    album_id: i64,
    titre: &str,
    c: &mut Compte,
    refus_consecutifs: &mut u32,
) -> FinAlbum {
    // Frontière de pause, puis la lecture d'abord.
    if !attendre_hors_lecture().await {
        return FinAlbum::Pause;
    }
    let backend = state.backend.clone();
    let pistes =
        match tokio::task::spawn_blocking(move || charger_les_pistes(&backend, album_id)).await {
            Ok(Ok(p)) => p,
            Ok(Err(e)) => {
                warn!(album_id, error = %e, "acoustid_pistes_illisibles");
                return FinAlbum::Termine;
            }
            Err(e) => {
                warn!(album_id, error = %e, "acoustid_pistes_illisibles");
                return FinAlbum::Termine;
            }
        };
    if pistes.is_empty() {
        return FinAlbum::Termine;
    }

    let repo = TrackRepo::with_backend(state.backend.clone());
    let mut lues: Vec<(PisteAIdentifier, Vec<EnregistrementAcoustid>)> =
        Vec::with_capacity(pistes.len());
    for p in &pistes {
        // `fpcalc` décode : rien pendant la lecture, même au milieu d'un album.
        if !attendre_hors_lecture().await {
            return FinAlbum::Pause;
        }
        let Some((empreinte, duree_s)) = empreinte_de(p, c).await else {
            lues.push((p.piste.clone(), Vec::new()));
            continue;
        };
        let resultats = match interroger(cle, &empreinte, duree_s).await {
            Ok(r) => {
                *refus_consecutifs = 0;
                r
            }
            Err(RefusAcoustid::CleRefusee) => return FinAlbum::Arret("acoustid_cle_refusee"),
            Err(refus) => {
                warn!(album_id, track_id = p.piste.track_id, refus = %refus, "acoustid_refus");
                c.refus_acoustid += 1;
                *refus_consecutifs += 1;
                if *refus_consecutifs >= ECHECS_CONSECUTIFS_MAX {
                    return FinAlbum::Arret("acoustid_injoignable");
                }
                Vec::new()
            }
        };
        let meilleur = resultats.iter().map(|r| r.score).fold(0.0_f64, f64::max);
        if let Err(e) = repo.set_acoustid(p.piste.track_id, &empreinte, meilleur) {
            warn!(track_id = p.piste.track_id, error = %e, "acoustid_empreinte_non_ecrite");
        }
        let mut piste = p.piste.clone();
        piste.duree_s = duree_s;
        lues.push((piste, resultats));
    }

    let DecisionAlbum::Retenue {
        release,
        enregistrements,
        votes,
        pistes: nb,
    } = decider_l_album(Some(titre), &lues)
    else {
        c.sans_majorite += 1;
        info!(album_id, "acoustid_sans_majorite");
        return FinAlbum::Termine;
    };

    // #4805 — lu et GARDÉ en base (créneau MusicBrainz compris) : la lecture
    // des crédits qui suit la pose le relit sans requête.
    let Some(detail) =
        musicbrainz_release::lookup_release_detail_gardee(&state.backend, &release.id).await
    else {
        // Rien n'est écrit : la release n'a pas pu être lue. Une passe neuve
        // la retentera.
        c.pannes_musicbrainz += 1;
        warn!(album_id, release_id = %release.id, "acoustid_release_illisible");
        return FinAlbum::Termine;
    };
    let locales: Vec<LocalTrack> = pistes.iter().map(|p| p.locale.clone()).collect();
    let recordings = completer_par_l_empreinte(
        map_recording_ids(&locales, &detail.tracks),
        &enregistrements,
        &detail.tracks,
    );

    let backend = state.backend.clone();
    let release_id = release.id.clone();
    let groupe = release.release_group_id.clone();
    let nb_locales = locales.len();
    let ecrit = tokio::task::spawn_blocking(move || {
        // L'album a pu être identifié entre la sélection et son tour (à la
        // main, par la passe texte) : on ne l'écrase pas.
        let deja = backend
            .query_one(
                "SELECT musicbrainz_release_id FROM albums WHERE id = ?",
                &[&album_id as &dyn ToSqlValue],
            )?
            .and_then(|r| r.first().and_then(|v| v.as_string()))
            .filter(|s| !s.trim().is_empty());
        if deja.is_some() {
            return Ok(None);
        }
        apply_album_identification(
            &backend,
            album_id,
            &release_id,
            groupe.as_deref(),
            &recordings,
            nb_locales,
            Some(&detail),
        )
        .map(Some)
    })
    .await;
    match ecrit {
        Ok(Ok(Some(applied))) => {
            c.identifies += 1;
            // #4805 — l'album n'avait pas de pressage : il vient d'en changer.
            super::super::credits_apres_identification::apres_identification(
                state,
                album_id,
                "reidentified",
            );
            c.pistes_identifiees += applied.tracks_matched;
            info!(
                album_id,
                release_id = %release.id,
                votes,
                pistes = nb,
                matched = applied.tracks_matched,
                "acoustid_album_identifie"
            );
        }
        Ok(Ok(None)) => info!(album_id, "acoustid_album_deja_identifie"),
        Ok(Err(e)) => warn!(album_id, error = %e, "acoustid_ecriture_echouee"),
        Err(e) => warn!(album_id, error = %e, "acoustid_ecriture_echouee"),
    }
    FinAlbum::Termine
}
