//! Graver le drapeau « compilation » de Tune dans les fichiers.
//!
//! Demandé par Bertrand le 18/09/2026, depuis l'écran Métadonnées, en même
//! temps que le bouton qui pose le drapeau (#4427) : la base retient le choix
//! **tout de suite**, et graver est une **seconde action, explicite**. C'est
//! l'arbitrage — l'utilisateur doit savoir quand Tune touche à ses fichiers,
//! donc la gravure ne peut pas être un effet de bord du premier geste.
//!
//! ## Ce que la passe écrit
//!
//! Pour chaque album demandé, la valeur que `albums.is_compilation` porte
//! **maintenant** : `1` ou `0`, sous `ItemKey::FlagCompilation` — `TCMP` en
//! ID3, `COMPILATION` en VorbisComment, l'atome `cpil` en MP4. C'est la clé
//! exacte que le scan relit (`metadata/mod.rs`, `lire_drapeau_compilation`),
//! et lui seul : graver ailleurs ne servirait personne.
//!
//! `0` est écrit, pas effacé. Un champ absent et un champ à zéro ne disent pas
//! la même chose — le lecteur du scan les sépare depuis la phase 1 (C1), et un
//! `0` explicite est ce qui empêche la forme des dossiers de reprendre la main
//! sur une compilation que l'utilisateur vient de refuser.
//!
//! ## Ce qu'elle ne fait pas
//!
//! * **Un conteneur que le scan ne relit pas** (WAV, DSF, DFF, Matroska…)
//!   n'est pas touché : il est COMPTÉ à part (`hors_format`), pour que l'écran
//!   puisse le dire au lieu d'annoncer un faux « fait ». Même règle que
//!   [`super::graver_dr`].
//! * **Aucune tâche de fond.** La passe DR parcourt la bibliothèque entière ;
//!   celle-ci porte sur une sélection faite à la main — douze albums, quelques
//!   centaines de fichiers au pire. Une réponse directe évite au client
//!   d'avoir à suivre un registre pour un geste qui dure une seconde.
//! * **Elle ne change pas la base.** Le drapeau y est déjà, posé par
//!   `PUT /albums/{id}` ou `POST /albums/batch-update` ; la gravure ne fait que
//!   le porter jusqu'au disque.
use std::collections::HashMap;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::Deserialize;
use serde_json::json;
use tracing::{info, warn};
use tune_core::db::backend::ToSqlValue;
use tune_core::metadata::tag_writer::{TagFormat, detect_format, write_metadata_to_file};

use crate::state::AppState;

#[derive(Debug, Deserialize)]
pub(crate) struct GraverCompilationRequest {
    /// Les albums cochés dans l'écran Métadonnées.
    pub album_ids: Vec<i64>,
}

/// Le bilan d'une gravure, tel que l'écran l'annonce.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Bilan {
    /// Fichiers dont l'étiquette a été écrite.
    pub ecrits: usize,
    /// Fichiers écartés parce que le scan ne relit pas leur conteneur.
    pub hors_format: usize,
    /// Fichiers que lofty a refusés (manquant, verrouillé, illisible).
    pub echecs: usize,
    /// Albums effectivement traités — un identifiant inconnu ne compte pas.
    pub albums: usize,
}

/// Le scan relit-il le drapeau dans ce conteneur ?
///
/// `ItemKey::FlagCompilation` couvre les trois familles que
/// [`detect_format`] nomme, et lofty s'y charge de la correspondance. Tout le
/// reste — WAV, DSF, DFF, Matroska — tombe sur des chemins de repli qui ne
/// rendent pas le drapeau : y graver quelque chose serait écrire pour
/// personne.
pub(crate) fn conteneur_relu(chemin: &str) -> bool {
    !matches!(detect_format(chemin), TagFormat::Unknown)
}

/// Les pistes locales d'un album, avec la valeur du drapeau à graver.
///
/// 🔴 « Locales » se lit sur la SOURCE, pas sur le chemin de fichier. Le
/// prédicat a longtemps été `t.file_path IS NOT NULL AND t.file_path != ''`
/// seul, ce qui prenait le chemin pour un substitut de « local » : rien
/// n'interdit à une source distante d'en porter un, et cette route **écrit
/// dans les fichiers de l'utilisateur**. Les deux conditions restent, elles ne
/// répondent pas à la même question — « la ligne est locale » et « le fichier
/// est ouvrable ». Forme retenue :
/// [`tune_core::db::track_repo::sql::est_local`].
fn sql_pistes() -> String {
    format!(
        "SELECT COALESCE(al.is_compilation, 0), t.file_path \
         FROM albums al JOIN tracks t ON t.album_id = al.id \
         WHERE al.id = ?1 AND {piste_locale} \
           AND t.file_path IS NOT NULL AND t.file_path != '' \
         ORDER BY t.id",
        piste_locale = tune_core::db::track_repo::sql::PISTE_LOCALE,
    )
}

