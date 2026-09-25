//! #4837 (Dominique Pamingle, fils 1901/1902) — Edge Of Thorns (DE) recevait
//! la pochette de l'album *Edge of Thorns* de Savatage, déposée au dépôt
//! communautaire sous le BON MBID et servie en priorité.
//!
//! Côté Tune, le défaut vérifiable est le rejet local qui ne tenait pas : le
//! drapeau « image incorrecte » effaçait l'image, puis la passe
//! d'enrichissement suivante — phase communautaire par MBID d'abord —
//! ramenait les MÊMES octets. L'épreuve pose une vraie image dans un vrai
//! cache, signale par la fonction de production que les deux routes
//! appellent, puis juge avec le filtre qu'appliquent toutes les sources de la
//! cascade.

use super::*;
use crate::db::artist_repo::ArtistRepo;
use crate::db::backend::DbBackend;
use crate::db::metadata_report_repo::MetadataReportRepo;
use crate::db::models::Artist;
use std::sync::Arc;

const MBID_EDGE_OF_THORNS: &str = "6971fee8-3255-45fe-b34e-aabe13f7d271";

fn base() -> Arc<dyn DbBackend> {
    let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    Arc::new(db)
}

/// Des octets d'image distincts (le contenu importe peu : le cache et le
/// filtre travaillent sur l'empreinte des octets).
fn image(graine: u8) -> Vec<u8> {
    let mut v = vec![0xFF, 0xD8, 0xFF, 0xE0];
    v.extend((0..2048u32).map(|i| (i as u8).wrapping_mul(graine)));
    v
}

#[test]
fn une_image_d_artiste_rejetee_ne_revient_pas_a_la_passe_suivante_4837() {
    let db = base();
    let cache = crate::test_scratch::scratch_dir("image-rejetee-4837");
    let artistes = ArtistRepo::with_backend(db.clone());
    let mut a = Artist::new("Edge Of Thorns (DE)".into());
    a.musicbrainz_id = Some(MBID_EDGE_OF_THORNS.into());
    let id = artistes.create(&a).unwrap();

    // L'image fausse, posée comme la phase communautaire la pose.
    let fausse = image(3);
    let adresse = cache_fetched_image(&fausse, &cache, "jpg").expect("mise en cache");
    artistes.update_image(id, &adresse, "community").unwrap();

    // Le drapeau « image incorrecte ».
    let effacee = signaler_image_artiste(
        &db,
        &cache,
        id,
        None,
        "wrong_entity",
        None,
        "2026-09-23T21:00:00Z",
    )
    .expect("signalement");
    assert!(effacee, "l'image signalée est effacée localement");
    assert_eq!(artistes.get(id).unwrap().unwrap().image_path, None);

    // La passe suivante : ce que lit chaque source de la cascade.
    let refusees = MetadataReportRepo::with_backend(db.clone())
        .empreintes_d_image_rejetees(Some(id), Some(MBID_EDGE_OF_THORNS))
        .unwrap();
    assert!(
        image_retenue(fausse.clone(), &refusees).is_none(),
        "#4837 — l'image que l'utilisateur vient de rejeter ne doit pas être \
         reposée par l'enrichissement suivant (dépôt communautaire servi en \
         priorité sous le bon MBID). Empreintes gardées : {refusees:?}"
    );
    // Une AUTRE image pour le même artiste reste acceptée.
    assert_eq!(image_retenue(image(5), &refusees), Some(image(5)));

    // Le rejet survit à la recréation de la ligne `artists` (nouvel id, même
    // MBID) : un scan ou une fusion ne le fait pas oublier.
    let par_mbid = MetadataReportRepo::with_backend(db.clone())
        .empreintes_d_image_rejetees(Some(id + 1000), Some(MBID_EDGE_OF_THORNS))
        .unwrap();
    assert!(
        image_retenue(fausse, &par_mbid).is_none(),
        "#4837 — le rejet est gardé par MBID aussi. Empreintes : {par_mbid:?}"
    );
}

/// TÉMOIN VERT — sans rejet, rien n'est écarté ; un autre artiste n'hérite
/// pas du rejet d'Edge Of Thorns.
#[test]
fn sans_rejet_rien_n_est_ecarte_4837() {
    let db = base();
    let cache = crate::test_scratch::scratch_dir("image-rejetee-4837-vide");
    let artistes = ArtistRepo::with_backend(db.clone());
    let mut a = Artist::new("Edge Of Thorns (DE)".into());
    a.musicbrainz_id = Some(MBID_EDGE_OF_THORNS.into());
    let id = artistes.create(&a).unwrap();
    let savatage = artistes.create(&Artist::new("Savatage".into())).unwrap();
    let fausse = image(3);
    let adresse = cache_fetched_image(&fausse, &cache, "jpg").unwrap();
    artistes.update_image(id, &adresse, "community").unwrap();
    signaler_image_artiste(&db, &cache, id, None, "wrong_entity", None, "t").unwrap();

    let pour_savatage = MetadataReportRepo::with_backend(db.clone())
        .empreintes_d_image_rejetees(Some(savatage), Some("ac15222f-savatage"))
        .unwrap();
    assert!(pour_savatage.is_empty(), "{pour_savatage:?}");
    assert_eq!(image_retenue(fausse.clone(), &pour_savatage), Some(fausse));
}
