//! Modifier les métadonnées de tout le résultat filtré d'Oxygen en un geste
//! (#7539, GO de Bertrand du 30/10/2026).
//!
//! | Route | Effet |
//! |---|---|
//! | `GET  /library/edit-by-filter/preview?<filtres>&item_type=&field=&value=` | l'aperçu, rien n'est écrit |
//! | `POST /library/edit-by-filter?<filtres>` `{item_type, field, value, expected_tracks, expected_albums}` | la base, en UNE transaction |
//! | `GET  /library/edit-by-filter/files/status` | l'avancement de l'écriture des fichiers |
//! | `POST /library/edit-by-filter/files/cancel` | l'arrêter |
//!
//! # Pourquoi une route neuve
//!
//! `POST /library/albums/batch-update` prend une liste d'identifiants
//! d'ALBUMS, écrit album par album sans transaction ni plafond, ne connaît ni
//! le compositeur ni les pistes, et n'écrit aucun fichier. La sélection
//! d'Oxygen est désignée par les FILTRES (#7128) : on reprend cette
//! désignation, et le plan de [`TrackRepo::planifier_edition_par_filtre`].
//!
//! # Les fichiers : une tâche de fond, bornée et annulable
//!
//! La base change d'abord, en une transaction, et la réponse revient tout de
//! suite. Réglage « Écrire les modifications dans les fichiers audio » coché
//! (désactivé par défaut), une passe de fond réécrit ensuite le SEUL champ
//! modifié dans les fichiers des pistes réellement changées :
//!
//! - bornée : au plus [`PLAFOND_EDITION_PAR_FILTRE`] fichiers, le plafond des
//!   pistes ; une seule passe à la fois (`409 files_pass_running` sinon,
//!   avant toute écriture en base) ;
//! - annulable : `POST …/files/cancel`, lu avant CHAQUE fichier — de même que
//!   le réglage, relu avant chaque fichier ;
//! - sûre : l'écrivain atomique de « Graver tags » (`write_tags_atomique`,
//!   #6635 : copie cachée, contrôle de l'audio, `rename`), jamais le fichier
//!   d'une piste qu'une zone tient, jamais une image CUE ;
//! - visible : inscrite au registre des tâches de fond (bandeau global), avec
//!   son avancement, et son bilan dans `…/files/status`.
//!
//! Pourquoi pas DANS la requête : 5 000 réécritures atomiques (copie complète
//! de chaque fichier) dépassent de loin le délai d'une requête HTTP, et un
//! navigateur qui coupe n'arrêterait rien. Pourquoi pas DANS la transaction :
//! un fichier réécrit ne se « rembobine » pas — la base est la vérité, le
//! fichier suit, et chaque refus est nommé.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use axum::Json;
use axum::extract::{RawQuery, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{Value, json};
use tune_core::db::artist_repo::ArtistRepo;
use tune_core::db::facet_filter::TrackFilter;
use tune_core::db::track_repo::{
    ChampDeMasse, CibleDuFiltre, FichierAEcrire, PLAFOND_EDITION_PAR_FILTRE, PlanEdition,
    RefusEdition, TrackRepo, ValeurDeMasse, valider_valeur,
};
use tune_core::metadata::tag_writer::{
    MOTIF_FORMAT_SANS_EMPREINTE, TagUpdate, write_tags_atomique,
};

use super::facets::hors_executeur;
use super::query_multi::{track_filter_from_raw, values_of};
use crate::error::AppError;
use crate::state::AppState;

/// Identifiant de la passe au registre des tâches de fond.
pub(crate) const TACHE_FICHIERS: &str = "edit_by_filter_files";
/// Clé `settings` du bilan de la dernière passe.
pub(crate) const CLE_STATUT: &str = "edit_by_filter_files_status";
const REFUS_DETAILLES_MAX: usize = 50;

/// La passe en cours : son identifiant et son drapeau d'arrêt.
static PASSE: Mutex<Option<(String, Arc<AtomicBool>)>> = Mutex::new(None);

fn passe_en_cours() -> bool {
    PASSE.lock().map(|p| p.is_some()).unwrap_or(true)
}

#[derive(Deserialize)]
pub(super) struct CorpsEdition {
    #[serde(default = "piste")]
    item_type: String,
    field: String,
    value: String,
    expected_tracks: Option<i64>,
    expected_albums: Option<i64>,
}

fn piste() -> String {
    "track".into()
}

fn filtre_de_la_vue(state: &AppState, raw: Option<&str>) -> Result<TrackFilter, AppError> {
    let mut filtre = track_filter_from_raw(raw)?;
    if let Some(nom) = values_of(raw, "collection")
        .into_iter()
        .map(|s| s.trim().to_string())
        .find(|s| !s.is_empty())
    {
        let scope = super::facets::resolve_collection(state, &nom);
        filtre.collection_ids = scope.albums;
        filtre.collection_track_ids = scope.tracks;
    }
    Ok(filtre)
}

fn mauvaise_demande(code: &str, message: String) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({ "error": message, "code": code })),
    )
        .into_response()
}

