//! La vue Oxygen ne liste plus deux fois la même piste (#4101).
//!
//! ## Ce que JeromeQ voyait — forum fil 1781, 13/09/2026
//!
//! Fiche album **Led Zeppelin IV**, huit pistes sur le disque, **seize lignes**
//! à l'écran : chaque titre deux fois, avec le même numéro de piste. La fiche
//! de l'album, elle, en montrait huit ; la file en enfilait huit.
//!
//! ## Où naît le doublon — et pourquoi cette épreuve interroge la ROUTE
//!
//! Le « 16 titres » du bandeau ne prouve rien : c'est `g.tracks.length` côté
//! client, la longueur de la LISTE. Et la pagination d'Oxygen écarte déjà les
//! identifiants qu'elle connaît, donc `loadMore` ne peut pas fabriquer ces
//! lignes. Restait à trancher entre trois causes, et la lecture du code les
//! départage :
//!
//! * **pas une jointure** : les trois jointures de `TrackRepo::sql::track_from`
//!   portent sur des clés primaires, et toutes les facettes sont des `EXISTS` —
//!   aucune ne peut multiplier une ligne ;
//! * **pas la pagination** : le client déduplique par `id` d'une fenêtre à
//!   l'autre ;
//! * **ce sont deux lignes `tracks` réelles**, une par fichier — le cas #1362
//!   de Cyrille Moutia (un CD rippé, et le même morceau récupéré ailleurs,
//!   posé dans le dossier de l'album). Le repli de ces copies existe depuis
//!   #1362, mais il n'était posé QUE sur la fiche d'album
//!   (`dedup_display_tracks`), la file (`resoudre_pistes_d_album`) et le
//!   compteur `albums.track_count` (`sql_compte_pistes_visibles`). Jamais sur
//!   `GET /library/tracks` — **la seule route que la vue Oxygen appelle.**
//!
//! ## Ce que cette épreuve mesure
//!
//! Le corps JSON de la ROUTE, sur ses DEUX chemins — non facetté
//! (`TrackRepo::list_visible`) et facetté (`list_filtered`) —, jamais une
//! condition SQL : un témoin qui rejouerait le prédicat le recopierait au lieu
//! de le garder.
//!
//! 🔴 Et elle compte des **identifiants distincts**, jamais des lignes : c'est
//! le piège nommé par l'issue elle-même. Un compteur qui compte les lignes
//! verdirait sur seize lignes tout autant que sur huit.
//!
//! ## Le banc, et pourquoi il porte autant de cas
//!
//! | ligne | rôle |
//! |---|---|
//! | 8 FLAC de Led Zeppelin IV | l'album du rapport |
//! | 3 copies AAC de trois d'entre elles | le doublon à replier — et le barème à vérifier : c'est le FLAC qui survit |
//! | 1 titre répété dans le même album sous un AUTRE numéro | le repli ne doit PAS manger deux morceaux distincts |
//! | 1 piste d'un AUTRE album, même titre et même numéro | l'album borne le repli |
//! | 2 pistes d'un album témoin | l'ouverture n'a rien retiré à ce qui marchait |
//!
//! Sans la ligne « même titre, autre numéro » et sans la ligne « autre album »,
//! un prédicat trop gourmand — qui replierait sur le seul titre — passerait au
//! vert en cassant la bibliothèque.

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use std::collections::HashSet;
use tower::ServiceExt;

use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::artist_repo::ArtistRepo;
use tune_core::db::models::{Album, Track};
use tune_core::db::track_repo::TrackRepo;
use tune_server::state::AppState;

const ALBUM: &str = "Led Zeppelin IV";
const ARTISTE: &str = "Led Zeppelin";
const ANNEE: i32 = 1971;

const TEMOIN_ALBUM: &str = "Physical Graffiti";

/// Les huit pistes du disque, dans l'ordre.
const PISTES: [&str; 8] = [
    "Black Dog",
    "Rock and Roll",
    "The Battle of Evermore",
    "Stairway to Heaven",
    "Misty Mountain Hop",
    "Four Sticks",
    "Going to California",
    "When the Levee Breaks",
];

