//! #3789 — `GET /library/search` servait des pistes NUES.
//!
//! Les mêmes pistes, listées par `/library/tracks` ou par la fiche d'un album,
//! portent depuis #1388 et #3518 leur `dynamic_range`, leur `play_count` et
//! leur `last_played_at`. La recherche, elle, sérialisait ses pistes à la main
//! (`t.to_json()` dans une boucle) et ne passait par aucun des deux
//! enrichissements — alors que l'écran de recherche du client rend ses titres
//! dans la MÊME table à colonnes que la bibliothèque (`ListePistesV2`,
//! `SearchV2.svelte`). Trois colonnes se vidaient donc selon l'écran d'où on
//! venait, ce qu'un testeur lit « mes fichiers ne sont pas tagués ».
//!
//! ## Pourquoi ces essais passent par la ROUTE MONTÉE
//!
//! Un essai qui appellerait `joindre_dr_par_piste` en direct resterait VERT
//! alors même que `search` ne l'appelle pas — le défaut « écrit mais pas
//! branché ». On monte donc `tune_server::routes::router(state)` et on lit la
//! charge JSON que la route rend vraiment.
//!
//! ## Le contrat, qui n'est PAS le même pour les trois clés
//!
//! - `dynamic_range` est **ABSENTE** quand la piste n'a pas le tag — jamais
//!   `null`, jamais `0`. DR0 est la mesure d'un master saturé, pas une
//!   absence : une piste tagguée `0` doit donc bien porter la clé à `"0"`.
//! - `play_count` vaut `0` et `last_played_at` vaut `null` pour une piste
//!   jamais jouée : ici le zéro est une information, et les deux clés doivent
//!   TOUJOURS exister.
//!
//! ## La cause que ces essais gardent
//!
//! `listen_history.track_id` est toujours NULL en production (le seul site qui
//! écrit l'historique passe `track_id: None`). Les écoutes posées ici le sont
//! donc sans `track_id`, comme #3518 l'exige.

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;

use tune_core::db::models::{Album, Artist, Track};
use tune_server::state::AppState;

fn etat() -> AppState {
    AppState::new(":memory:", 0, Default::default()).expect("AppState sur SQLite")
}

/// Le mot cherché : le nom de l'ARTISTE. Le prédicat de la recherche porte un
/// `LIKE` sur `ar.name`, si bien que les quatre pistes remontent quel que soit
/// l'état de l'index FTS de ce banc de test.
const MOT: &str = "Miles Davis";

/// Le mot qui ne se trouve QUE dans `track_metadata` : ni dans un titre, ni
/// dans un nom d'artiste, ni dans un genre, ni dans `tracks.composer`, ni dans
/// une année d'album. Il n'atteint donc la piste 4 que par la passe
/// « métadonnées étendues » de la route — l'autre moitié de la page.
const MOT_META: &str = "Chostakovitch";

/// Quatre pistes du même artiste, chacune sur un cas du contrat :
///
/// | piste | `dr_track` | écoutes | ce qu'elle éprouve                    |
/// |-------|------------|---------|---------------------------------------|
/// | 1     | `13`       | 2       | le cas nominal, les trois clés         |
/// | 2     | *(aucun)*  | 0       | l'ABSENCE de `dynamic_range`           |
/// | 3     | `0`        | 0       | DR0 est une MESURE, elle sort          |
/// | 4     | `9`        | 1       | trouvée par ses métadonnées seulement  |
fn bibliotheque(state: &AppState) {
    let backend = &state.backend;
    tune_core::db::artist_repo::ArtistRepo::with_backend(backend.clone())
        .create(&Artist::new(MOT.to_string()))
        .expect("artiste");
    tune_core::db::album_repo::AlbumRepo::with_backend(backend.clone())
        .create(&Album::new("Kind of Blue".to_string()))
        .expect("album");

    let repo = tune_core::db::track_repo::TrackRepo::with_backend(backend.clone());
    for (n, titre) in [
        (1, "So What"),
        (2, "Blue in Green"),
        (3, "Flamenco Sketches"),
        (4, "Freddie Freeloader"),
    ] {
        let mut t = Track::new(titre.to_string());
        t.artist_id = Some(1);
        t.album_id = Some(1);
        t.track_number = n;
        t.channels = 2;
        t.file_path = Some(format!("/musique/{n}.flac"));
        repo.create(&t).expect("piste");
    }

    // Le tag `DYNAMIC RANGE` tel que le scan le range (#1806). La piste 2 n'en
    // a AUCUN : c'est elle qui éprouve l'absence.
    let meta = tune_core::db::track_metadata_repo::TrackMetadataRepo::with_backend(backend.clone());
    meta.set(1, "dr_track", "13").expect("dr 1");
    meta.set(3, "dr_track", "0").expect("dr 3");
    meta.set(4, "dr_track", "9").expect("dr 4");
    // Une valeur de métadonnée cherchable — `composer` est dans la liste que
    // `search_by_value` interroge. C'est le seul chemin vers la piste 4.
    meta.set(4, "composer", MOT_META).expect("composer 4");
}

