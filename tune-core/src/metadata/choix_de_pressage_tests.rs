//! Témoins du choix de pressage (#4805, étape D) — sans réseau.
use std::cell::{Cell, RefCell};

use serde_json::{Value, json};

use super::*;

fn candidat(id: &str, groupe: &str, titre: &str, artiste: &str, score: i32) -> MBReleaseMatch {
    MBReleaseMatch {
        release_id: id.into(),
        release_group_id: Some(groupe.into()),
        title: titre.into(),
        artist: artiste.into(),
        score,
        status: Some("Official".into()),
        ..Default::default()
    }
}

fn pistes(n: i32) -> Vec<LocalTrack> {
    (1..=n)
        .map(|i| LocalTrack {
            id: i as i64,
            disc: 1,
            position: i,
            title: format!("Piste {i}"),
        })
        .collect()
}

/// Une réponse `/release/{id}` de `n` pistes sur un disque.
fn detail_json(id: &str, groupe: &str, titre: &str, n: u32) -> Value {
    let tracks: Vec<Value> = (1..=n)
        .map(|i| {
            json!({
                "position": i,
                "number": i.to_string(),
                "title": format!("Piste {i}"),
                "recording": { "id": format!("rec-{id}-{i}") }
            })
        })
        .collect();
    json!({
        "id": id,
        "title": titre,
        "status": "Official",
        "release-group": { "id": groupe },
        "artist-credit": [{ "name": "Artiste", "joinphrase": "" }],
        "media": [{ "position": 1, "tracks": tracks }]
    })
}

// -- La règle de confiance --

#[test]
fn la_regle_de_confiance_de_metarust() {
    // ≥ 90 avec 10 points d'avance.
    assert!(score_nettement_devant(92, 82));
    assert!(!score_nettement_devant(92, 83));
    // ≥ 95 face à un second sous 90.
    assert!(score_nettement_devant(96, 89));
    assert!(!score_nettement_devant(96, 90));
    // Sous 90, jamais sur le score seul.
    assert!(!score_nettement_devant(89, 40));
    // L'ex aequo à 100.
    assert!(!score_nettement_devant(100, 100));
}

#[test]
fn un_candidat_seul_est_retenu() {
    let c = [candidat("a", "g", "Kind of Blue", "Miles Davis", 60)];
    assert_eq!(
        choisir_le_pressage(&c, "Kind of Blue", "Miles Davis", None),
        Choix::Retenu(0)
    );
    assert_eq!(choisir_le_pressage(&[], "X", "Y", None), Choix::Aucun);
}

#[test]
fn un_album_nettement_devant_est_retenu_sur_son_score() {
    let c = [
        candidat("a", "g1", "Requiem", "Mozart; Karajan", 100),
        candidat("b", "g2", "Ein deutsches Requiem", "Brahms; Karajan", 90),
    ];
    assert_eq!(
        choisir_le_pressage(&c, "Requiem", "Herbert von Karajan", None),
        Choix::Retenu(0)
    );
}

/// 🔴 Les pressages d'un MÊME album ne se font pas concurrence : sept fois 100
/// pour `Kind of Blue`, c'est un album, pas sept.
#[test]
fn les_pressages_d_un_meme_album_ne_le_rendent_pas_ambigu() {
    let mut c: Vec<MBReleaseMatch> = (0..7)
        .map(|i| {
            candidat(
                &format!("r{i}"),
                "rg-kob",
                "Kind of Blue",
                "Miles Davis",
                100,
            )
        })
        .collect();
    c[0].track_count = Some(19);
    c[1].track_count = Some(5);
    c[2].track_count = Some(6);
    // Le pressage aux 5 pistes des fichiers, même classé deuxième.
    assert_eq!(
        choisir_le_pressage(&c, "Kind of Blue", "Miles Davis", Some(5)),
        Choix::Retenu(1)
    );
}

#[test]
fn deux_albums_a_egalite_que_rien_ne_departage_sont_ambigus() {
    let c = [
        candidat("a", "g1", "Buddha‐Bar XXIV", "Various Artists", 100),
        candidat("b", "g2", "Buddha-Bar: Ocean", "Various Artists", 100),
        candidat("c", "g3", "Buddha-Bar: Perception", "Various Artists", 100),
    ];
    assert_eq!(
        choisir_le_pressage(&c, "Buddha-Bar", "Various Artists", Some(26)),
        Choix::Ambigu
    );
    // 🔴 Un autre album qui a juste le bon nombre de pistes ne gagne pas : le
    //    titre est obligatoire au départage (cas réel du banc).
    let mut best = candidat(
        "d",
        "g4",
        "Buddha-Bar Best Collection",
        "Various Artists",
        100,
    );
    best.track_count = Some(26);
    let c = [c[0].clone(), c[1].clone(), best];
    assert_eq!(
        choisir_le_pressage(&c, "Buddha-Bar", "Various Artists", Some(26)),
        Choix::Ambigu
    );
}

