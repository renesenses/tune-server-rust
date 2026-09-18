//! Le drapeau « compilation », posé À LA MAIN — et qui tient (#4427).
//!
//! **Le défaut.** Bertrand, 18/09/2026 : la compilation *Coco María Presents*
//! apparaît **douze fois** dans la bibliothèque, une vignette par artiste de
//! piste, même titre, même pochette. `albums.is_compilation` existe, mais il
//! n'est écrit **que par le scan** (`library/ingest.rs` : « pas d'artiste
//! d'album explicite ET plusieurs artistes distincts »), et
//! `AlbumRepo::mark_compilation` ne sait que le **lever**. Un utilisateur ne
//! pouvait donc ni le poser, ni le retirer.
//!
//! **Ce qui est tranché** (Bertrand, 18/09) :
//!
//! 1. la décision manuelle **prime** ; en son absence, le scan décide comme
//!    avant ;
//! 2. elle **survit au re-scan** — sans quoi l'écran ment : le prochain passage
//!    du scanner défait le geste ;
//! 3. la gravure dans les fichiers est **proposée, jamais imposée** : la base
//!    retient le choix tout de suite, graver est une seconde action explicite.
//!
//! D'où deux colonnes et non une : `is_compilation` porte ce que le scan a
//! déduit, `compilation_manuelle` ce que l'utilisateur a voulu. Les écraser
//! l'une par l'autre rendrait le retour en arrière impossible.
use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::{info, warn};

use crate::state::AppState;
use std::collections::HashMap;
use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::track_repo::TrackRepo;

#[derive(Deserialize)]
pub(super) struct DemandeCompilation {
    album_ids: Vec<i64>,
    valeur: bool,
    /// Réunir les albums visés en un seul disque. N'a de sens qu'en posant le
    /// drapeau sur PLUSIEURS albums — c'est le cas de Coco María.
    #[serde(default)]
    fusionner: bool,
}

/// `POST /library/albums/compilation`
///
/// Pose (ou retire) la décision manuelle, et aligne le drapeau déduit sur elle.
///
/// ⚠️ `valeur = false` **ne défait pas** une fusion déjà faite. Les deux gestes
/// sont distincts, et confondre un simple décochage avec une commande
/// destructrice serait le pire des malentendus sur cet écran.
pub(super) async fn poser_compilation(
    State(state): State<AppState>,
    Json(demande): Json<DemandeCompilation>,
) -> impl IntoResponse {
    if demande.album_ids.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "aucun album", "motif": "album_ids_vide"})),
        )
            .into_response();
    }
    let repo = AlbumRepo::with_backend(state.backend.clone());
    let mut poses = 0usize;
    let mut echecs: Vec<Value> = Vec::new();
    for id in &demande.album_ids {
        match repo.poser_compilation_manuelle(*id, demande.valeur) {
            Ok(()) => poses += 1,
            Err(e) => {
                warn!(album_id = *id, error = %e, "compilation_manuelle_echec");
                echecs.push(json!({ "album_id": id, "erreur": e.to_string() }));
            }
        }
    }

    // La fusion n'est tentée QUE si on pose le drapeau sur plusieurs albums :
    // réunir en retirant n'aurait aucun sens, et réunir un album seul non plus.
    let mut fusionnes = 0usize;
    let mut cible_fusion: Option<i64> = None;
    if demande.fusionner && demande.valeur && demande.album_ids.len() > 1 {
        let cible = demande.album_ids[0];
        cible_fusion = Some(cible);
        for doublon in &demande.album_ids[1..] {
            match repo.absorber(cible, *doublon) {
                Ok(_) => fusionnes += 1,
                Err(e) => {
                    warn!(cible, doublon = *doublon, error = %e, "compilation_fusion_echec");
                    echecs.push(json!({ "album_id": doublon, "erreur": e.to_string() }));
                }
            }
        }
    }

    info!(
        albums = demande.album_ids.len(),
        valeur = demande.valeur,
        poses,
        fusionnes,
        "compilation_manuelle_posee"
    );
    state.event_bus.emit(
        tune_core::event_types::EventType::LibraryUpdated.as_str(),
        json!({ "source": "compilation_manuelle", "albums": demande.album_ids.len() }),
    );

    (
        StatusCode::OK,
        Json(json!({
            "poses": poses,
            "valeur": demande.valeur,
            "fusionnes": fusionnes,
            "album_cible": cible_fusion,
            "echecs": echecs,
        })),
    )
        .into_response()
}