/// La demande lue et validée : cible, champ, valeur.
fn demande(
    item_type: &str,
    field: &str,
    value: &str,
) -> Result<(CibleDuFiltre, ChampDeMasse, ValeurDeMasse), Response> {
    let cible = CibleDuFiltre::depuis_item_type(item_type).ok_or_else(|| {
        mauvaise_demande(
            "bad_item_type",
            format!("item_type must be `track` or `album`, got `{item_type}`"),
        )
    })?;
    let champ = ChampDeMasse::depuis_nom(field).ok_or_else(|| {
        mauvaise_demande(
            "bad_field",
            format!("field must be genre, year, label, album_artist or composer, got `{field}`"),
        )
    })?;
    let valeur = valider_valeur(champ, value).map_err(|r| refus(r, cible))?;
    Ok((cible, champ, valeur))
}

fn refus(r: RefusEdition, cible: CibleDuFiltre) -> Response {
    match r {
        RefusEdition::FiltreVide => mauvaise_demande(
            "empty_filter",
            "no active filter: refusing to edit the whole library".into(),
        ),
        RefusEdition::ValeurVide => mauvaise_demande("empty_value", "the value is empty".into()),
        RefusEdition::AnneeInvalide => {
            mauvaise_demande("invalid_year", "the year must be a 4-digit number".into())
        }
        RefusEdition::ValeurTropLongue { max } => mauvaise_demande(
            "value_too_long",
            format!("the value is longer than {max} characters"),
        ),
        RefusEdition::TropDePistes { nombre, plafond } => (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({
                "error": format!("{nombre} tracks match, the limit is {plafond}"),
                "code": "too_many_items",
                "item_type": cible.item_type(),
                "count": nombre,
                "limit": plafond,
            })),
        )
            .into_response(),
        RefusEdition::ApercuPerime { pistes, albums } => (
            StatusCode::CONFLICT,
            Json(json!({
                "error": "the selection changed since the preview: nothing was written",
                "code": "preview_changed",
                "tracks": pistes,
                "albums": albums,
            })),
        )
            .into_response(),
    }
}

fn erreur_interne(e: impl std::fmt::Display) -> Response {
    tracing::error!(error = %e, "edition_par_filtre_echec");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({ "error": e.to_string() })),
    )
        .into_response()
}

fn corps_du_plan(
    plan: &PlanEdition,
    cible: CibleDuFiltre,
    champ: ChampDeMasse,
    valeur: &ValeurDeMasse,
    ecriture: bool,
) -> Value {
    json!({
        "item_type": champ.cible_effective(cible).item_type(),
        "field": champ.nom(),
        "value": valeur.affichage(),
        "tracks": plan.pistes_changees.len(),
        "albums": plan.albums_changes.len(),
        "unchanged": plan.pistes_deja,
        "remote_skipped": plan.pistes_distantes,
        "remote_albums_skipped": plan.albums_distants,
        "limit": PLAFOND_EDITION_PAR_FILTRE,
        "over_limit": false,
        crate::routes::ecriture_fichiers::CHAMP_REPONSE: ecriture,
        "files": if ecriture { plan.fichiers.len() } else { 0 },
        "files_cue_skipped": if ecriture { plan.fichiers_cue } else { 0 },
        "track_examples": plan.exemples_pistes,
        "album_examples": plan.exemples_albums,
    })
}