/// Celles dont une copie de MOINDRE qualité traîne dans le même dossier.
const DOUBLEES: [usize; 3] = [0, 1, 2];

/// Le titre répété DANS l'album sous un autre numéro : deux morceaux distincts
/// qui portent le même nom ne doivent pas se replier l'un sur l'autre.
const HOMONYME_DANS_L_ALBUM: &str = "Black Dog";
const HOMONYME_NUMERO: i64 = 9;

/// Ce que la vue doit rendre pour cet album : huit pistes, plus l'homonyme.
const VISIBLES_DANS_L_ALBUM: usize = PISTES.len() + 1;

/// Plancher du détecteur : un banc appauvri doit ROUGIR, pas passer à vide.
fn exige_un_banc_qui_departage() {
    assert!(
        !DOUBLEES.is_empty(),
        "sans copie doublée, l'épreuve verdirait sur une base déjà propre — \
         elle ne garderait rien"
    );
    assert!(
        PISTES.contains(&HOMONYME_DANS_L_ALBUM),
        "l'homonyme doit porter le titre d'une piste RÉELLE de l'album, \
         sinon il ne teste pas la clé du repli"
    );
    assert_ne!(
        HOMONYME_NUMERO, 0,
        "l'homonyme doit porter un numéro DIFFÉRENT des huit pistes"
    );
}

/// Pose le banc. Rend le nombre de lignes `tracks` réellement écrites — ce que
/// la vue ne doit PLUS rendre.
fn poser_le_banc(etat: &AppState) -> usize {
    let artistes = ArtistRepo::with_backend(etat.backend.clone());
    let albums = AlbumRepo::with_backend(etat.backend.clone());
    let pistes = TrackRepo::with_backend(etat.backend.clone());

    let artiste_id = artistes
        .get_or_create(ARTISTE, None, None)
        .expect("artiste")
        .id;

    let creer_album = |titre: &str| -> i64 {
        let mut album = Album::new(titre.to_string());
        album.artist_id = artiste_id;
        album.year = Some(ANNEE);
        albums
            .create(&album)
            .unwrap_or_else(|e| panic!("{titre} : {e}"))
    };
    let album_id = creer_album(ALBUM);
    let temoin_id = creer_album(TEMOIN_ALBUM);

    let mut ecrites = 0usize;
    let mut poser = |album: i64, titre: &str, numero: i32, format: &str, sr: i32, bd: i32| {
        let mut t = Track::new(titre.to_string());
        t.album_id = Some(album);
        t.artist_id = artiste_id;
        t.disc_number = 1;
        t.track_number = numero;
        t.year = Some(ANNEE);
        t.format = Some(format.to_string());
        t.sample_rate = Some(sr);
        t.bit_depth = Some(bd);
        // Le `file_path` est ce qui rend ces deux lignes LÉGITIMES en base :
        // deux fichiers existent, la contrainte `UNIQUE` est respectée, et
        // c'est bien l'AFFICHAGE qui doit choisir.
        t.file_path = Some(format!("/musique/{album}/{numero:02}-{titre}.{format}"));
        pistes
            .create(&t)
            .unwrap_or_else(|e| panic!("{titre} ({format}) : {e}"));
        ecrites += 1;
    };

    for (i, titre) in PISTES.iter().enumerate() {
        let numero = i as i32 + 1;
        poser(album_id, titre, numero, "flac", 44100, 16);
        if DOUBLEES.contains(&i) {
            // La copie de moindre qualité : même album, même disque, même
            // numéro, même titre — exactement la clé du repli #1362.
            poser(album_id, titre, numero, "aac", 48000, 24);
        }
    }

    // Deux morceaux DISTINCTS qui portent le même nom dans le même album.
    poser(
        album_id,
        HOMONYME_DANS_L_ALBUM,
        HOMONYME_NUMERO as i32,
        "flac",
        44100,
        16,
    );

    // Même titre, même numéro — mais un AUTRE album : l'album borne le repli.
    poser(temoin_id, PISTES[0], 1, "flac", 44100, 16);
    poser(temoin_id, "Kashmir", 2, "flac", 44100, 16);

    ecrites
}