#[test]
fn un_seul_album_au_titre_exact_l_emporte() {
    // `Horses/Horses` (édition Legacy) à 100 devant `Horses` à 95 : le score
    // ne sépare pas, le titre si.
    let c = [
        candidat("legacy", "g-legacy", "Horses/Horses", "Patti Smith", 100),
        candidat("horses", "g-horses", "Horses", "Patti Smith", 95),
    ];
    assert_eq!(
        choisir_le_pressage(&c, "Horses", "Patti Smith", Some(8)),
        Choix::Retenu(1)
    );
}

#[test]
fn seul_dans_son_peloton_sous_90_il_faut_le_titre() {
    let c = [
        candidat("a", "g1", "Nocturnes: A Selection", "Chopin; Pires", 85),
        candidat("b", "g2", "Préludes", "Chopin; Pires", 60),
    ];
    assert_eq!(
        choisir_le_pressage(&c, "Nocturnes", "Maria João Pires", None),
        Choix::Ambigu
    );
    let c = [
        candidat("a", "g1", "Nocturnes", "Chopin; Pires", 85),
        candidat("b", "g2", "Préludes", "Chopin; Pires", 60),
    ];
    assert_eq!(
        choisir_le_pressage(&c, "Nocturnes", "Maria João Pires", None),
        Choix::Retenu(0)
    );
}

// -- Dvořák / Beethoven --

/// La réponse de recherche du cas de #4805 D : Dvořák classé devant Beethoven,
/// tous deux à 100, sous le même chef.
fn recherche_dvorak_beethoven() -> Value {
    json!({ "releases": [
        {
            "id": "rel-dvorak", "score": 100, "title": "Symphony no. 9", "status": "Official",
            "track-count": 4, "release-group": { "id": "rg-dvorak" },
            "artist-credit": [
                { "name": "Antonín Dvořák", "joinphrase": "; " },
                { "name": "Wiener Philharmoniker", "joinphrase": ", " },
                { "name": "Herbert von Karajan", "joinphrase": "" }
            ]
        },
        {
            "id": "rel-beethoven", "score": 100, "title": "Symphony no. 9", "status": "Official",
            "track-count": 4, "release-group": { "id": "rg-beethoven" },
            "artist-credit": [
                { "name": "Beethoven", "joinphrase": "; " },
                { "name": "Berliner Philharmoniker", "joinphrase": ", " },
                { "name": "Herbert von Karajan", "joinphrase": "" }
            ]
        }
    ]})
}

#[test]
fn dvorak_beethoven_le_compositeur_du_titre_departage() {
    let c = parse_search_results(
        &recherche_dvorak_beethoven(),
        "Symphony No. 9",
        "Herbert von Karajan",
    );
    assert_eq!(c.len(), 2);
    // Avant : le premier venu, Dvořák.
    assert_eq!(c[0].release_id, "rel-dvorak");
    // Après : le compositeur nommé en tête du titre local tranche.
    match choisir_le_pressage(
        &c,
        "Beethoven: Symphony No. 9",
        "Herbert von Karajan",
        Some(4),
    ) {
        Choix::Retenu(i) => assert_eq!(c[i].release_id, "rel-beethoven"),
        autre => panic!("Beethoven attendu, obtenu {autre:?}"),
    }
    // Sans diacritiques dans la balise, Dvořák se reconnaît encore.
    match choisir_le_pressage(&c, "Dvorak: Symphony No. 9", "Herbert von Karajan", Some(4)) {
        Choix::Retenu(i) => assert_eq!(c[i].release_id, "rel-dvorak"),
        autre => panic!("Dvořák attendu, obtenu {autre:?}"),
    }
    // Sans compositeur dans le titre : rien ne départage, on n'écrit rien.
    assert_eq!(
        choisir_le_pressage(&c, "Symphony No. 9", "Herbert von Karajan", Some(4)),
        Choix::Ambigu
    );
}