/// Une écoute, **sans `track_id`** — exactement ce que l'orchestrateur écrit.
fn ecoute(state: &AppState, titre: &str, quand: &str) {
    state
        .backend
        .execute(
            &format!(
                "INSERT INTO listen_history \
                   (track_id, title, artist_name, album_title, source, listened_at) \
                 VALUES (NULL, '{titre}', '{MOT}', 'Kind of Blue', 'local', '{quand}')"
            ),
            &[],
        )
        .expect("ecoute");
}

async fn json(routeur: Router, chemin: &str) -> Value {
    let r = routeur
        .oneshot(
            Request::builder()
                .uri(chemin)
                .body(Body::empty())
                .expect("requete"),
        )
        .await
        .expect("reponse");
    assert_eq!(r.status(), StatusCode::OK, "chemin: {chemin}");
    let octets = axum::body::to_bytes(r.into_body(), 8 << 20)
        .await
        .expect("corps");
    serde_json::from_slice(&octets).expect("json")
}

/// Les pistes de `GET /library/search?q=…`, indexées par titre.
async fn pistes_de_la_recherche(state: AppState, mot: &str) -> Vec<Value> {
    let corps = json(
        tune_server::routes::router(state),
        &format!("/api/v1/library/search?q={}", urlencoding(mot)),
    )
    .await;
    corps["tracks"]
        .as_array()
        .expect("la recherche rend un tableau de pistes")
        .clone()
}

/// Assez d'encodage pour une espace : ce banc n'envoie que des mots latins.
fn urlencoding(s: &str) -> String {
    s.replace(' ', "%20")
}

fn par_titre<'a>(pistes: &'a [Value], titre: &str) -> &'a Value {
    pistes
        .iter()
        .find(|p| p["title"] == titre)
        .unwrap_or_else(|| panic!("la piste « {titre} » manque a la reponse"))
}

/// Le cas nominal : la recherche porte les trois colonnes.
///
/// Contre-épreuve : rendre à `search` sa boucle `t.to_json()` — c'est-à-dire
/// retirer l'appel à `joindre_dr_par_piste` — et cet essai passe au rouge sur
/// la toute première assertion.
#[tokio::test]
async fn la_recherche_porte_le_dr_les_ecoutes_et_la_derniere_ecoute() {
    let state = etat();
    bibliotheque(&state);
    for quand in ["2026-09-01T10:00:00Z", "2026-09-05T21:30:00Z"] {
        ecoute(&state, "So What", quand);
    }

    let pistes = pistes_de_la_recherche(state, MOT).await;
    let jouee = par_titre(&pistes, "So What");

    assert_eq!(
        jouee["dynamic_range"], "13",
        "le tag DYNAMIC RANGE doit ressortir sur la recherche comme ailleurs"
    );
    assert_eq!(
        jouee["play_count"], 2,
        "deux ecoutes doivent se compter, malgre un track_id NULL en base"
    );
    assert_eq!(
        jouee["last_played_at"], "2026-09-05T21:30:00Z",
        "la DERNIERE ecoute, pas la premiere"
    );
}

/// Le piège central : une piste SANS tag DR n'a pas la clé du tout.
///
/// Pas `null`, pas `0`. Une cellule vide n'est pas « DR 0 » : DR0 est la
/// mesure d'un master saturé, et un client qui lirait `0` afficherait la pire
/// note possible sur une piste qu'on n'a simplement pas mesurée.
#[tokio::test]
async fn une_piste_sans_tag_dr_ne_porte_pas_la_cle() {
    let state = etat();
    bibliotheque(&state);

    let pistes = pistes_de_la_recherche(state, MOT).await;
    let nue = par_titre(&pistes, "Blue in Green");
    let obj = nue.as_object().expect("un objet");

    assert!(
        !obj.contains_key("dynamic_range"),
        "sans tag, la cle doit etre ABSENTE — elle vaut {:?}",
        obj.get("dynamic_range")
    );
    assert!(
        nue["dynamic_range"].is_null(),
        "corollaire : l'acces rend `null` parce que la cle manque, \
         jamais parce qu'on l'aurait posee a null"
    );
}

/// DR0 est une MESURE : la clé sort, à `"0"`.
///
/// C'est l'essai qui interdit la « correction » évidente — filtrer les valeurs
/// nulles ou vides à la sortie — qui ferait disparaître la note des masters
/// les plus compressés, précisément ceux que l'indicateur sert à repérer.
#[tokio::test]
async fn dr0_sort_parce_que_c_est_une_mesure_et_non_une_absence() {
    let state = etat();
    bibliotheque(&state);

    let pistes = pistes_de_la_recherche(state, MOT).await;
    let saturee = par_titre(&pistes, "Flamenco Sketches");

    assert_eq!(
        saturee["dynamic_range"], "0",
        "DR0 est la mesure d'un master sature : elle doit sortir"
    );
}

