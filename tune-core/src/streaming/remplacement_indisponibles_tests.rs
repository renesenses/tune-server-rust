//! #8066 — critères, score et décision du remplacement d'un titre indisponible.

use super::*;

fn piste(id: &str, titre: &str, artiste: &str, album: &str, duree_ms: u64) -> StreamTrack {
    StreamTrack {
        id: id.into(),
        title: titre.into(),
        artist: artiste.into(),
        album: Some(album.into()).filter(|a: &String| !a.is_empty()),
        album_id: None,
        duration_ms: duree_ms,
        cover_path: None,
        track_number: None,
        disc_number: None,
        explicit: false,
        quality: None,
        isrc: None,
        disponible: Some(true),
        composer: None,
        artist_id: None,
    }
}

fn disparu() -> TitreCherche {
    TitreCherche {
        source_id: "111".into(),
        title: "Teardrop".into(),
        artist: "Massive Attack".into(),
        album: Some("Mezzanine".into()),
        duration_ms: Some(330_000),
        isrc: None,
    }
}

#[test]
fn seule_la_disponibilite_fausse_est_indisponible_8066() {
    let mut p = piste("1", "a", "b", "c", 1);
    p.disponible = Some(false);
    assert!(est_indisponible(&p));
    p.disponible = None;
    assert!(
        !est_indisponible(&p),
        "le service ne dit rien : on ne conclut pas"
    );
    p.disponible = Some(true);
    assert!(!est_indisponible(&p));
}

#[test]
fn la_republication_identique_est_sure_8066() {
    let s = score(
        &disparu(),
        &piste("222", "Teardrop", "Massive Attack", "Mezzanine", 330_000),
    )
    .expect("candidat");
    assert_eq!(s, 1.0);
    assert!(s >= SEUIL_SUR);
}

#[test]
fn deux_secondes_d_ecart_restent_sures_trois_non_8066() {
    let a_deux = score(
        &disparu(),
        &piste("2", "Teardrop", "Massive Attack", "Mezzanine", 332_000),
    )
    .expect("±2 s : candidat");
    assert!(a_deux >= SEUIL_SUR, "{a_deux}");
    assert!(
        score(
            &disparu(),
            &piste("3", "Teardrop", "Massive Attack", "Mezzanine", 332_001),
        )
        .is_none(),
        "au-delà de 2 s : pas candidat"
    );
}

#[test]
fn titre_artiste_ou_album_differents_ne_sont_pas_candidats_8066() {
    let c = disparu();
    assert!(
        score(
            &c,
            &piste("2", "Angel", "Massive Attack", "Mezzanine", 330_000)
        )
        .is_none()
    );
    assert!(
        score(
            &c,
            &piste("2", "Teardrop", "Elizabeth Fraser", "Mezzanine", 330_000)
        )
        .is_none()
    );
    assert!(
        score(
            &c,
            &piste("2", "Teardrop", "Massive Attack", "Zzz Qqq", 330_000)
        )
        .is_none()
    );
}

#[test]
fn la_normalisation_absorbe_casse_accents_et_remaster_8066() {
    let s = score(
        &disparu(),
        &piste(
            "2",
            "TEARDROP (2019 Remaster)",
            "massive attack",
            "Mezzanine (Remastered 2019)",
            330_000,
        ),
    )
    .expect("même titre normalisé");
    assert!(s >= SEUIL_SUR, "{s}");
}

#[test]
fn un_album_inconnu_ne_passe_jamais_le_seuil_8066() {
    let s = score(
        &disparu(),
        &piste("2", "Teardrop", "Massive Attack", "", 330_000),
    )
    .unwrap();
    assert!(s < SEUIL_SUR, "{s}");
    let mut sans_duree = disparu();
    sans_duree.duration_ms = None;
    let s = score(
        &sans_duree,
        &piste("2", "Teardrop", "Massive Attack", "Mezzanine", 330_000),
    )
    .unwrap();
    assert!(s < SEUIL_SUR, "durée inconnue : {s}");
}

#[test]
fn le_meme_identifiant_ou_un_candidat_indisponible_sont_ecartes_8066() {
    assert!(
        score(
            &disparu(),
            &piste("111", "Teardrop", "Massive Attack", "Mezzanine", 330_000)
        )
        .is_none()
    );
    let mut p = piste("2", "Teardrop", "Massive Attack", "Mezzanine", 330_000);
    p.disponible = Some(false);
    assert!(score(&disparu(), &p).is_none());
}

#[test]
fn un_isrc_contraire_plafonne_le_score_8066() {
    let mut c = disparu();
    c.isrc = Some("GBAAA9800001".into());
    let mut p = piste("2", "Teardrop", "Massive Attack", "Mezzanine", 330_000);
    p.isrc = Some("GBAAA1900001".into());
    assert_eq!(score(&c, &p), Some(PLAFOND_ISRC_DIFFERENT));
    p.isrc = Some("GB-AAA-98-00001".into());
    assert_eq!(score(&c, &p), Some(1.0));
}

#[test]
fn decision_direct_choix_aucun_8066() {
    let c = disparu();
    let sure = piste("2", "Teardrop", "Massive Attack", "Mezzanine", 330_000);
    let douteuse = piste("3", "Teardrop", "Massive Attack", "", 330_000);
    let un = classer(&c, &[douteuse.clone(), sure.clone()]);
    assert_eq!(un[0].piste.id, "2", "le plus sûr d'abord");
    assert_eq!(decider(&un), Decision::Direct);
    assert_eq!(candidat_direct(&un).map(|c| c.piste.id.as_str()), Some("2"));

    let mut jumelle = sure.clone();
    jumelle.id = "4".into();
    let deux = classer(&c, &[sure, jumelle]);
    assert_eq!(
        decider(&deux),
        Decision::Choix,
        "deux sûrs : l'utilisateur choisit"
    );
    assert!(candidat_direct(&deux).is_none());

    assert_eq!(decider(&classer(&c, &[douteuse])), Decision::Choix);
    assert_eq!(decider(&classer(&c, &[])), Decision::Aucun);
}