async fn corps_de(app: &Router, chemin: &str) -> Value {
    let reponse = app
        .clone()
        .oneshot(Request::get(chemin).body(Body::empty()).unwrap())
        .await
        .unwrap_or_else(|e| panic!("{chemin} : routeur en échec : {e}"));
    let statut = reponse.status();
    let octets = axum::body::to_bytes(reponse.into_body(), usize::MAX)
        .await
        .unwrap_or_else(|e| panic!("{chemin} : corps illisible : {e}"));
    assert_eq!(
        statut,
        StatusCode::OK,
        "{chemin} : statut {statut}, corps {}",
        String::from_utf8_lossy(&octets)
    );
    serde_json::from_slice(&octets).unwrap_or_else(|e| panic!("{chemin} : JSON illisible : {e}"))
}

/// `(id, titre, numéro, format)` des lignes rendues pour un album donné.
fn lignes_de_l_album(corps: &Value, album_titre: &str) -> Vec<(i64, String, i64, String)> {
    corps["items"]
        .as_array()
        .unwrap_or_else(|| panic!("`items` doit être un tableau — {corps}"))
        .iter()
        .filter(|t| t["album_title"].as_str() == Some(album_titre))
        .map(|t| {
            (
                t["id"].as_i64().unwrap_or_default(),
                t["title"].as_str().unwrap_or_default().to_string(),
                t["track_number"].as_i64().unwrap_or_default(),
                t["format"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect()
}

/// Ce que la route doit dire, sur l'un ou l'autre de ses deux chemins.
fn exiger_une_vue_propre(corps: &Value, chemin: &str, lignes_ecrites: usize) {
    let rendues = lignes_de_l_album(corps, ALBUM);

    // 🔴 Des IDENTIFIANTS DISTINCTS, pas des lignes : le compteur de l'issue
    // comptait les lignes, et c'est précisément ce qui ne prouvait rien.
    let ids: HashSet<i64> = rendues.iter().map(|(id, ..)| *id).collect();
    assert_eq!(
        ids.len(),
        VISIBLES_DANS_L_ALBUM,
        "{chemin} : « {ALBUM} » doit rendre {VISIBLES_DANS_L_ALBUM} pistes \
         DISTINCTES, il en rend {} — lignes : {rendues:?}",
        ids.len()
    );
    assert_eq!(
        rendues.len(),
        ids.len(),
        "{chemin} : la réponse contient deux fois le même identifiant — \
         lignes : {rendues:?}"
    );

    // Chaque (numéro, titre) une seule fois : le symptôme de JeromeQ, mot pour
    // mot — « 1 Black Dog / 1 Black Dog ».
    let mut presentations: Vec<(i64, String)> = rendues
        .iter()
        .map(|(_, titre, numero, _)| (*numero, titre.to_lowercase()))
        .collect();
    presentations.sort();
    let uniques: HashSet<_> = presentations.iter().cloned().collect();
    assert_eq!(
        presentations.len(),
        uniques.len(),
        "{chemin} : un même (numéro, titre) paraît deux fois — {presentations:?}"
    );

    // #1362 : c'est la copie de MEILLEURE qualité qui survit. Un repli qui
    // garderait « la première venue » laisserait l'ordre SQL choisir entre le
    // FLAC et l'AAC.
    for i in DOUBLEES {
        let numero = i as i64 + 1;
        let survivante = rendues
            .iter()
            .find(|(_, titre, n, _)| *n == numero && titre == PISTES[i])
            .unwrap_or_else(|| {
                panic!(
                    "{chemin} : la piste {numero} « {} » a disparu — {rendues:?}",
                    PISTES[i]
                )
            });
        assert_eq!(
            survivante.3, "flac",
            "{chemin} : piste {numero} — c'est la copie de meilleure qualité \
             qui doit survivre, pas l'AAC"
        );
    }

    // L'homonyme sous un autre numéro est intact : le repli ne mange pas deux
    // morceaux distincts.
    assert!(
        rendues
            .iter()
            .any(|(_, titre, n, _)| titre == HOMONYME_DANS_L_ALBUM && *n == HOMONYME_NUMERO),
        "{chemin} : « {HOMONYME_DANS_L_ALBUM} » n° {HOMONYME_NUMERO} doit rester \
         rendu — deux morceaux distincts ne se replient pas — {rendues:?}"
    );

    // Le banc contenait bien plus de lignes que la vue n'en rend : sans quoi
    // l'épreuve verdirait à vide.
    assert!(
        lignes_ecrites > VISIBLES_DANS_L_ALBUM,
        "banc dégénéré : {lignes_ecrites} lignes écrites pour \
         {VISIBLES_DANS_L_ALBUM} attendues"
    );
}

/// Le chemin NON facetté — celui qu'Oxygen emprunte à l'ouverture.
#[tokio::test(flavor = "multi_thread")]
async fn i4101_la_vue_pistes_ne_liste_chaque_piste_qu_une_fois() {
    exige_un_banc_qui_departage();

    let etat = AppState::new(":memory:", 0, Default::default()).expect("état serveur isolé");
    let lignes = poser_le_banc(&etat);
    let app = tune_server::routes::router(etat);

    let chemin = "/api/v1/library/tracks?limit=200";
    let corps = corps_de(&app, chemin).await;
    exiger_une_vue_propre(&corps, chemin, lignes);

    // L'album témoin n'a rien perdu — et sa piste homonyme de « Black Dog »,
    // même numéro, est bien là : l'album borne le repli.
    let temoin = lignes_de_l_album(&corps, TEMOIN_ALBUM);
    assert_eq!(
        temoin.len(),
        2,
        "« {TEMOIN_ALBUM} » doit rendre ses 2 pistes — {temoin:?}"
    );
    assert!(
        temoin
            .iter()
            .any(|(_, titre, n, _)| titre == PISTES[0] && *n == 1),
        "« {} » n° 1 d'un AUTRE album doit rester rendu — {temoin:?}",
        PISTES[0]
    );

    // Le `total` de la pagination suit la MÊME exclusion que la liste, sinon
    // la fenêtre suivante part d'un mauvais décalage et saute des pistes.
    let rendues = corps["items"].as_array().expect("items").len();
    assert_eq!(
        corps["total"].as_i64(),
        Some(rendues as i64),
        "le total doit compter ce que la liste rend — corps : {corps}"
    );
}

/// Le chemin FACETTÉ — celui qu'Oxygen emprunte dès qu'une facette est posée.
/// Deux chemins, deux prédicats : un correctif posé sur un seul laisserait
/// l'autre doubler.
#[tokio::test(flavor = "multi_thread")]
async fn i4101_le_chemin_facette_replie_aussi() {
    exige_un_banc_qui_departage();

    let etat = AppState::new(":memory:", 0, Default::default()).expect("état serveur isolé");
    let lignes = poser_le_banc(&etat);
    let app = tune_server::routes::router(etat);

    // `year` est une facette de `tracks` : elle laisse passer les DEUX copies,
    // donc elle ne masque pas le défaut qu'on cherche à garder.
    let chemin = "/api/v1/library/tracks?year=1971&limit=200";
    let corps = corps_de(&app, chemin).await;
    exiger_une_vue_propre(&corps, chemin, lignes);

    let rendues = corps["items"].as_array().expect("items").len();
    assert_eq!(
        corps["total"].as_i64(),
        Some(rendues as i64),
        "le total du chemin facetté doit compter ce que la liste rend — {corps}"
    );
}