/// Les deux clés d'écoute existent TOUJOURS, même sur une bibliothèque sans
/// une seule écoute — c'est ce qui permet au client d'afficher « jamais joué »
/// au lieu d'une case vide dont il ne saurait pas si elle vient de la route.
#[tokio::test]
async fn les_deux_cles_d_ecoute_existent_meme_sans_aucune_ecoute() {
    let state = etat();
    bibliotheque(&state);

    let pistes = pistes_de_la_recherche(state, MOT).await;
    assert_eq!(pistes.len(), 4, "les quatre pistes doivent remonter");
    for piste in &pistes {
        let obj = piste.as_object().expect("un objet");
        assert_eq!(
            piste["play_count"], 0,
            "ici un zero est une information, pas une absence"
        );
        assert!(
            obj.contains_key("last_played_at"),
            "la cle doit etre presente, a null"
        );
        assert!(piste["last_played_at"].is_null());
    }
}

/// L'AUTRE moitié de la page.
///
/// La route rend deux ensembles concaténés : les pistes trouvées par le
/// prédicat de recherche, et celles trouvées par leurs métadonnées étendues.
/// L'enrichissement est branché UNE fois, sur la liste réunie — un branchement
/// posé sur la seule première moitié laisserait ces pistes-là nues, et le
/// tableau se viderait par endroits selon le mot cherché.
///
/// Cet essai garde aussi le fait que l'enrichissement n'a pas mangé
/// `matched_metadata`, l'annotation propre à cette route.
#[tokio::test]
async fn une_piste_trouvee_par_ses_metadonnees_est_enrichie_aussi() {
    let state = etat();
    bibliotheque(&state);
    ecoute(&state, "Freddie Freeloader", "2026-09-07T08:00:00Z");

    let pistes = pistes_de_la_recherche(state, MOT_META).await;
    assert_eq!(
        pistes.len(),
        1,
        "ce mot ne se trouve que dans les metadonnees d'une seule piste"
    );
    let piste = par_titre(&pistes, "Freddie Freeloader");

    assert_eq!(piste["dynamic_range"], "9");
    assert_eq!(piste["play_count"], 1);
    assert_eq!(piste["last_played_at"], "2026-09-07T08:00:00Z");
    assert_eq!(
        piste["matched_metadata"]["composer"], MOT_META,
        "l'annotation propre a la recherche doit survivre a l'enrichissement"
    );
}

/// Non-régression : la pastille de canaux ne disparaît pas.
///
/// `/library/search` sérialisait par `Track::to_json`, qui ajoute le champ
/// CALCULÉ `channel_badge` ; le seam passait par `serde_json::to_value`, qui ne
/// l'ajoute pas. Brancher la recherche sur le seam sans corriger cela aurait
/// RETIRÉ la pastille de la recherche pour y poser le DR — une régression pour
/// fermer un trou. Le seam sérialise donc désormais par `to_json` lui aussi.
#[tokio::test]
async fn la_pastille_de_canaux_ne_disparait_pas_de_la_recherche() {
    let state = etat();
    bibliotheque(&state);

    let pistes = pistes_de_la_recherche(state, MOT).await;
    for piste in &pistes {
        assert!(
            piste
                .as_object()
                .expect("un objet")
                .contains_key("channel_badge"),
            "le champ calcule que la recherche servait deja doit rester"
        );
    }
}

/// « Que le tableau ait les mêmes colonnes partout » : la même piste, vue par
/// la recherche et par la table des titres, porte les trois clés dans les deux
/// cas et avec la MÊME valeur.
///
/// C'est l'affirmation que l'issue formule, et la seule qui se vérifie en
/// comparant deux routes plutôt qu'en relisant une seule.
#[tokio::test]
async fn la_recherche_et_la_table_des_titres_disent_la_meme_chose() {
    let state = etat();
    bibliotheque(&state);
    ecoute(&state, "So What", "2026-09-05T21:30:00Z");
    let routeur = tune_server::routes::router(state);

    let par_la_recherche = json(routeur.clone(), "/api/v1/library/search?q=Miles%20Davis").await;
    let par_la_table = json(routeur, "/api/v1/library/tracks?limit=10").await;

    let a = par_la_recherche["tracks"]
        .as_array()
        .expect("des pistes")
        .clone();
    let b = par_la_table["items"]
        .as_array()
        .expect("des pistes")
        .clone();
    let a = par_titre(&a, "So What");
    let b = par_titre(&b, "So What");

    for cle in ["dynamic_range", "play_count", "last_played_at"] {
        assert_eq!(
            a[cle], b[cle],
            "« {cle} » doit valoir la meme chose sur les deux routes"
        );
        assert!(
            !a[cle].is_null(),
            "« {cle} » ne doit pas etre absente des deux cotes a la fois — \
             une comparaison de deux vides passerait sans rien garder"
        );
    }
}

/// Une recherche sans résultat reste une page vide, et sans erreur : les deux
/// enrichissements court-circuitent sur une liste d'identifiants vide, donc
/// zéro requête.
#[tokio::test]
async fn une_recherche_sans_resultat_reste_une_page_vide() {
    let state = etat();
    bibliotheque(&state);

    let pistes = pistes_de_la_recherche(state, "Stockhausen").await;
    assert!(pistes.is_empty());
}
