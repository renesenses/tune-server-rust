//! `POST /system/import/roon` quand le fichier téléversé est un EXPORT DU
//! MOISSONNEUR (`{"source":"roon","artistes":[…]}`) — phase 2 du pont Roon
//! (#3914). Le CSV de l'interface Roon garde son chemin historique dans
//! `import.rs` ; ici on n'importe pas des pistes, on ENRICHIT celles qu'on a.
//!
//! Voir `tune_core::library::pont_roon` pour la lecture et les règles ; ce
//! fichier applique contre la base et écrit.
use std::collections::HashMap;

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;
use tracing::{info, warn};
use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::artist_repo::ArtistRepo;
use tune_core::db::backend::ToSqlValue;
use tune_core::db::track_metadata_repo::TrackMetadataRepo;
use tune_core::db::track_repo::TrackRepo;
use tune_core::library::pont_roon::{
    ExportRoon, PisteLocale, Rapport, apparier_piste, credits_a_ecrire, index_par_titre, plier,
};

use crate::state::AppState;

/// Marque d'origine, sur la piste : `track_metadata.credits_source = roon`.
/// C'est la garde de tout le chantier — ce qui vient de Roon se reconnaît, et
/// reste local (voir le témoin `le_sync_cloud_ne_pousse_ni_credits_ni_images`).
pub(crate) const CLE_PROVENANCE: &str = "credits_source";
pub(crate) const PROVENANCE_ROON: &str = "roon";
/// Le rôle donné aux noms que Roon ajoute à l'interprète — voir `pont_roon`.
const ROLE: &str = "composer";

/// L'entrée : le texte téléversé est-il un export du moissonneur ?
pub(crate) fn est_un_export_du_pont(texte: &str) -> bool {
    texte.trim_start().starts_with('{')
        && texte.contains("\"source\"")
        && texte.contains("\"artistes\"")
}

/// Applique (ou aperçoit) un export contre la bibliothèque.
pub(crate) fn appliquer(state: &AppState, export: &ExportRoon, apercu: bool) -> Rapport {
    let backend = state.backend.clone();
    let artistes = ArtistRepo::with_backend(backend.clone());
    let albums = AlbumRepo::with_backend(backend.clone());
    let pistes = TrackRepo::with_backend(backend.clone());
    let meta = TrackMetadataRepo::with_backend(backend.clone());

    // Les artistes locaux, une fois, repliés.
    let locaux: Vec<(i64, String)> = artistes
        .list_all_id_name_mbid()
        .unwrap_or_default()
        .into_iter()
        .map(|(id, nom, _)| (id, nom))
        .collect();
    let par_nom: HashMap<String, usize> = index_par_titre(&locaux, |(_, n)| n.as_str());

    let mut r = Rapport {
        artistes_total: export.artistes.len(),
        ..Default::default()
    };
    for ar in &export.artistes {
        r.albums_total += ar.albums.len();
        r.pistes_total += ar.albums.iter().map(|a| a.pistes.len()).sum::<usize>();
        if ar.image.is_some() {
            r.images_nommees += 1;
        }
        let Some(&i) = par_nom.get(&plier(&ar.nom)) else {
            r.artistes_inconnus.push(ar.nom.clone());
            continue;
        };
        r.artistes_apparies += 1;
        let (artiste_id, artiste_nom) = &locaux[i];
        let siens = albums.list_by_artist(*artiste_id).unwrap_or_default();
        let par_titre = index_par_titre(&siens, |a| a.title.as_str());
        for al in &ar.albums {
            r.images_nommees += usize::from(al.image.is_some());
            let Some(&j) = par_titre.get(&plier(&al.titre)) else {
                r.albums_inconnus.push(format!("{} — {}", ar.nom, al.titre));
                continue;
            };
            r.albums_apparies += 1;
            let Some(album_id) = siens[j].id else {
                continue;
            };
            let locales: Vec<PisteLocale> = pistes
                .list_by_album(album_id)
                .unwrap_or_default()
                .into_iter()
                .filter_map(|t| {
                    let id = t.id?;
                    Some(PisteLocale {
                        id,
                        titre: t.title.clone(),
                        numero: (t.track_number > 0).then_some(t.track_number),
                        disque: (t.disc_number > 0).then_some(t.disc_number),
                        artiste: t.artist_name.clone(),
                        a_des_credits: a_des_credits(&backend, id),
                    })
                })
                .collect();
            for p in &al.pistes {
                let Some(locale) = apparier_piste(p, &locales) else {
                    continue;
                };
                r.pistes_appariees += 1;
                let Some(ligne) = p.credits.as_deref().filter(|c| !c.trim().is_empty()) else {
                    continue;
                };
                let mut interpretes: Vec<&str> = vec![artiste_nom.as_str(), ar.nom.as_str()];
                if let Some(a) = locale.artiste.as_deref() {
                    interpretes.push(a);
                }
                let noms = credits_a_ecrire(ligne, &interpretes);
                if noms.is_empty() {
                    continue;
                }
                if locale.a_des_credits {
                    r.credits_deja_presents += 1;
                    continue;
                }
                r.credits_a_ecrire += 1;
                if apercu {
                    continue;
                }
                let ecrits = ecrire(&backend, &artistes, locale.id, &noms);
                if ecrits > 0 {
                    r.credits_ecrits += 1;
                    let _ = meta.set(locale.id, CLE_PROVENANCE, PROVENANCE_ROON);
                }
            }
        }
    }
    r
}

