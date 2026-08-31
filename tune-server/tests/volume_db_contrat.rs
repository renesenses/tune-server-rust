//! Le volume se lit ET se règle en décibels — contrat HTTP (#1274).
//!
//! `zaurux`, sur le fil forum-hifi n°41831, en cinq mots : « pas de réglage au
//! db près ». Il vient de Roon et de HQPlayer, où le volume logiciel s'affiche
//! et se saisit en dB. Dans Tune, le volume était un multiplicateur linéaire
//! présenté en pourcentage : un curseur à 90 % vaut −0,9 dB, pas −10 dB, et il
//! n'existait aucun moyen de viser −18 dB.
//!
//! Le client web savait déjà *afficher* des dB — `VolumeControl.svelte` calcule
//! `20·log10(volume)` dans son coin depuis la préférence `volumeDisplay`. Ce
//! qui manquait, et que ce fichier garde, c'est le reste :
//!
//! 1. le **serveur** dit le dB, au lieu de laisser chaque client le recalculer
//!    (six clients, six occasions de diverger) ;
//! 2. le serveur **accepte** le dB en écriture, ce que ni le pour-cent entier
//!    du PATCH ni le curseur ne permettaient ;
//! 3. le champ est **additif** — `volume` reste présent, au même endroit et
//!    dans la même unité, parce que six clients déployés en dépendent, dont
//!    trois avec un défaut silencieux (`?? 0.5`, `?? 1.0`) qui masquerait sa
//!    disparition au lieu de la signaler.
//!
//! ⚠️ `tune-server` porte `autotests = false` — ce fichier n'est compilé que
//! parce qu'il est déclaré dans l'agrégateur `server_contracts.rs`. Voir
//! `tests_orphelins.rs`.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

fn app() -> axum::Router {
    let state = tune_server::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    tune_server::routes::router(state)
}

async fn envoyer(app: &axum::Router, requete: Request<Body>) -> (StatusCode, Value) {
    let resp = app.clone().oneshot(requete).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(json!(null)),
    )
}

async fn get(app: &axum::Router, path: &str) -> (StatusCode, Value) {
    envoyer(app, Request::get(path).body(Body::empty()).unwrap()).await
}

