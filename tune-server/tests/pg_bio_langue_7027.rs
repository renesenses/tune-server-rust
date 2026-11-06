//! #7027 sur une VRAIE base PostgreSQL : une bio rangée sans langue n'est
//! jamais servie comme de l'anglais.
//!
//! La cause est une valeur en base : `artists.bio_lang` vide (`''`, écrit par
//! la bio éditoriale d'un service de streaming) ou NULL (fonds communautaire,
//! lignes anciennes), avec une source renseignée. Ce banc rejoue les deux
//! valeurs sur PostgreSQL, par le vrai routeur, avec `Accept-Language: en` :
//!
//! - une version anglaise en cache : elle est servie ;
//! - aucune version anglaise, site injoignable : `bio` nul, `repli` dit.
//!
//! Le texte français n'apparaît jamais dans la réponse. Même doctrine que
//! `pg_routes_serveur.rs` : variable ABSENTE ⇒ saut annoncé ; variable POSÉE
//! mais injoignable ⇒ le test TOMBE.

#![cfg(feature = "postgres")]

use axum::body::Body;
use axum::http::Request;
use serde_json::{Value, json};
use tower::ServiceExt;

use tune_core::db::artist_repo::ArtistRepo;
use tune_core::db::backend::ToSqlValue;
use tune_core::db::models::Artist;
use tune_core::db::settings_repo::SettingsRepo;
use tune_server::state::AppState;

/// Préfixe propre à cette épreuve : les tables sont partagées avec les autres
/// bancs PostgreSQL, on ne vide rien et on ne touche qu'à nos lignes.
const MARQUE: &str = "bio-langue-7027";
const BIO_FR: &str = "Arcade Fire est un groupe de rock indépendant canadien.";
const BIO_EN: &str = "Arcade Fire is a Canadian indie rock band.";

fn url_pg() -> Option<String> {
    std::env::var("TUNE_TEST_PG_URL").ok()
}

/// État PostgreSQL dont le site de biographies est injoignable (proxy fermé) :
/// aucun appel réseau réel.
fn etat_postgres(url: &str) -> AppState {
    let config = tune_server::config::TuneConfig {
        database_url: Some(url.to_string()),
        ..Default::default()
    };
    let mut state = AppState::new("", 0, config).expect("AppState sur PostgreSQL");
    let ferme = std::net::TcpListener::bind("127.0.0.1:0").expect("port libre");
    let adresse = ferme.local_addr().expect("adresse");
    drop(ferme);
    state.http_client = tune_core::http::client::builder()
        .proxy(reqwest::Proxy::all(format!("http://{adresse}")).expect("proxy valide"))
        .build()
        .expect("client HTTP du témoin");
    state
}

fn cle_cache(nom: &str) -> String {
    format!("cache:artistbio:{nom}:en")
}

fn menage(state: &AppState, noms: &[&str]) {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    for nom in noms {
        let _ = settings.delete(&cle_cache(nom));
    }
    state
        .backend
        .execute(
            "DELETE FROM artists WHERE name LIKE $1",
            &[&format!("{MARQUE}%") as &dyn ToSqlValue],
        )
        .expect("ménage des artistes");
}

/// Un artiste à bio française, source `qobuz`, `bio_lang` vide ou NULL.
fn artiste_bio_sans_langue(state: &AppState, nom: &str, langue_nulle: bool) -> i64 {
    let repo = ArtistRepo::with_backend(state.backend.clone());
    let id = repo
        .create(&Artist::new(nom.to_string()))
        .expect("création de l'artiste");
    repo.update_bio_full(id, BIO_FR, "qobuz", None, "", "")
        .expect("bio de service sans langue");
    if langue_nulle {
        state
            .backend
            .execute(
                "UPDATE artists SET bio_lang = NULL WHERE id = $1",
                &[&id as &dyn ToSqlValue],
            )
            .expect("bio_lang à NULL");
    }
    id
}

async fn bio_en(state: &AppState, id: i64) -> Value {
    let app = tune_server::routes::router(state.clone());
    let resp = app
        .oneshot(
            Request::get(format!("/api/v1/library/artists/{id}/bio"))
                .header(axum::http::header::ACCEPT_LANGUAGE, "en")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(resp.status().is_success(), "statut {}", resp.status());
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).expect("réponse JSON")
}

#[tokio::test(flavor = "multi_thread")]
async fn pg_7027_une_bio_sans_langue_n_est_jamais_servie_comme_de_l_anglais() {
    let Some(url) = url_pg() else {
        eprintln!("TUNE_TEST_PG_URL absente — épreuve PostgreSQL sautée");
        return;
    };
    let state = etat_postgres(&url);
    let vide = format!("{MARQUE}-vide");
    let nulle = format!("{MARQUE}-nulle");
    menage(&state, &[&vide, &nulle]);

    for (nom, langue_nulle) in [(&vide, false), (&nulle, true)] {
        let id = artiste_bio_sans_langue(&state, nom, langue_nulle);

        // Version anglaise en cache : elle est servie.
        SettingsRepo::with_backend(state.backend.clone())
            .set(
                &cle_cache(nom),
                &json!({ "artist": nom, "bio": BIO_EN, "source": "community" }).to_string(),
            )
            .expect("entrée de cache anglaise");
        let body = bio_en(&state, id).await;
        assert_eq!(
            body["bio"].as_str(),
            Some(BIO_EN),
            "{nom} : une bio française sans langue a été servie à une \
             interface anglaise (#7027) : {body}"
        );

        // Aucune version anglaise, site injoignable : rien, et le repli dit.
        SettingsRepo::with_backend(state.backend.clone())
            .delete(&cle_cache(nom))
            .expect("retrait du cache");
        let body = bio_en(&state, id).await;
        assert!(
            !body.to_string().contains(BIO_FR),
            "{nom} : du français rendu à une interface anglaise : {body}"
        );
        assert!(body["bio"].is_null(), "{nom} : {body}");
        assert_eq!(
            body["repli"]["raison"], "langue_indisponible",
            "{nom} : {body}"
        );
        assert!(
            body["repli"]["langue_disponible"].is_null(),
            "{nom} : {body}"
        );
    }

    menage(&state, &[&vide, &nulle]);
}
