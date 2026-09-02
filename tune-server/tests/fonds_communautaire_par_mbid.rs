//! #2258 — « les artistes sans MBID ne sont jamais téléversés ».
//!
//! ## Le mécanisme, pas le mot
//!
//! Le fonds communautaire de biographies de mozaiklabs.fr est indexé par
//! identifiant MusicBrainz : `cloud::bio_sync::download_artist_bios`
//! l'interroge par `"{artist_url}?musicbrainz_ids={}"`. Les deux requêtes qui
//! l'alimentent l'exigent donc non vide —
//! `ArtistRepo::artists_with_bio_and_mbid` pour l'envoi,
//! `artists_without_bio_with_mbid` pour les candidats au téléchargement.
//!
//! Un artiste sans MBID est par conséquent écarté **des deux côtés** : sa
//! biographie ne part jamais, et aucune ne lui revient. Ce filtre n'est pas un
//! défaut à retirer — le lever enverrait des clés vides à une API qui s'en
//! sert d'index, et une biographie mal rattachée irait chez tous les
//! utilisateurs. Le défaut, c'est que l'exclusion soit **muette**.
//!
//! ## Ce que ces tests gardent
//!
//! Que le compte de cette exclusion atteigne bien une RÉPONSE HTTP — le
//! panneau d'enrichissement et la réponse du bouton « récupérer les
//! biographies » — et pas seulement une ligne de journal. Personne ne lit les
//! journaux d'un serveur audio.
//!
//! Les tests montent un vrai `AppState` et un vrai routeur, sèment de vrais
//! artistes en base, et interrogent les routes de production. Aucune requête
//! SQL n'est retranscrite ici.
//!
//! ## Hermétique : aucun appel réseau
//!
//! `GET /system/enrichment/status` est en lecture seule. Pour la route POST,
//! le réglage `lastfm_api_key` est posé VIDE — un réglage présent l'emporte
//! sur l'environnement (`bio_batch::cle_lastfm_avec_reglage`) — et les seuls
//! candidats semés sont dépourvus de MBID : la passe artistes les compte en
//! échec sans émettre la moindre requête, et la passe albums sort par
//! `batch_album_bio_skip_all_have_bios` sur une base sans album.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;
use tune_core::db::artist_repo::ArtistRepo;
use tune_core::db::models::Artist;
use tune_core::db::settings_repo::SettingsRepo;

/// Une bibliothèque comme celle de Bilou : deux artistes identifiés, sept qui
/// ne le sont pas.
///
/// - `Pink Floyd` — MBID **et** bio : le seul que la passe d'envoi téléverse.
/// - `Miles Davis` — MBID, pas de bio : le seul candidat au téléchargement.
/// - trois bios sans MBID : elles existent et ne partiront jamais.
/// - quatre sans bio ni MBID : rien ne pourra jamais les servir.
fn app_bibliotheque_de_bilou() -> axum::Router {
    let state = tune_server::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    let repo = ArtistRepo::with_backend(state.backend.clone());

    let mut identifie_avec_bio = Artist::new("Pink Floyd".into());
    identifie_avec_bio.musicbrainz_id = Some("83d91898-7763-47d7-b03b-b92132375c47".into());
    identifie_avec_bio.bio = Some("Groupe de rock anglais.".into());
    repo.create(&identifie_avec_bio).expect("artiste identifié");

    let mut identifie_sans_bio = Artist::new("Miles Davis".into());
    identifie_sans_bio.musicbrainz_id = Some("561d854a-6a28-4aa7-8c99-323e6ce46c2a".into());
    repo.create(&identifie_sans_bio).expect("candidat");

    for nom in ["Bagad Kemper", "Alan Stivell", "Denez Prigent"] {
        let mut a = Artist::new(nom.into());
        a.bio = Some(format!("Notice locale de {nom}."));
        repo.create(&a).expect("bio sans MBID");
    }
    for nom in ["Sonerien Du", "Startijenn", "Carlos Núñez", "Kepa"] {
        repo.create(&Artist::new(nom.into()))
            .expect("ni bio ni MBID");
    }

    // Aucune clé Last.fm : la passe de bios ne peut émettre aucune requête.
    SettingsRepo::with_backend(state.backend.clone())
        .set("lastfm_api_key", "")
        .expect("réglage posé vide");

    tune_server::routes::router(state)
}