/// POST /library/albums/compilation/graver
pub(crate) async fn graver_compilation(
    State(state): State<AppState>,
    Json(body): Json<GraverCompilationRequest>,
) -> impl IntoResponse {
    if body.album_ids.is_empty() {
        return (StatusCode::BAD_REQUEST, "aucun album").into_response();
    }
    // Graver n'a pas d'autre effet que d'écrire dans les fichiers : désactivé
    // (le défaut), on refuse — le drapeau reste en base, où il est déjà.
    if !crate::routes::ecriture_fichiers::autorisee(&state) {
        return crate::routes::ecriture_fichiers::refus("graver_compilation");
    }

    let mut bilan = Bilan::default();
    for id in &body.album_ids {
        // `?1` tel quel : le dos PostgreSQL traduit les jetons lui-même
        // (`translate_placeholders`), comme pour tous les dépôts.
        let params: [&dyn ToSqlValue; 1] = [id];
        let rows = match state.backend.query_many(&sql_pistes(), &params) {
            Ok(r) => r,
            Err(e) => {
                warn!(album_id = id, erreur = %e, "graver_compilation_sql");
                continue;
            }
        };
        if rows.is_empty() {
            continue;
        }
        bilan.albums += 1;
        for row in &rows {
            let (Some(drapeau), Some(chemin)) = (
                row.first().and_then(|v| v.as_i64()),
                row.get(1).and_then(|v| v.as_string()),
            ) else {
                continue;
            };
            if !conteneur_relu(&chemin) {
                bilan.hors_format += 1;
                continue;
            }
            let mut champs = HashMap::new();
            champs.insert(
                "compilation".to_string(),
                if drapeau != 0 { "1" } else { "0" }.to_string(),
            );
            match write_metadata_to_file(&chemin, &champs).await {
                Ok(_) => bilan.ecrits += 1,
                Err(e) => {
                    warn!(fichier = %chemin, erreur = %e, "graver_compilation_echec");
                    bilan.echecs += 1;
                }
            }
        }
    }

    info!(
        albums = bilan.albums,
        ecrits = bilan.ecrits,
        hors_format = bilan.hors_format,
        echecs = bilan.echecs,
        "graver_compilation_termine"
    );
    Json(json!({
        "albums": bilan.albums,
        "ecrits": bilan.ecrits,
        "hors_format": bilan.hors_format,
        "echecs": bilan.echecs,
    }))
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::AppState;

    /// Le FLAC d'épreuve du dépôt, recopié dans un dossier temporaire. On
    /// n'écrit jamais dans la fixture d'origine.
    fn flac_temporaire(dir: &std::path::Path, nom: &str) -> String {
        let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../tune-core/tests/fixtures/test.flac");
        let cible = dir.join(nom);
        std::fs::copy(&source, &cible).expect("copie du FLAC d'épreuve");
        cible.to_string_lossy().to_string()
    }

    /// Ce que le SCAN lit dans le fichier — pas ce que nous croyons y avoir
    /// écrit. C'est toute la différence entre graver et graver pour personne.
    fn drapeau_vu_par_le_scan(chemin: &str) -> Option<bool> {
        tune_core::metadata::read_metadata(std::path::Path::new(chemin))
            .expect("relecture du fichier")
            .compilation
    }

    /// 🔴 #4427 — la gravure écrit la clé que le scan relit, dans les deux
    /// sens.
    ///
    /// Le fichier d'épreuve ne porte AUCUN champ « compilation » au départ :
    /// c'est le cas massif mesuré le 14/09 (23 677 FLAC sur 24 937). Le
    /// lecteur rend donc `None` — « le fichier ne dit rien ». Après gravure il
    /// doit dire quelque chose, et dire la bonne chose.
    #[tokio::test]
    async fn la_gravure_ecrit_la_cle_que_le_scan_relit() {
        let dir = tempfile::tempdir().expect("dossier temporaire");
        let chemin = flac_temporaire(dir.path(), "piste.flac");
        assert_eq!(
            drapeau_vu_par_le_scan(&chemin),
            None,
            "le fichier d'épreuve ne doit rien affirmer au départ"
        );

        let state = AppState::new(":memory:", 0, Default::default()).expect("état");
        let b = &state.backend;
        b.execute(
            "INSERT INTO albums (id, title, is_compilation) VALUES (1, 'Coco María Presents', 1)",
            &[],
        )
        .expect("album");
        b.execute(
            "INSERT INTO tracks (title, format, file_path, album_id) VALUES ('Pê Patu Pá', 'flac', ?1, 1)",
            &[&chemin as &dyn ToSqlValue],
        )
        .expect("piste");

        // Réglage jamais touché : refus, fichier intact.
        let refus = graver_compilation(
            State(state.clone()),
            Json(GraverCompilationRequest { album_ids: vec![1] }),
        )
        .await
        .into_response();
        assert_eq!(refus.status(), StatusCode::CONFLICT);
        assert_eq!(
            drapeau_vu_par_le_scan(&chemin),
            None,
            "réglage désactivé : la gravure ne doit pas toucher au fichier"
        );

        crate::routes::ecriture_fichiers::activer_pour_test(&state.backend);
        let corps = GraverCompilationRequest { album_ids: vec![1] };
        let _ = graver_compilation(State(state.clone()), Json(corps)).await;
        assert_eq!(
            drapeau_vu_par_le_scan(&chemin),
            Some(true),
            "après gravure, le scan doit lire le drapeau levé"
        );

        // L'autre sens : un `0` explicite, et non l'effacement du champ. Le
        // lecteur doit lire « faux », pas « rien ».
        b.execute("UPDATE albums SET is_compilation = 0 WHERE id = 1", &[])
            .expect("drapeau baissé");
        let _ = graver_compilation(
            State(state.clone()),
            Json(GraverCompilationRequest { album_ids: vec![1] }),
        )
        .await;
        assert_eq!(
            drapeau_vu_par_le_scan(&chemin),
            Some(false),
            "un refus gravé doit se lire « faux », jamais « absent »"
        );
    }

    /// Un conteneur que le scan ne relit pas est COMPTÉ, jamais gravé : sans
    /// quoi l'écran annoncerait un « fait » que le prochain scan démentirait.
    #[test]
    fn un_conteneur_non_relu_n_est_pas_grave() {
        assert!(conteneur_relu("/musique/piste.flac"));
        assert!(conteneur_relu("/musique/piste.mp3"));
        assert!(conteneur_relu("/musique/piste.m4a"));
        assert!(!conteneur_relu("/musique/piste.wav"));
        assert!(!conteneur_relu("/musique/piste.dsf"));
        assert!(!conteneur_relu("/musique/piste.dff"));
    }
}