#[derive(Deserialize)]
pub(super) struct DemandeGravure {
    album_ids: Vec<i64>,
}

/// `POST /library/albums/compilation/graver`
///
/// Écrit `COMPILATION` / `TCMP` dans les fichiers des pistes de ces albums.
///
/// **Seconde action, explicite** (arbitrage du 18/09) : la base a déjà retenu
/// le choix, graver est un geste à part — parce qu'il touche les fichiers de
/// l'utilisateur, et que rien n'oblige à les modifier pour que Tune se
/// souvienne.
///
/// La valeur gravée est la DÉCISION MANUELLE de l'album, pas un paramètre de la
/// requête : graver l'inverse de ce que l'écran montre serait le plus sûr moyen
/// de désaligner les fichiers et la base.
///
/// ⚠️ Régression à ne pas rejouer — #4238 : le graveur effaçait les champs
/// Vorbis qu'il ne connaissait pas. On passe par `write_metadata_to_file`, qui
/// n'écrit QUE les clés données et laisse le reste intact.
pub(super) async fn graver_compilation(
    State(state): State<AppState>,
    Json(demande): Json<DemandeGravure>,
) -> impl IntoResponse {
    if demande.album_ids.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "aucun album", "motif": "album_ids_vide"})),
        )
            .into_response();
    }
    let albums = AlbumRepo::with_backend(state.backend.clone());
    let pistes = TrackRepo::with_backend(state.backend.clone());
    let mut ecrits = 0usize;
    let mut echecs: Vec<Value> = Vec::new();
    let mut sans_decision: Vec<i64> = Vec::new();

    for id in &demande.album_ids {
        let valeur = match albums.compilation_manuelle(*id) {
            Ok(Some(v)) => v,
            Ok(None) => {
                // Rien n'a été tranché : on ne grave pas une déduction du scan
                // dans les fichiers de l'utilisateur.
                sans_decision.push(*id);
                continue;
            }
            Err(e) => {
                echecs.push(json!({ "album_id": id, "erreur": e.to_string() }));
                continue;
            }
        };
        let mut champs = HashMap::new();
        champs.insert(
            "compilation".to_string(),
            if valeur {
                "1".to_string()
            } else {
                String::new()
            },
        );
        for piste in pistes.list_by_album(*id).unwrap_or_default() {
            let Some(chemin) = piste.file_path.as_deref() else {
                continue;
            };
            match tune_core::metadata::tag_writer::write_metadata_to_file(chemin, &champs).await {
                Ok(_) => ecrits += 1,
                Err(e) => {
                    warn!(album_id = *id, chemin, error = %e, "compilation_gravure_echec");
                    echecs.push(json!({ "album_id": id, "fichier": chemin, "erreur": e }));
                }
            }
        }
    }

    info!(
        albums = demande.album_ids.len(),
        ecrits,
        echecs = echecs.len(),
        sans_decision = sans_decision.len(),
        "compilation_gravee"
    );
    (
        StatusCode::OK,
        Json(json!({
            "fichiers_ecrits": ecrits,
            "echecs": echecs,
            // Nommé plutôt que silencieux : un album sans décision manuelle
            // n'est pas une erreur, mais l'écran doit pouvoir le dire.
            "sans_decision": sans_decision,
        })),
    )
        .into_response()
}