/// Lit une route et rend `(statut, JSON)`. Le corps brut apparaît dans le
/// message d'échec : une réponse vide — route mal préfixée, filtre
/// d'authentification — se lit d'un coup d'œil plutôt qu'en « EOF while
/// parsing a value ».
async fn appeler(app: &axum::Router, requete: Request<Body>) -> (StatusCode, Value) {
    let reponse = app
        .clone()
        .oneshot(requete)
        .await
        .expect("appel de la route");
    let statut = reponse.status();
    let octets = axum::body::to_bytes(reponse.into_body(), 1 << 20)
        .await
        .expect("corps de la reponse");
    let json: Value = serde_json::from_slice(&octets).unwrap_or_else(|e| {
        panic!(
            "reponse non JSON (statut {statut}) : {e} — corps brut : {:?}",
            String::from_utf8_lossy(&octets)
        )
    });
    (statut, json)
}

async fn statut_enrichissement(app: &axum::Router) -> (StatusCode, Value) {
    appeler(
        app,
        Request::builder()
            .uri("/api/v1/system/enrichment/status")
            .body(Body::empty())
            .unwrap(),
    )
    .await
}

/// Le cœur de #2258 : le panneau d'enrichissement dit enfin combien
/// d'artistes la clé du fonds écarte, **des deux côtés**.
///
/// Contre-épreuve : retirer `"fonds_communautaire": …` de la réponse de
/// `enrichment_status`, ou remettre `AND musicbrainz_id IS NOT NULL AND
/// musicbrainz_id != ''` dans `sql::count_bio_sans_mbid`, fait rougir ce test.
#[tokio::test]
async fn le_panneau_chiffre_les_artistes_ecartes_par_la_cle_du_fonds() {
    let app = app_bibliotheque_de_bilou();
    let (code, corps) = statut_enrichissement(&app).await;
    assert_eq!(code, StatusCode::OK);

    let fonds = &corps["fonds_communautaire"];
    assert!(
        !fonds.is_null(),
        "le panneau doit porter la mesure de l'exclusion, pas la taire : {corps}"
    );
    assert_eq!(
        fonds["bios_non_partagees"], 3,
        "trois biographies locales qu'aucun autre utilisateur ne recevra jamais"
    );
    assert_eq!(
        fonds["artistes_non_servis"], 4,
        "quatre artistes qu'aucune biographie communautaire ne pourra servir"
    );
}

/// Un nombre sans sa cause ne s'explique pas. Le panneau nomme la CLÉ qui
/// produit l'exclusion, et le remède qui existe déjà dans le serveur :
/// `batch_match_artist_mbids`, lancé par `POST /system/enrich`.
#[tokio::test]
async fn le_panneau_nomme_la_cle_et_le_remede() {
    let app = app_bibliotheque_de_bilou();
    let (_, corps) = statut_enrichissement(&app).await;

    assert_eq!(
        corps["fonds_communautaire"]["cle"], "musicbrainz_id",
        "l'ecran doit pouvoir dire POURQUOI ces artistes sont ecartes"
    );
    assert_eq!(
        corps["fonds_communautaire"]["remede"], "artist_mbid_matching",
        "le remede existe deja cote serveur : l'appariement MBID"
    );
}