fn a_des_credits(
    backend: &std::sync::Arc<dyn tune_core::db::backend::DbBackend>,
    track_id: i64,
) -> bool {
    let id = track_id.to_string();
    backend
        .query_one(
            "SELECT 1 FROM track_credits WHERE track_id = ? LIMIT 1",
            &[&id as &dyn ToSqlValue],
        )
        .ok()
        .flatten()
        .is_some()
}

/// Même écriture que `credits::ecrire_credits` — identifiants en CHAÎNE pour
/// le miroir PostgreSQL, fiche artiste LIÉE si elle existe, jamais créée.
fn ecrire(
    backend: &std::sync::Arc<dyn tune_core::db::backend::DbBackend>,
    artistes: &ArtistRepo,
    track_id: i64,
    noms: &[String],
) -> usize {
    let id = track_id.to_string();
    let mut n = 0;
    for (pos, nom) in noms.iter().enumerate() {
        let artist_id: Option<String> = artistes
            .get_by_name(nom)
            .ok()
            .flatten()
            .and_then(|a| a.id)
            .map(|i| i.to_string());
        let pos = pos as i32;
        if backend
            .execute(
                "INSERT INTO track_credits (track_id, artist_id, artist_name, role, instrument, position) \
                 VALUES (?, ?, ?, ?, NULL, ?)",
                &[
                    &id as &dyn ToSqlValue,
                    &artist_id as &dyn ToSqlValue,
                    nom as &dyn ToSqlValue,
                    &ROLE as &dyn ToSqlValue,
                    &pos as &dyn ToSqlValue,
                ],
            )
            .is_ok()
        {
            n += 1;
        }
    }
    n
}

