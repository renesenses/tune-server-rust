//! Le genre et le pays d'une station voyagent en CLÉ et en CODE (#fuites-fr).
//!
//! Le semis du catalogue écrit ces deux colonnes en français —
//! `tune-core/src/db/migrations.rs:455` et suivantes (« Éclectique »,
//! « Chanson française », « Généraliste »), `tune-core/migrations/radios/
//! annuaire_mozaiklabs_2026_08_30.sql:138` et suivantes (« Royaume-Uni »,
//! « États-Unis », « Pays-Bas », « Suisse », « Japon »). `GET /radios` les
//! rendait telles quelles : un testeur roumain voyait des pastilles de genre
//! françaises au milieu d'une interface roumaine (v0.9.161).
//!
//! Les essais tiennent quatre propriétés, dans cet ordre :
//!
//! 1. la station porte désormais `country_code` (ISO 3166-1 alpha-2),
//!    `genre_key` (clé stable) et `genre_label` (genre traduit) ;
//! 2. **la contre-épreuve de la traduction** : le même appel dans trois
//!    langues rend TROIS libellés différents. Sans elle, un `genre_label`
//!    recopié depuis `genre` passerait l'essai 1 sans rien traduire ;
//! 3. la rétro-compatibilité : `genre` et `country` gardent mot pour mot leur
//!    valeur d'avant — `docs/contrat-web.json` les cite toujours ;
//! 4. une station ajoutée à la main, au genre libre, ne gagne aucune clé : on
//!    ne devine pas, et le client retombe sur `genre`.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;
use tune_core::db::radio_repo::{RadioRepo, RadioStation};

fn app_et_etat() -> (axum::Router, tune_server::state::AppState) {
    let state = tune_server::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    let router = tune_server::routes::router(state.clone());
    (router, state)
}

fn station(nom: &str, genre: &str, pays: &str) -> RadioStation {
    RadioStation {
        id: None,
        name: nom.into(),
        url: format!(
            "https://exemple.invalid/{}",
            nom.to_lowercase().replace(' ', "-")
        ),
        homepage: None,
        logo_url: None,
        country: Some(pays.into()),
        language: None,
        genre: Some(genre.into()),
        codec: None,
        bitrate: None,
        is_favorite: false,
        last_played: None,
        play_count: 0,
    }
}

async fn lister(app: &axum::Router, accept_language: &str) -> Value {
    let reponse = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/radios")
                .header("Accept-Language", accept_language)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(reponse.status(), StatusCode::OK);
    let octets = axum::body::to_bytes(reponse.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&octets).unwrap()
}

/// Retrouver une station par son nom dans le corps rendu — plutôt que par un
/// indice de tableau, que l'ordre du catalogue livré ferait dériver.
fn par_nom<'a>(corps: &'a Value, nom: &str) -> &'a Value {
    corps
        .as_array()
        .expect("le corps de GET /radios est un tableau")
        .iter()
        .find(|s| s["name"] == nom)
        .unwrap_or_else(|| panic!("station « {nom} » absente du corps rendu"))
}

#[tokio::test]
async fn la_station_porte_son_code_pays_et_sa_cle_de_genre() {
    let (app, etat) = app_et_etat();
    let repo = RadioRepo::with_backend(etat.backend.clone());
    repo.create(&station("Témoin Linn", "Classique", "Royaume-Uni"))
        .unwrap();

    let corps = lister(&app, "ro").await;
    let s = par_nom(&corps, "Témoin Linn");

    assert_eq!(s["country_code"], "GB", "code ISO du pays");
    assert_eq!(s["genre_key"], "radio.genre.classical", "clé du genre");
}