async fn corps(methode: &str, path: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method(methode)
        .uri(path)
        .header("Content-Type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

async fn post(app: &axum::Router, path: &str, body: Value) -> (StatusCode, Value) {
    envoyer(app, corps("POST", path, body).await).await
}

async fn put(app: &axum::Router, path: &str, body: Value) -> (StatusCode, Value) {
    envoyer(app, corps("PUT", path, body).await).await
}

async fn patch(app: &axum::Router, path: &str, body: Value) -> (StatusCode, Value) {
    envoyer(app, corps("PATCH", path, body).await).await
}

/// Crée une zone et rend son id.
async fn zone(app: &axum::Router) -> i64 {
    let (status, body) = post(app, "/api/v1/zones", json!({"name": "Salon"})).await;
    assert_eq!(status, StatusCode::CREATED, "création de zone : {body}");
    body["id"].as_i64().expect("un id de zone")
}

/// Le dB attendu pour un volume linéaire, recalculé ICI plutôt qu'importé.
///
/// C'est volontaire : réutiliser `volume_scale` ferait que le test passerait
/// encore si la loi de conversion changeait sous lui. Cette formule est celle
/// que le client web applique déjà (`VolumeControl.svelte`), donc celle avec
/// laquelle le serveur ne doit pas se contredire.
fn db_attendu(lineaire: f64) -> f64 {
    20.0 * lineaire.log10()
}

fn presque(gauche: f64, droite: f64) -> bool {
    (gauche - droite).abs() < 1e-9
}

#[tokio::test]
async fn le_db_accompagne_le_pour_cent_sur_les_surfaces_de_zone() {
    let app = app();
    let id = zone(&app).await;

    // Les trois charges utiles qu'un client lit pour peupler un curseur.
    for chemin in [
        "/api/v1/zones".to_string(),
        format!("/api/v1/zones/{id}"),
        format!("/api/v1/zones/{id}/status"),
    ] {
        let (status, body) = get(&app, &chemin).await;
        assert_eq!(status, StatusCode::OK, "{chemin} : {body}");
        let z = if body.is_array() { &body[0] } else { &body };

        // Le champ historique est intact — c'est la rétro-compatibilité, et
        // c'est ce que six clients déployés lisent.
        let lineaire = z["volume"]
            .as_f64()
            .unwrap_or_else(|| panic!("{chemin} : volume absent — {body}"));
        assert!(
            (0.0..=1.0).contains(&lineaire),
            "{chemin} : volume hors 0..1 ({lineaire})"
        );

        // Et le champ neuf en est la lecture exacte, pas une autre mesure.
        let db = z["volume_db"]
            .as_f64()
            .unwrap_or_else(|| panic!("{chemin} : volume_db absent — {body}"));
        assert!(
            presque(db, db_attendu(lineaire)),
            "{chemin} : {lineaire} devrait valoir {} dB, pas {db}",
            db_attendu(lineaire)
        );
    }
}

#[tokio::test]
async fn un_reglage_en_db_atteint_sa_cible() {
    let app = app();
    let id = zone(&app).await;

    // Le geste que l'issue réclame : demander −20 dB, pas « 10 % ».
    let (status, body) = post(
        &app,
        &format!("/api/v1/zones/{id}/volume"),
        json!({"volume_db": -20.0}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        presque(body["volume"].as_f64().expect("volume"), 0.1),
        "−20 dB vaut 0,1 en linéaire — reçu {body}"
    );
    assert!(
        presque(body["volume_db"].as_f64().expect("volume_db"), -20.0),
        "la réponse doit confirmer la cible — reçu {body}"
    );

    // Et la valeur se relit, elle n'est pas seulement renvoyée en écho.
    let (_, relu) = get(&app, &format!("/api/v1/zones/{id}/status")).await;
    assert!(
        presque(relu["volume_db"].as_f64().expect("volume_db"), -20.0),
        "relecture : {relu}"
    );
}

/// Le volume que le serveur PERSISTE, tel qu'il est aujourd'hui.
///
/// `zones.volume` est une colonne `INTEGER` 0..100. L'état de lecture, lui,
/// garde le `f64` exact — d'où deux vues qui ne disent pas la même chose :
///
/// | surface | source | précision |
/// |---|---|---|
/// | réponse à POST/PUT `…/volume` | la valeur commandée | exacte |
/// | `GET /zones` et `GET /zones/{id}/status` | état de lecture (`f64`) | exacte |
/// | `GET /zones/{id}` | colonne `zones.volume` | **arrondie au pour-cent** |
///
/// L'arrondi coûte au plus 0,09 dB en haut d'échelle, mais 6 dB entre 1 % et
/// 2 % : le « réglage au dB près » ne survit pas à un redémarrage sous −30 dB.
/// Le corriger demande de changer le type de la colonne aux QUATRE endroits du
/// schéma (SQLite neuf, migration SQLite, PG neuf, migration PG) et de relire
/// chaque `z.volume as f64 / 100.0` — hors périmètre de #1274, et à arbitrer.
///
/// Ce test n'excuse pas la limite, il la CLOUE : il exige la valeur exacte que
/// produit la quantification, pas une tolérance floue. Si quelqu'un change la
/// résolution de la persistance, ce test rougit et la question se repose.
#[tokio::test]
async fn la_vue_persistee_arrondit_au_pour_cent_limite_connue_et_mesuree() {
    let app = app();
    let id = zone(&app).await;

    let mut pire_ecart: f64 = 0.0;
    // Jusqu'à −40 dB seulement : c'est 1 %, le dernier cran que la colonne
    // sait encore représenter. En dessous, elle ne quantifie plus, elle coupe
    // — voir `sous_moins_quarante_six_db_la_persistance_tombe_au_silence`.
    for n in 0..=40 {
        let cible = -f64::from(n);
        let (status, _) = post(
            &app,
            &format!("/api/v1/zones/{id}/volume"),
            json!({ "volume_db": cible }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        // Ce que la colonne entière peut retenir de cette cible.
        let lineaire = 10f64.powf(cible / 20.0);
        let quantifie = (lineaire * 100.0).round() / 100.0;
        let attendu = db_attendu(quantifie);

        let (_, relu) = get(&app, &format!("/api/v1/zones/{id}")).await;
        let relu_db = relu["volume_db"].as_f64().expect("volume_db");
        assert!(
            presque(relu_db, attendu),
            "{cible} dB persisté : attendu {attendu} dB (soit {quantifie} linéaire), reçu {relu_db}"
        );
        pire_ecart = pire_ecart.max((attendu - cible).abs());
    }

    // Et l'ampleur du défaut est elle aussi clouée : c'est le chiffre qui
    // justifie l'arbitrage, il ne doit pas pouvoir enfler en silence.
    assert!(
        (1.0..4.0).contains(&pire_ecart),
        "l'écart maximal de la persistance sur 0..−40 dB a changé : {pire_ecart} dB"
    );
}

/// La même colonne entière, poussée jusqu'à son point de rupture.
///
/// Sous ≈ −46 dB, `round(volume × 100)` vaut **zéro**. Ce n'est plus un
/// arrondi, c'est une coupure : la zone se rallume muette. Le défaut est
/// ANTÉRIEUR à #1274 — `POST …/volume {"volume": 0.004}` produisait déjà cela
/// depuis toujours — mais le réglage en dB le met à portée de doigt, parce que
/// −48 dB est un réglage plausible pour qui a beaucoup de gain en aval.
///
/// Il n'est PAS corrigé ici : borner la valeur persistée à 1 % changerait le
/// volume de quelqu'un sans qu'il le demande, et cela relève du même arbitrage
/// que la résolution de la colonne. Ce test le rend visible et reproductible,
/// pour que l'arbitrage se fasse sur un fait et non sur une intuition.
#[tokio::test]
async fn sous_moins_quarante_six_db_la_persistance_tombe_au_silence() {
    let app = app();
    let id = zone(&app).await;

    let (status, commande) = post(
        &app,
        &format!("/api/v1/zones/{id}/volume"),
        json!({"volume_db": -48.0}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{commande}");

    // La commande, elle, est honorée : le son sort bien à −48 dB.
    assert!(
        presque(commande["volume_db"].as_f64().expect("volume_db"), -48.0),
        "la commande doit être exacte — {commande}"
    );
    let (_, vivant) = get(&app, &format!("/api/v1/zones/{id}/status")).await;
    assert!(
        presque(vivant["volume_db"].as_f64().expect("volume_db"), -48.0),
        "l'état de lecture doit être exact — {vivant}"
    );

    // Mais ce qui est écrit en base ne l'est plus : c'est zéro, donc du
    // silence, donc `null` en dB — indiscernable d'un mute volontaire.
    let (_, persiste) = get(&app, &format!("/api/v1/zones/{id}")).await;
    assert_eq!(
        persiste["volume"],
        json!(0.0),
        "la colonne entière tombe à zéro — {persiste}"
    );
    assert_eq!(
        persiste["volume_db"],
        Value::Null,
        "et le dB suit honnêtement : silence = null, jamais un plancher — {persiste}"
    );
}

#[tokio::test]
async fn le_champ_historique_reste_seul_maitre_a_bord() {
    let app = app();
    let id = zone(&app).await;

    // Exactement la requête que le client web, Flutter et iPadOS envoient
    // aujourd'hui. Elle ne doit RIEN perdre : ni son code, ni son champ.
    let (status, body) = post(
        &app,
        &format!("/api/v1/zones/{id}/volume"),
        json!({"volume": 0.5}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(presque(body["volume"].as_f64().expect("volume"), 0.5));
    // Le dB arrive en plus, sans avoir été demandé — c'est le but.
    assert!(presque(
        body["volume_db"].as_f64().expect("volume_db"),
        -6.020_599_913_279_624
    ));
}

#[tokio::test]
async fn le_zero_reste_le_silence_et_non_un_plancher() {
    let app = app();
    let id = zone(&app).await;

    let (status, body) = post(
        &app,
        &format!("/api/v1/zones/{id}/volume"),
        json!({"volume": 0.0}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // `null`, pas −60 ni −144 : le silence n'a pas d'atténuation finie, et un
    // client qui renverrait un plancher chiffré rallumerait la zone.
    assert_eq!(
        body["volume_db"],
        Value::Null,
        "le silence doit sortir en null — reçu {body}"
    );
    // Le champ est PRÉSENT, seulement nul : un client qui teste sa présence
    // pour choisir son affichage ne doit pas basculer en mode « pas de dB ».
    assert!(
        body.as_object()
            .expect("un objet")
            .contains_key("volume_db"),
        "volume_db doit être présent même à zéro — {body}"
    );
}

#[tokio::test]
async fn les_deux_unites_ensemble_sont_refusees() {
    let app = app();
    let id = zone(&app).await;

    // Accepter les deux obligerait à inventer un gagnant, et le perdant
    // partirait en silence — sur un volume, c'est un niveau surprise.
    let (status, body) = post(
        &app,
        &format!("/api/v1/zones/{id}/volume"),
        json!({"volume": 0.5, "volume_db": -20.0}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    // Et une requête vide ne choisit pas un volume à la place de l'auditeur.
    let (status, body) = post(&app, &format!("/api/v1/zones/{id}/volume"), json!({})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
}

#[tokio::test]
async fn un_gain_positif_est_refuse_pas_rabote() {
    let app = app();
    let id = zone(&app).await;

    // Il n'y a pas de volume au-dessus de la pleine échelle, il n'y a que de
    // l'écrêtage. Rendre 100 % en silence ferait croire au +3 dB obtenu.
    let (status, body) = post(
        &app,
        &format!("/api/v1/zones/{id}/volume"),
        json!({"volume_db": 3.0}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    // 0 dB, lui, est la pleine échelle : parfaitement légitime.
    let (status, body) = post(
        &app,
        &format!("/api/v1/zones/{id}/volume"),
        json!({"volume_db": 0.0}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(presque(body["volume"].as_f64().expect("volume"), 1.0));
    assert!(presque(body["volume_db"].as_f64().expect("volume_db"), 0.0));
}

#[tokio::test]
async fn les_quatre_surfaces_d_ecriture_parlent_toutes_le_db() {
    let app = app();
    let id = zone(&app).await;

    // POST — web, Flutter, iPadOS.
    let (status, body) = post(
        &app,
        &format!("/api/v1/zones/{id}/volume"),
        json!({"volume_db": -12.0}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "POST : {body}");
    // La relecture se fait sur l'état de lecture, seule vue exacte : la vue
    // persistée arrondit au pour-cent (cf.
    // `la_vue_persistee_arrondit_au_pour_cent_limite_connue_et_mesuree`).
    let (_, relu) = get(&app, &format!("/api/v1/zones/{id}/status")).await;
    assert!(
        presque(relu["volume_db"].as_f64().expect("volume_db"), -12.0),
        "après POST : {relu}"
    );

    // PUT — tune-remote et tune-widget. Même route, autre verbe : l'oublier
    // laisserait deux clients sur six sans réglage en dB.
    let (status, body) = put(
        &app,
        &format!("/api/v1/zones/{id}/volume"),
        json!({"volume_db": -6.0}),
    )
    .await;
    assert!(status.is_success(), "PUT : {status} {body}");
    let (_, relu) = get(&app, &format!("/api/v1/zones/{id}/status")).await;
    assert!(
        presque(relu["volume_db"].as_f64().expect("volume_db"), -6.0),
        "après PUT : {relu}"
    );

    // PATCH — la surface de réglage de zone, dont le champ `volume` est un
    // ENTIER 0..100 et ne peut donc pas viser un dB.
    let (status, body) = patch(
        &app,
        &format!("/api/v1/zones/{id}"),
        json!({"volume_db": -40.0}),
    )
    .await;
    assert!(status.is_success(), "PATCH : {status} {body}");
    let (_, relu) = get(&app, &format!("/api/v1/zones/{id}/status")).await;
    assert!(
        presque(relu["volume_db"].as_f64().expect("volume_db"), -40.0),
        "après PATCH : {relu}"
    );

    // Et le PATCH refuse les deux unités ensemble, comme les autres.
    let (status, body) = patch(
        &app,
        &format!("/api/v1/zones/{id}"),
        json!({"volume": 50, "volume_db": -6.0}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "PATCH : {body}");
}

#[tokio::test]
async fn le_patch_sans_volume_ne_touche_pas_au_volume() {
    let app = app();
    let id = zone(&app).await;

    post(
        &app,
        &format!("/api/v1/zones/{id}/volume"),
        json!({"volume_db": -20.0}),
    )
    .await;

    // Contre-épreuve du garde-fou d'arbitrage : « aucun des deux champs » est
    // le cas courant d'un PATCH, et il ne doit ni échouer ni remettre le
    // volume à une valeur par défaut.
    let (status, body) = patch(
        &app,
        &format!("/api/v1/zones/{id}"),
        json!({"name": "Cave"}),
    )
    .await;
    assert!(status.is_success(), "{status} {body}");

    let (_, relu) = get(&app, &format!("/api/v1/zones/{id}")).await;
    assert_eq!(relu["name"], "Cave");
    assert!(
        presque(relu["volume_db"].as_f64().expect("volume_db"), -20.0),
        "un PATCH sans volume a déplacé le volume : {relu}"
    );
}

#[tokio::test]
async fn le_reglage_au_db_pres_tient_sur_toute_l_echelle_utile() {
    let app = app();
    let id = zone(&app).await;

    // La promesse de l'issue, vérifiée cran par cran de 0 à −60 dB : demander
    // −N dB et relire doit rendre −N dB. C'est ce que le pour-cent entier ne
    // savait pas faire — entre 1 % et 2 %, il n'y a rien, et l'écart vaut 6 dB.
    for n in 0..=60 {
        let cible = -f64::from(n);
        let (status, body) = post(
            &app,
            &format!("/api/v1/zones/{id}/volume"),
            json!({ "volume_db": cible }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{cible} dB : {body}");
        let rendu = body["volume_db"].as_f64().expect("volume_db");
        assert!(
            presque(rendu, cible),
            "demandé {cible} dB, obtenu {rendu} dB"
        );

        // Les DEUX vues exactes : la liste des zones (qui préfère l'état de
        // lecture) et l'état de lecture lui-même. Ce sont elles que le client
        // relit après chaque commande pour repositionner son curseur.
        for chemin in [
            "/api/v1/zones".to_string(),
            format!("/api/v1/zones/{id}/status"),
        ] {
            let (_, relu) = get(&app, &chemin).await;
            let z = if relu.is_array() { &relu[0] } else { &relu };
            let relu_db = z["volume_db"].as_f64().expect("volume_db");
            assert!(
                presque(relu_db, cible),
                "{chemin} : relecture de {cible} dB → {relu_db} dB — {relu}"
            );
        }
    }
}