/// `GET /library/edit-by-filter/preview` — ce que le geste ferait.
pub(super) async fn apercu(
    State(state): State<AppState>,
    RawQuery(raw): RawQuery,
) -> Result<Response, AppError> {
    let un = |cle: &str| values_of(raw.as_deref(), cle).into_iter().next();
    let item_type = un("item_type").unwrap_or_else(piste);
    let (cible, champ, valeur) = match demande(
        &item_type,
        &un("field").unwrap_or_default(),
        &un("value").unwrap_or_default(),
    ) {
        Ok(d) => d,
        Err(r) => return Ok(r),
    };
    let filtre = filtre_de_la_vue(&state, raw.as_deref())?;
    hors_executeur("edit_by_filter_preview", move || {
        let ecriture = crate::routes::ecriture_fichiers::autorisee(&state);
        let repo = TrackRepo::with_backend(state.backend.clone());
        match repo.planifier_edition_par_filtre(&filtre, cible, champ, valeur.clone()) {
            Ok(Ok(plan)) => {
                Json(corps_du_plan(&plan, cible, champ, &valeur, ecriture)).into_response()
            }
            // L'aperçu d'un résultat trop grand se lit, il ne se refuse pas :
            // l'écran dit « trop d'éléments », comme l'étiquetage.
            Ok(Err(RefusEdition::TropDePistes { nombre, plafond })) => Json(json!({
                "item_type": champ.cible_effective(cible).item_type(),
                "field": champ.nom(),
                "value": valeur.affichage(),
                "count": nombre,
                "limit": plafond,
                "over_limit": true,
                crate::routes::ecriture_fichiers::CHAMP_REPONSE: ecriture,
            }))
            .into_response(),
            Ok(Err(r)) => refus(r, cible),
            Err(e) => erreur_interne(e),
        }
    })
    .await
}

/// `POST /library/edit-by-filter` — la base, en une transaction ; puis, si le
/// réglage le permet, la passe de fond des fichiers.
pub(super) async fn appliquer(
    State(state): State<AppState>,
    RawQuery(raw): RawQuery,
    Json(corps): Json<CorpsEdition>,
) -> Result<Response, AppError> {
    let (cible, champ, valeur) = match demande(&corps.item_type, &corps.field, &corps.value) {
        Ok(d) => d,
        Err(r) => return Ok(r),
    };
    let filtre = filtre_de_la_vue(&state, raw.as_deref())?;
    let attendu = match (corps.expected_tracks, corps.expected_albums) {
        (Some(p), Some(a)) => Some((p, a)),
        _ => None,
    };
    let ecriture = crate::routes::ecriture_fichiers::autorisee(&state);
    if ecriture && passe_en_cours() {
        return Ok((
            StatusCode::CONFLICT,
            Json(json!({
                "error": "a file-writing pass is already running: nothing was written",
                "code": "files_pass_running",
            })),
        )
            .into_response());
    }
    let state2 = state.clone();
    let valeur2 = valeur.clone();
    let resultat = hors_executeur("edit_by_filter", move || {
        let repo = TrackRepo::with_backend(state2.backend.clone());
        // L'artiste d'album se résout AVANT la transaction (son dépôt a sa
        // propre connexion) — et seulement si le plan change quelque chose,
        // pour ne pas créer un artiste orphelin sur un refus.
        let artiste_id = if champ == ChampDeMasse::ArtisteAlbum {
            match repo.planifier_edition_par_filtre(&filtre, cible, champ, valeur2.clone()) {
                Ok(Ok(p)) if p.albums_changes.is_empty() && p.pistes_changees.is_empty() => None,
                Ok(Ok(_)) => {
                    let nom = valeur2.affichage();
                    let artistes = ArtistRepo::with_backend(state2.backend.clone());
                    let trouve = artistes.get_by_name(&nom).ok().flatten().and_then(|a| a.id);
                    match trouve {
                        Some(id) => Some(id),
                        None => match artistes.get_or_create(&nom, None, None) {
                            Ok(a) => a.id,
                            Err(e) => return Err(erreur_interne(e)),
                        },
                    }
                }
                Ok(Err(r)) => return Err(refus(r, cible)),
                Err(e) => return Err(erreur_interne(e)),
            }
        } else {
            None
        };
        if champ == ChampDeMasse::ArtisteAlbum && artiste_id.is_none() {
            // Rien à changer : le plan vide, sans transaction.
            return match repo.planifier_edition_par_filtre(&filtre, cible, champ, valeur2.clone()) {
                Ok(Ok(p)) => Ok(p),
                Ok(Err(r)) => Err(refus(r, cible)),
                Err(e) => Err(erreur_interne(e)),
            };
        }
        match repo.appliquer_edition_par_filtre(&filtre, cible, champ, valeur2, artiste_id, attendu)
        {
            Ok(Ok(p)) => Ok(p),
            Ok(Err(r)) => Err(refus(r, cible)),
            Err(e) => Err(erreur_interne(e)),
        }
    })
    .await?;
    let plan = match resultat {
        Ok(p) => p,
        Err(r) => return Ok(r),
    };
    let mut corps_reponse = corps_du_plan(&plan, cible, champ, &valeur, ecriture);
    corps_reponse["status"] = json!("ok");
    corps_reponse["files_task_id"] = Value::Null;
    if ecriture && !plan.fichiers.is_empty() {
        let fichiers: Vec<FichierAEcrire> = plan
            .fichiers
            .iter()
            .take(PLAFOND_EDITION_PAR_FILTRE as usize)
            .cloned()
            .collect();
        match lancer_la_passe(&state, champ, &valeur, fichiers) {
            Some(id) => corps_reponse["files_task_id"] = json!(id),
            None => corps_reponse["files_pass_refused"] = json!(true),
        }
    }
    Ok(Json(corps_reponse).into_response())
}