// -- Le suffixe d'édition --

#[test]
fn le_suffixe_d_edition_choisit_parmi_les_candidats() {
    // MetaRust : « Buhloone Mindstate (30th Anniversary) ». Le titre compact
    // (`Mindstate` = `Mind State`) et la désambiguïsation désignent l'édition.
    let data = json!({ "releases": [
        {
            "id": "rel-1993", "score": 100, "title": "Buhloone Mind State", "status": "Official",
            "release-group": { "id": "rg-1993" },
            "artist-credit": [{ "name": "De La Soul", "joinphrase": "" }]
        },
        {
            "id": "rel-2023", "score": 100, "title": "Buhloone Mind State", "status": "Official",
            "disambiguation": "30th anniversary", "release-group": { "id": "rg-2023" },
            "artist-credit": [{ "name": "De La Soul", "joinphrase": "" }]
        }
    ]});
    let titre = "Buhloone Mindstate (30th Anniversary)";
    // Jugés comme au second essai de la recherche, contre le titre interrogé
    // (sans son suffixe) : sans la comparaison compacte, `Mind State` n'y
    // passait pas.
    let interroge = crate::metadata::musicbrainz_release::titre_de_requete(titre).unwrap();
    assert_eq!(interroge, "Buhloone Mindstate");
    let c = parse_search_results(&data, &interroge, "De La Soul");
    assert_eq!(c.len(), 2, "le titre compact rend les deux plausibles");
    match choisir_le_pressage(&c, titre, "De La Soul", None) {
        Choix::Retenu(i) => assert_eq!(c[i].release_id, "rel-2023"),
        autre => panic!("l'édition anniversaire attendue, obtenu {autre:?}"),
    }
    // Sans suffixe : deux albums, rien ne départage.
    assert_eq!(
        choisir_le_pressage(&c, "Buhloone Mindstate", "De La Soul", None),
        Choix::Ambigu
    );
}

#[test]
fn le_suffixe_choisit_aussi_le_pressage_dans_un_album() {
    let mut std = candidat("std", "rg", "Nevermind", "Nirvana", 100);
    std.track_count = Some(13);
    let mut deluxe = candidat("dlx", "rg", "Nevermind", "Nirvana", 100);
    deluxe.disambiguation = Some("deluxe edition".into());
    deluxe.track_count = Some(40);
    let c = [std, deluxe];
    assert_eq!(
        choisir_le_pressage(&c, "Nevermind (Deluxe Edition)", "Nirvana", None),
        Choix::Retenu(1)
    );
    assert_eq!(
        choisir_le_pressage(&c, "Nevermind", "Nirvana", Some(13)),
        Choix::Retenu(0)
    );
}

// -- La garde de complétude --

fn mb_pistes(n: u32) -> Vec<MBTrack> {
    (1..=n)
        .map(|i| MBTrack {
            position: i,
            disc: 1,
            title: format!("Piste {i}"),
            ..Default::default()
        })
        .collect()
}

#[test]
fn la_garde_de_completude() {
    // Complet.
    let c = completude(&pistes(10), &mb_pistes(10));
    assert_eq!(
        (c.presentes, c.manquantes, c.en_double, c.en_trop),
        (10, 0, 0, 0)
    );
    assert!(c.acceptable(true));
    // Une piste de trop : le fichier 15 n'existe pas sur un pressage de 14.
    let c = completude(&pistes(15), &mb_pistes(14));
    assert_eq!(c.en_trop, 1);
    assert!(!c.acceptable(true) && !c.acceptable(false));
    // Un rang en double.
    let mut l = pistes(4);
    l[3].position = 3;
    let c = completude(&l, &mb_pistes(4));
    assert_eq!((c.en_double, c.manquantes), (1, 1));
    assert!(!c.acceptable(false));
    // 12 fichiers face au coffret de 40 : moins de la moitié.
    let c = completude(&pistes(12), &mb_pistes(40));
    assert_eq!(c.manquantes, 28);
    assert!(!c.acceptable(true));
    assert!(
        c.acceptable(false),
        "sans exigence de moitié, un album partiel passe"
    );
    // La moitié pile passe.
    assert!(completude(&pistes(20), &mb_pistes(40)).acceptable(true));
    // Des fichiers sans numéro comptent pour des présents possibles.
    let mut l = pistes(6);
    for p in &mut l {
        p.position = 0;
    }
    let c = completude(&l, &mb_pistes(10));
    assert_eq!(c.sans_numero, 6);
    assert!(c.acceptable(true));
}