#[tokio::test]
async fn le_libelle_du_genre_suit_la_langue_de_la_requete() {
    // LA contre-épreuve : trois langues, trois libellés. Un `genre_label`
    // qui recopierait `genre` rendrait « Classique » partout et échouerait
    // ici, alors qu'il passerait l'essai précédent.
    let (app, etat) = app_et_etat();
    let repo = RadioRepo::with_backend(etat.backend.clone());
    repo.create(&station("Témoin Linn", "Classique", "Royaume-Uni"))
        .unwrap();

    for (entete, attendu) in [
        ("ro", "Clasică"),
        ("fr-FR,fr;q=0.9", "Classique"),
        ("de-DE,de;q=0.9", "Klassik"),
        ("ja", "クラシック"),
    ] {
        let corps = lister(&app, entete).await;
        assert_eq!(
            par_nom(&corps, "Témoin Linn")["genre_label"],
            attendu,
            "genre_label en « {entete} »"
        );
    }

    // Et les trois libellés diffèrent réellement deux à deux : sans cette
    // ligne, une table où « Clasică » vaudrait « Classique » passerait.
    let ro = lister(&app, "ro").await;
    let de = lister(&app, "de").await;
    assert_ne!(
        par_nom(&ro, "Témoin Linn")["genre_label"],
        par_nom(&de, "Témoin Linn")["genre_label"],
        "deux langues ne peuvent pas rendre le même libellé"
    );
}

#[tokio::test]
async fn les_champs_publies_gardent_leur_valeur_dorigine() {
    // `docs/contrat-web.json` cite `genre` et `country` parmi les champs
    // optionnels de `GET /radios` : on AJOUTE, on ne remplace pas.
    let (app, etat) = app_et_etat();
    let repo = RadioRepo::with_backend(etat.backend.clone());
    repo.create(&station("Témoin Linn", "Classique", "Royaume-Uni"))
        .unwrap();

    let corps = lister(&app, "ro").await;
    let s = par_nom(&corps, "Témoin Linn");
    assert_eq!(s["genre"], "Classique", "`genre` inchangé");
    assert_eq!(s["country"], "Royaume-Uni", "`country` inchangé");
    assert_eq!(s["name"], "Témoin Linn");
    assert!(s["stream_url"].is_string(), "`stream_url` toujours là");
}

#[tokio::test]
async fn tous_les_pays_du_catalogue_livre_ont_un_code() {
    let (app, etat) = app_et_etat();
    let repo = RadioRepo::with_backend(etat.backend.clone());
    for (nom, pays, code) in [
        ("Témoin GB", "Royaume-Uni", "GB"),
        ("Témoin US", "États-Unis", "US"),
        ("Témoin NL", "Pays-Bas", "NL"),
        ("Témoin CH", "Suisse", "CH"),
        ("Témoin JP", "Japon", "JP"),
        ("Témoin FR", "France", "FR"),
        ("Témoin CA", "Canada", "CA"),
        ("Témoin BE", "Belgique", "BE"),
    ] {
        repo.create(&station(nom, "Jazz", pays)).unwrap();
        let corps = lister(&app, "ro").await;
        assert_eq!(par_nom(&corps, nom)["country_code"], code, "pays {pays}");
    }
}

#[tokio::test]
async fn une_station_au_genre_libre_ne_gagne_aucune_cle() {
    let (app, etat) = app_et_etat();
    let repo = RadioRepo::with_backend(etat.backend.clone());
    repo.create(&station("Ma radio", "Fanfare de quartier", "Sylvanie"))
        .unwrap();

    let corps = lister(&app, "ro").await;
    let s = par_nom(&corps, "Ma radio");
    assert!(s.get("genre_key").is_none(), "pas de clé devinée");
    assert!(s.get("genre_label").is_none(), "pas de libellé deviné");
    assert!(s.get("country_code").is_none(), "pas de code deviné");
    assert_eq!(s["genre"], "Fanfare de quartier", "le genre libre survit");
}

#[tokio::test]
async fn la_recherche_rend_les_memes_reperes_que_la_liste() {
    // Un point de sortie oublié est le défaut d'origine : `/radios/search`
    // rend des stations, donc il rend les mêmes repères.
    let (app, etat) = app_et_etat();
    let repo = RadioRepo::with_backend(etat.backend.clone());
    repo.create(&station("Témoin Linn", "Classique", "Royaume-Uni"))
        .unwrap();

    let reponse = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/radios/search?q=Linn")
                .header("Accept-Language", "ro")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(reponse.status(), StatusCode::OK);
    let octets = axum::body::to_bytes(reponse.into_body(), usize::MAX)
        .await
        .unwrap();
    let corps: Value = serde_json::from_slice(&octets).unwrap();
    let s = par_nom(&corps["items"], "Témoin Linn");
    assert_eq!(s["country_code"], "GB");
    assert_eq!(s["genre_key"], "radio.genre.classical");
    assert_eq!(s["genre_label"], "Clasică");
}