/// Le `TagUpdate` du seul champ modifié.
fn maj_du_champ(champ: ChampDeMasse, valeur: &ValeurDeMasse) -> TagUpdate {
    let mut maj = TagUpdate::default();
    match (champ, valeur) {
        (ChampDeMasse::Genre, ValeurDeMasse::Texte(g)) => {
            let liste = tune_core::db::genre_album_pistes::colonnes_de_piste(g)
                .and_then(|(_, json)| serde_json::from_str::<Vec<String>>(&json).ok())
                .unwrap_or_default();
            maj.genre = liste.first().cloned();
            maj.genres = liste;
        }
        (ChampDeMasse::Annee, ValeurDeMasse::Annee(a)) => maj.year = Some(*a),
        (ChampDeMasse::Label, ValeurDeMasse::Texte(v)) => maj.label = Some(v.clone()),
        (ChampDeMasse::Compositeur, ValeurDeMasse::Texte(v)) => maj.composer = Some(v.clone()),
        (ChampDeMasse::ArtisteAlbum, ValeurDeMasse::Texte(v)) => maj.album_artist = Some(v.clone()),
        _ => {}
    }
    maj
}

fn publier_statut(state: &AppState, statut: &Value) {
    tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone())
        .set(CLE_STATUT, &statut.to_string())
        .ok();
}

fn noter(refus: &mut Vec<Value>, f: &FichierAEcrire, motif: &str) {
    if refus.len() < REFUS_DETAILLES_MAX {
        refus.push(json!({"track_id": f.track_id, "file": f.file_path, "reason": motif}));
    }
}