/// La réponse HTTP : le rapport, en aperçu ou après écriture.
pub(crate) fn repondre(state: &AppState, texte: &str, apercu: bool) -> Response {
    let export = match ExportRoon::lire(texte) {
        Ok(e) => e,
        Err(e) => {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(json!({"error": "export_pont_roon_illisible", "detail": e})),
            )
                .into_response();
        }
    };
    let rapport = appliquer(state, &export, apercu);
    if apercu {
        info!(?rapport, "pont_roon_apercu_rendu_sans_ecriture");
    } else {
        info!(?rapport, "pont_roon_importe");
    }
    if rapport.artistes_apparies == 0 {
        warn!(
            artistes = rapport.artistes_total,
            "pont_roon_aucun_artiste_apparie — l'export vient-il de la même bibliothèque ?"
        );
    }
    let mut v = serde_json::to_value(&rapport).unwrap_or_default();
    v["preview"] = json!(apercu);
    v["source"] = json!("roon_pont");
    v["absent_de_l_api"] = json!(export.absent_de_l_api);
    (StatusCode::OK, Json(v)).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn etat() -> AppState {
        AppState::new(":memory:", 0, Default::default()).unwrap()
    }

    /// Une bibliothèque de deux artistes, un album, deux pistes.
    fn bibliotheque(s: &AppState) -> (i64, i64) {
        let b = &s.backend;
        b.execute(
            "INSERT INTO artists (id, name) VALUES (1, '16 Horsepower'), (2, 'Hank Williams')",
            &[],
        )
        .unwrap();
        b.execute(
            "INSERT INTO albums (id, title, artist_id) VALUES (1, 'Folklore', 1)",
            &[],
        )
        .unwrap();
        b.execute(
            "INSERT INTO tracks (id, title, album_id, artist_id, track_number, file_path, source) VALUES \
             (10, 'Hutterite Mile', 1, 1, 1, '/m/1.flac', 'local'), \
             (11, 'Alone and Forsaken', 1, 1, 4, '/m/4.flac', 'local')",
            &[],
        )
        .unwrap();
        (10, 11)
    }

    const EXPORT: &str = r#"{"source":"roon","core":"x","artistes":[
      {"nom":"16 horsepower","image":"ce1d","albums":[{"titre":"FOLKLORE","image":"5c46","pistes":[
        {"titre":"1. Hutterite Mile","credits":"16 Horsepower, David Eugene Edwards"},
        {"titre":"4. Alone and Forsaken","credits":"16 Horsepower, Hank Williams"},
        {"titre":"7. Inconnue","credits":"16 Horsepower, X"}]}]},
      {"nom":"Nick Drake","albums":[{"titre":"Pink Moon","pistes":[]}]}
    ],"absent_de_l_api":["biographies"]}"#;

    fn credits_de(s: &AppState, id: i64) -> Vec<(String, String, Option<i64>)> {
        s.backend
            .query_many(
                "SELECT artist_name, role, artist_id FROM track_credits WHERE track_id = ? ORDER BY position",
                &[&id.to_string() as &dyn ToSqlValue],
            )
            .unwrap()
            .iter()
            .map(|r| {
                (
                    r.first().and_then(|v| v.as_string()).unwrap_or_default(),
                    r.get(1).and_then(|v| v.as_string()).unwrap_or_default(),
                    r.get(2).and_then(|v| v.as_i64()),
                )
            })
            .collect()
    }

    /// L'aperçu compte tout et n'écrit rien ; l'import écrit ce que l'aperçu
    /// a compté, marque la provenance, lie la fiche artiste quand elle existe.
    #[test]
    fn apercu_puis_import_sur_l_export_de_fabien() {
        let s = etat();
        let (p1, p4) = bibliotheque(&s);
        let export = ExportRoon::lire(EXPORT).unwrap();

        let r = appliquer(&s, &export, true);
        assert_eq!((r.artistes_total, r.artistes_apparies), (2, 1), "{r:?}");
        assert_eq!(r.artistes_inconnus, vec!["Nick Drake"]);
        assert_eq!((r.albums_total, r.albums_apparies), (2, 1));
        assert_eq!((r.pistes_total, r.pistes_appariees), (3, 2));
        assert_eq!(
            (r.credits_a_ecrire, r.credits_ecrits),
            (2, 0),
            "aperçu : rien d'écrit"
        );
        assert_eq!(
            (r.images_nommees, r.images_portees),
            (2, 0),
            "l'export nomme, ne porte pas"
        );
        assert!(credits_de(&s, p1).is_empty());

        let r = appliquer(&s, &export, false);
        assert_eq!(r.credits_ecrits, 2, "{r:?}");
        assert_eq!(
            credits_de(&s, p1),
            vec![(
                "David Eugene Edwards".to_string(),
                "composer".to_string(),
                None
            )]
        );
        // Hank Williams existe comme artiste local : la fiche est LIÉE.
        assert_eq!(
            credits_de(&s, p4),
            vec![("Hank Williams".to_string(), "composer".to_string(), Some(2))]
        );
        let m = TrackMetadataRepo::with_backend(s.backend.clone());
        assert_eq!(
            m.get_all(p1)
                .unwrap()
                .get(CLE_PROVENANCE)
                .map(String::as_str),
            Some("roon")
        );

        // Second import : les crédits existent, on ne double pas.
        let r = appliquer(&s, &export, false);
        assert_eq!((r.credits_deja_presents, r.credits_ecrits), (2, 0), "{r:?}");
        assert_eq!(credits_de(&s, p1).len(), 1);
    }

    /// Le format est refusé quand ce n'est pas le nôtre, et le CSV ne passe
    /// pas par ici.
    #[test]
    fn la_porte_ne_prend_que_l_export_du_moissonneur() {
        assert!(est_un_export_du_pont(EXPORT));
        assert!(!est_un_export_du_pont("Title,Artist\nA,B"));
        assert!(!est_un_export_du_pont(r#"{"data":[]}"#));
        let s = etat();
        let rep = repondre(&s, r#"{"source":"plex","artistes":[]}"#, true);
        assert_eq!(rep.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    /// 🔴 LA garde du chantier : ce qui vient de Roon reste LOCAL. Le sync
    /// cloud pousse artistes (nom, bio, MBID) et albums — jamais
    /// `track_credits`, jamais `track_metadata`, jamais une image.
    #[test]
    fn le_sync_cloud_ne_pousse_ni_credits_ni_images() {
        const SYNC: &str = include_str!("../../../../tune-core/src/cloud/library_sync.rs");
        let sans = SYNC
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !sans.contains("track_credits"),
            "le sync cloud lit track_credits"
        );
        assert!(
            !sans.contains("track_metadata"),
            "le sync cloud lit track_metadata"
        );
        assert!(
            !sans.contains("image_path"),
            "le sync cloud pousse des images"
        );
    }
}