// -- Les identifiants des balises --

#[test]
fn lecture_des_mbid_colles() {
    let id = "1f7e3c2a-4b5d-4e6f-8a9b-0c1d2e3f4a5b";
    assert!(est_un_mbid(id));
    assert!(!est_un_mbid("pas-un-mbid"));
    assert_eq!(normaliser_mbid(id).as_deref(), Some(id));
    assert_eq!(normaliser_mbid(&id.to_uppercase()).as_deref(), Some(id));
    assert_eq!(
        normaliser_mbid(&format!("https://musicbrainz.org/release/{id}?tab=x")).as_deref(),
        Some(id)
    );
    assert_eq!(normaliser_mbid(&format!("mbid={id};")).as_deref(), Some(id));
    assert_eq!(
        normaliser_mbid("é".repeat(40).as_str()),
        None,
        "pas de panique sur un texte multioctet"
    );
    assert_eq!(normaliser_mbid(""), None);
}

#[test]
fn la_release_majoritaire_des_balises() {
    let a = "1f7e3c2a-4b5d-4e6f-8a9b-0c1d2e3f4a5b".to_string();
    let b = "2f7e3c2a-4b5d-4e6f-8a9b-0c1d2e3f4a5b".to_string();
    assert_eq!(
        release_majoritaire(&[a.clone(), a.clone(), b.clone()], 4).as_deref(),
        Some(a.as_str())
    );
    // Moins de la moitié des pistes.
    assert_eq!(release_majoritaire(&[a.clone()], 4), None);
    // Deux releases à parts égales.
    assert_eq!(release_majoritaire(&[a.clone(), b.clone()], 2), None);
    assert_eq!(release_majoritaire(&["n'importe quoi".into()], 1), None);
}

#[test]
fn lecture_des_codes_barres() {
    assert_eq!(
        normaliser_code_barres("0 28947 75841 9").as_deref(),
        Some("028947758419")
    );
    assert_eq!(normaliser_code_barres("abc"), None);
    assert_eq!(normaliser_code_barres("1234"), None);
}

// -- La cascade --

/// Un transport qui panique : l'étape ne doit PAS l'appeler.
fn jamais_de_recherche(
    requete: String,
    _: usize,
) -> std::future::Ready<Result<Value, RefusMusicBrainz>> {
    panic!("aucune recherche texte attendue, reçu « {requete} »")
}

fn entree<'a>(
    titre: &'a str,
    artiste: &'a str,
    locales: &'a [LocalTrack],
    releases: &'a [String],
    enregistrements: &'a [String],
    codes: &'a [String],
) -> EntreeDIdentification<'a> {
    EntreeDIdentification {
        titre,
        artiste,
        pistes: locales,
        releases_des_balises: releases,
        enregistrements_des_balises: enregistrements,
        codes_barres: codes,
    }
}

/// 🔴 Le MBID que les balises portent déjà passe avant la recherche texte :
/// une seule lecture, aucune recherche.
#[tokio::test(start_paused = true)]
async fn le_mbid_des_balises_passe_avant_la_recherche() {
    let id = "1f7e3c2a-4b5d-4e6f-8a9b-0c1d2e3f4a5b";
    let locales = pistes(4);
    let balises = vec![id.to_string(); 3];
    let lectures = RefCell::new(Vec::new());
    let issue = identifier_le_pressage(
        entree("Titre", "Artiste", &locales, &balises, &[], &[]),
        jamais_de_recherche,
        |chemin: String, inc: &'static str| {
            lectures.borrow_mut().push(format!("{chemin}?{inc}"));
            std::future::ready(Ok(Some(detail_json(id, "rg-balise", "Titre", 4))))
        },
    )
    .await;
    assert_eq!(
        *lectures.borrow(),
        vec![format!("release/{id}?{INC_DETAIL}")]
    );
    match issue {
        IssueDuChoix::Retenu {
            pressage,
            detail,
            source,
            ..
        } => {
            assert_eq!(source, SourceDuPressage::BaliseRelease);
            assert_eq!(pressage.release_id, id);
            assert_eq!(pressage.release_group_id.as_deref(), Some("rg-balise"));
            assert_eq!(detail.tracks.len(), 4);
        }
        autre => panic!("pressage des balises attendu, obtenu {autre:?}"),
    }
}