/// Témoins de la règle « bibliothèque LOCALE » — Bertrand, 27/09/2026.
#[cfg(test)]
mod tests_source_locale_20260927 {
    use super::*;
    use crate::state::AppState;

    fn banc() -> AppState {
        let s = AppState::new(":memory:", 0, Default::default()).expect("état");
        let b = &s.backend;
        b.execute(
            "INSERT INTO albums (id, title, is_compilation) VALUES (1, 'Anthologie', 1)",
            &[],
        )
        .expect("album");
        // 🔴 La piste distante PORTE un chemin : c'est le cas que la base de
        // Bertrand ne contient pas (0 des 49 440 pistes `upnp` du .18 en a un),
        // et sans lui le filtre par chemin rendrait le même résultat.
        for (id, chemin, source) in [
            (10i64, "/m/anthologie/01.flac", "local"),
            (11, "/u/anthologie/01.flac", "upnp"),
        ] {
            let c = chemin.to_string();
            let s2 = source.to_string();
            b.execute(
                "INSERT INTO tracks (id, title, album_id, file_path, source) \
                 VALUES (?1, ?2, 1, ?3, ?4)",
                &[&id as &dyn ToSqlValue, &format!("p{id}"), &c, &s2],
            )
            .expect("piste");
        }
        s
    }

    fn pistes_retenues(s: &AppState) -> Vec<String> {
        let id = 1i64;
        let params: [&dyn ToSqlValue; 1] = [&id];
        s.backend
            .query_many(&sql_pistes(), &params)
            .expect("requête")
            .iter()
            .filter_map(|r| r.get(1).and_then(|v| v.as_string()))
            .collect()
    }

    /// 🔴 Cette route ÉCRIT DANS LES FICHIERS DE L'UTILISATEUR. Une piste non
    /// locale qui porte un chemin ne doit pas y entrer.
    #[test]
    fn la_gravure_ecarte_une_piste_non_locale_qui_porte_un_chemin() {
        let s = banc();
        let vus = pistes_retenues(&s);
        assert!(
            !vus.iter().any(|c| c.starts_with("/u/")),
            "la piste `source = upnp` ne doit PAS être gravée — retenues {vus:?}"
        );
    }

    /// L'AUTRE sens : sans lui, un filtre qui rejette tout serait vert.
    #[test]
    fn la_gravure_garde_la_piste_locale() {
        let s = banc();
        let vus = pistes_retenues(&s);
        assert!(
            vus.iter().any(|c| c == "/m/anthologie/01.flac"),
            "la piste LOCALE doit rester gravée — retenues {vus:?}"
        );
    }
}