/// La réponse du bouton « récupérer les biographies » porte le même compte.
///
/// `artists_without_bio` annonce à l'utilisateur un travail à faire ; une part
/// de ce travail ne peut RIEN attendre du fonds communautaire. Les deux
/// nombres doivent voyager ensemble, sinon le premier promet ce que le second
/// interdit.
#[tokio::test]
async fn le_lancement_des_biographies_porte_le_meme_compte() {
    let app = app_bibliotheque_de_bilou();
    let (code, corps) = appeler(
        &app,
        Request::builder()
            .method("POST")
            .uri("/api/v1/system/enrich-bios")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(code, StatusCode::ACCEPTED);

    assert_eq!(
        corps["artists_without_bio"], 5,
        "cinq artistes sans biographie : un identifie, quatre non"
    );
    assert_eq!(
        corps["fonds_communautaire"]["artistes_non_servis"], 4,
        "quatre de ces cinq ne peuvent rien attendre du fonds communautaire"
    );
    assert_eq!(corps["fonds_communautaire"]["bios_non_partagees"], 3);
}

/// TÉMOIN — un artiste AVEC MBID est traité exactement comme avant.
///
/// Le compte de #2258 s'ajoute à côté du chemin qui marche ; il ne l'altère
/// pas. `artists_with_mbid` continue de voir les deux artistes identifiés, et
/// aucun d'eux n'entre dans le compte des écartés.
///
/// Ce test est vert AVANT comme APRÈS le correctif : il tombe si le compte a
/// été obtenu en desserrant l'une des deux requêtes de production.
#[tokio::test]
async fn temoin_un_artiste_avec_mbid_reste_hors_du_compte_des_ecartes() {
    let app = app_bibliotheque_de_bilou();
    let (code, corps) = statut_enrichissement(&app).await;
    assert_eq!(code, StatusCode::OK);

    // `artists_with_mbid` est un COUNT direct sur la colonne : c'est le
    // décompte qui dit, sans intermédiaire, que le chemin qui marche marche
    // toujours pour les deux artistes identifiés.
    assert_eq!(
        corps["stats"]["artists_with_mbid"], 2,
        "les deux artistes identifies restent identifies"
    );

    let ecartes = corps["fonds_communautaire"]["bios_non_partagees"]
        .as_i64()
        .expect("un nombre")
        + corps["fonds_communautaire"]["artistes_non_servis"]
            .as_i64()
            .expect("un nombre");
    assert_eq!(
        ecartes, 7,
        "sept artistes sans MBID exactement — les deux identifies n'en sont pas"
    );
}

/// TÉMOIN — le contrat existant du panneau ne bouge pas d'un champ.
///
/// Vert avant comme après : il tombe si l'ajout de `fonds_communautaire` a
/// déplacé ou renommé quoi que ce soit de ce que la route servait déjà.
#[tokio::test]
async fn temoin_le_contrat_existant_du_panneau_ne_bouge_pas() {
    let app = app_bibliotheque_de_bilou();
    let (code, corps) = statut_enrichissement(&app).await;
    assert_eq!(code, StatusCode::OK);

    for champ in [
        "total_tracks",
        "total_artists",
        "total_albums",
        "artists_with_bio",
        "artists_with_image",
        "artists_with_mbid",
        "albums_with_cover",
        "albums_with_bio",
    ] {
        assert!(
            corps["stats"].get(champ).is_some(),
            "le panneau doit continuer de servir stats.{champ}"
        );
    }
    for champ in ["premium", "last_run", "bio_last_run"] {
        assert!(
            corps.get(champ).is_some(),
            "{champ} fait partie du contrat existant"
        );
    }
}

/// Un garde qui ne trouve rien doit le dire, pas se taire.
///
/// Sur une bibliothèque entièrement identifiée, les deux nombres valent zéro —
/// et la clé reste servie. Un `fonds_communautaire` absent se lirait « serveur
/// trop ancien » ; un zéro se lit « personne n'est écarté ».
#[tokio::test]
async fn une_bibliotheque_entierement_identifiee_annonce_zero_et_non_rien() {
    let state = tune_server::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    let repo = ArtistRepo::with_backend(state.backend.clone());
    let mut a = Artist::new("Pink Floyd".into());
    a.musicbrainz_id = Some("83d91898-7763-47d7-b03b-b92132375c47".into());
    a.bio = Some("Groupe de rock anglais.".into());
    repo.create(&a).expect("artiste identifié");
    let app = tune_server::routes::router(state);

    let (code, corps) = statut_enrichissement(&app).await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(corps["fonds_communautaire"]["bios_non_partagees"], 0);
    assert_eq!(corps["fonds_communautaire"]["artistes_non_servis"], 0);
    assert_eq!(corps["fonds_communautaire"]["cle"], "musicbrainz_id");
}