#[tokio::test(start_paused = true)]
async fn un_mbid_des_balises_qui_ne_colle_pas_rend_l_album_ambigu() {
    let id = "1f7e3c2a-4b5d-4e6f-8a9b-0c1d2e3f4a5b";
    let locales = pistes(15);
    let balises = vec![id.to_string(); 15];
    let issue = identifier_le_pressage(
        entree("Titre", "Artiste", &locales, &balises, &[], &[]),
        jamais_de_recherche,
        |_: String, _: &'static str| {
            std::future::ready(Ok(Some(detail_json(id, "rg", "Titre", 14))))
        },
    )
    .await;
    assert!(
        matches!(
            issue,
            IssueDuChoix::Ambigu {
                raison: RaisonAmbigu::PistesIncompatibles,
                source: SourceDuPressage::BaliseRelease,
                ..
            }
        ),
        "{issue:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn un_mbid_inconnu_de_musicbrainz_laisse_place_a_la_recherche() {
    let id = "1f7e3c2a-4b5d-4e6f-8a9b-0c1d2e3f4a5b";
    let locales = pistes(5);
    let balises = vec![id.to_string(); 5];
    let recherches = Cell::new(0);
    let issue = identifier_le_pressage(
        entree("Kind of Blue", "Miles Davis", &locales, &balises, &[], &[]),
        |_: String, _: usize| {
            recherches.set(recherches.get() + 1);
            std::future::ready(Ok(json!({ "releases": [{
                "id": "rel-kob", "score": 100, "title": "Kind of Blue", "track-count": 5,
                "release-group": { "id": "rg-kob" },
                "artist-credit": [{ "name": "Miles Davis", "joinphrase": "" }]
            }]})))
        },
        |chemin: String, _: &'static str| {
            std::future::ready(Ok(if chemin.contains(id) {
                None
            } else {
                Some(detail_json("rel-kob", "rg-kob", "Kind of Blue", 5))
            }))
        },
    )
    .await;
    assert_eq!(recherches.get(), 1);
    assert!(
        matches!(&issue, IssueDuChoix::Retenu { source: SourceDuPressage::Recherche, pressage, .. } if pressage.release_id == "rel-kob"),
        "{issue:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn l_enregistrement_des_balises_mene_a_sa_release() {
    let rid = "3f7e3c2a-4b5d-4e6f-8a9b-0c1d2e3f4a5b";
    let locales = pistes(6);
    let enregistrements = vec![String::new(), rid.to_string()];
    let issue = identifier_le_pressage(
        entree(
            "Somethin' Else",
            "Cannonball Adderley",
            &locales,
            &[],
            &enregistrements,
            &[],
        ),
        jamais_de_recherche,
        |chemin: String, inc: &'static str| {
            std::future::ready(Ok(Some(if chemin.starts_with("recording/") {
                assert_eq!(inc, INC_ENREGISTREMENT);
                json!({ "releases": [
                    { "id": "rel-se", "title": "Somethin’ Else", "status": "Official" },
                    { "id": "rel-compil", "title": "Jazz Masters 100", "status": "Official" }
                ]})
            } else {
                assert_eq!(chemin, "release/rel-se");
                detail_json("rel-se", "rg-se", "Somethin’ Else", 6)
            })))
        },
    )
    .await;
    assert!(
        matches!(&issue, IssueDuChoix::Retenu { source: SourceDuPressage::BaliseEnregistrement, pressage, .. } if pressage.release_id == "rel-se"),
        "{issue:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn le_code_barres_passe_avant_la_recherche_texte() {
    let locales = pistes(9);
    let codes = vec!["0 28947 75841 9".to_string()];
    let requetes = RefCell::new(Vec::new());
    let issue = identifier_le_pressage(
        entree("Symphonies 5 & 7", "Carlos Kleiber", &locales, &[], &[], &codes),
        |requete: String, _: usize| {
            requetes.borrow_mut().push(requete);
            std::future::ready(Ok(json!({ "releases": [{
                "id": "rel-cb", "score": 100, "title": "Symphonies 5 & 7",
                "release-group": { "id": "rg-cb" },
                "artist-credit": [{ "name": "Beethoven; Wiener Philharmoniker, Carlos Kleiber", "joinphrase": "" }]
            }]})))
        },
        |_: String, _: &'static str| std::future::ready(Ok(Some(detail_json("rel-cb", "rg-cb", "Symphonies 5 & 7", 9)))),
    )
    .await;
    assert_eq!(*requetes.borrow(), vec!["barcode:028947758419".to_string()]);
    assert!(
        matches!(
            &issue,
            IssueDuChoix::Retenu {
                source: SourceDuPressage::CodeBarres,
                ..
            }
        ),
        "{issue:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn un_refus_arrete_la_cascade_et_se_dit() {
    let id = "1f7e3c2a-4b5d-4e6f-8a9b-0c1d2e3f4a5b";
    let locales = pistes(2);
    let balises = vec![id.to_string(); 2];
    let issue = identifier_le_pressage(
        entree("Titre", "Artiste", &locales, &balises, &[], &[]),
        jamais_de_recherche,
        |_: String, _: &'static str| std::future::ready(Err(RefusMusicBrainz::Statut(503))),
    )
    .await;
    assert!(
        matches!(issue, IssueDuChoix::Refus(RefusMusicBrainz::Statut(503))),
        "{issue:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn sans_balise_la_recherche_ambigue_n_ecrit_rien() {
    let locales = pistes(4);
    let issue = identifier_le_pressage(
        entree(
            "Symphony No. 9",
            "Herbert von Karajan",
            &locales,
            &[],
            &[],
            &[],
        ),
        |_: String, _: usize| std::future::ready(Ok(recherche_dvorak_beethoven())),
        |chemin: String,
         _: &'static str|
         -> std::future::Ready<Result<Option<Value>, RefusMusicBrainz>> {
            panic!("aucun détail ne doit être lu pour un album ambigu : {chemin}")
        },
    )
    .await;
    assert!(
        matches!(
            issue,
            IssueDuChoix::Ambigu {
                raison: RaisonAmbigu::AlbumsConcurrents,
                source: SourceDuPressage::Recherche,
                ..
            }
        ),
        "{issue:?}"
    );
}

/// L'album ambigu rend ses candidats : c'est la liste que le bouton
/// « Ré-identifier » propose à l'utilisateur.
#[tokio::test(start_paused = true)]
async fn un_album_ambigu_rend_ses_candidats() {
    let locales = pistes(4);
    let issue = identifier_le_pressage(
        entree(
            "Symphony No. 9",
            "Herbert von Karajan",
            &locales,
            &[],
            &[],
            &[],
        ),
        |_: String, _: usize| std::future::ready(Ok(recherche_dvorak_beethoven())),
        |_: String, _: &'static str| std::future::ready(Ok(None)),
    )
    .await;
    let IssueDuChoix::Ambigu { candidats, .. } = issue else {
        panic!("ambigu attendu, obtenu {issue:?}");
    };
    let ids: Vec<&str> = candidats.iter().map(|c| c.release_id.as_str()).collect();
    assert_eq!(ids, ["rel-dvorak", "rel-beethoven"]);
}

/// L'édition choisie par l'utilisateur est posée telle quelle, même si elle
/// ne colle pas aux fichiers : la complétude est rendue pour information.
#[tokio::test(start_paused = true)]
async fn le_pressage_choisi_par_l_utilisateur_est_lu_et_retenu() {
    let id = "1f7e3c2a-4b5d-4e6f-8a9b-0c1d2e3f4a5b";
    let locales = pistes(15);
    let lectures = RefCell::new(Vec::new());
    let issue = lire_le_pressage_choisi(
        &format!("https://musicbrainz.org/release/{id}"),
        &locales,
        |chemin: String, _: &'static str| {
            lectures.borrow_mut().push(chemin);
            std::future::ready(Ok(Some(detail_json(id, "rg-choisi", "Titre", 14))))
        },
    )
    .await
    .expect("un MBID valide");
    assert_eq!(*lectures.borrow(), vec![format!("release/{id}")]);
    match issue {
        IssueDuChoix::Retenu {
            pressage,
            source,
            completude,
            ..
        } => {
            assert_eq!(source, SourceDuPressage::ChoixUtilisateur);
            assert_eq!(pressage.release_id, id);
            assert_eq!(completude.en_trop, 1);
        }
        autre => panic!("pressage choisi attendu, obtenu {autre:?}"),
    }
    // Un identifiant qui n'en est pas un ne part pas vers MusicBrainz.
    let rien = lire_le_pressage_choisi(
        "pas un mbid",
        &locales,
        |_: String,
         _: &'static str|
         -> std::future::Ready<Result<Option<Value>, RefusMusicBrainz>> {
            panic!("aucune lecture attendue")
        },
    )
    .await;
    assert!(rien.is_none());
}