/// Lance la passe de fond. `None` : une autre passe tient déjà la place.
fn lancer_la_passe(
    state: &AppState,
    champ: ChampDeMasse,
    valeur: &ValeurDeMasse,
    fichiers: Vec<FichierAEcrire>,
) -> Option<String> {
    let id = uuid::Uuid::new_v4().to_string();
    let arret = Arc::new(AtomicBool::new(false));
    {
        let mut p = PASSE.lock().ok()?;
        if p.is_some() {
            return None;
        }
        *p = Some((id.clone(), arret.clone()));
    }
    let total = fichiers.len();
    publier_statut(
        state,
        &json!({"status": "running", "task_id": id, "field": champ.nom(), "written": 0, "total": total}),
    );
    let garde = state.background_tasks.begin(
        TACHE_FICHIERS,
        "Écriture des métadonnées modifiées dans les fichiers…",
        "maintenance",
    );
    let taches = state.background_tasks.clone();
    let state2 = state.clone();
    let maj = maj_du_champ(champ, valeur);
    let id2 = id.clone();
    tokio::spawn(async move {
        let _garde = garde;
        let pistes = TrackRepo::with_backend(state2.backend.clone());
        let (mut ecrits, mut sautes, mut erreurs) = (0usize, 0usize, 0usize);
        let mut refus: Vec<Value> = Vec::new();
        let mut fin = "done";
        taches.update_progress(TACHE_FICHIERS, 0, total as u64, "Fichiers");
        for (i, f) in fichiers.iter().enumerate() {
            if i % 25 == 0 {
                taches.update_progress(TACHE_FICHIERS, i as u64, total as u64, "Fichiers");
                publier_statut(
                    &state2,
                    &json!({"status": "running", "task_id": id2, "field": champ.nom(),
                            "written": ecrits, "skipped": sautes, "errors": erreurs,
                            "processed": i, "total": total}),
                );
            }
            if arret.load(Ordering::SeqCst) {
                fin = "cancelled";
                break;
            }
            // Le réglage relu avant CHAQUE fichier : coupé en cours de passe,
            // plus rien n'est écrit.
            if !crate::routes::ecriture_fichiers::autorisee(&state2) {
                tracing::warn!(
                    track_id = f.track_id,
                    "edition_par_filtre_reglage_coupe_arret"
                );
                noter(&mut refus, f, super::write_tags::MOTIF_REGLAGE_COUPE);
                fin = "stopped";
                break;
            }
            if tune_core::library::local_path::resolve_local_path(&f.file_path).is_missing() {
                sautes += 1;
                noter(&mut refus, f, "fichier introuvable");
                continue;
            }
            if super::graver_dr::fichier_en_lecture(&state2.playback, &pistes, &f.file_path).await {
                sautes += 1;
                noter(&mut refus, f, super::write_tags::MOTIF_FICHIER_EN_LECTURE);
                continue;
            }
            match write_tags_atomique(&f.file_path, &maj).await {
                Ok(r) => {
                    ecrits += 1;
                    if r.fields_written > 0 {
                        super::write_tags::remettre_la_ligne_en_phase(
                            &state2.backend,
                            &f.file_path,
                        );
                    }
                }
                Err(e) if e == MOTIF_FORMAT_SANS_EMPREINTE => {
                    sautes += 1;
                    noter(&mut refus, f, &e);
                }
                Err(e) => {
                    tracing::warn!(track_id = f.track_id, file = %f.file_path, error = %e, "edition_par_filtre_fichier_en_echec");
                    erreurs += 1;
                    noter(&mut refus, f, &e);
                }
            }
        }
        publier_statut(
            &state2,
            &json!({"status": fin, "task_id": id2, "field": champ.nom(),
                    "written": ecrits, "skipped": sautes, "errors": erreurs,
                    "processed": ecrits + sautes + erreurs, "total": total, "refus": refus}),
        );
        tracing::info!(task_id = %id2, statut = fin, ecrits, sautes, erreurs, total, "edition_par_filtre_fichiers_fin");
        if let Ok(mut p) = PASSE.lock() {
            *p = None;
        }
    });
    Some(id)
}

/// `GET /library/edit-by-filter/files/status`
pub(super) async fn statut_fichiers(State(state): State<AppState>) -> Json<Value> {
    let v = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone())
        .get(CLE_STATUT)
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .unwrap_or(json!({"status": "idle"}));
    Json(v)
}

/// `POST /library/edit-by-filter/files/cancel` — l'arrêt est lu avant le
/// fichier suivant ; celui en cours d'écriture finit (ou abandonne) seul,
/// l'écrivain atomique ne laisse jamais un fichier à moitié écrit.
pub(super) async fn annuler_fichiers() -> Json<Value> {
    let en_cours = PASSE.lock().ok().and_then(|p| {
        p.as_ref().map(|(id, a)| {
            a.store(true, Ordering::SeqCst);
            id.clone()
        })
    });
    Json(json!({"cancelled": en_cours.is_some(), "task_id": en_cours}))
}

#[cfg(test)]
#[path = "edition_par_filtre_tests.rs"]
mod tests;
