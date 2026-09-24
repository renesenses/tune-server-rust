//! Le plafond de la file d'attente est un RÉGLAGE, et ses bornes se ferment
//! à l'écriture (#2901, suite).
//!
//! # Ce qui existait déjà
//!
//! #2901 a transformé la constante `500` de #2228 en réglage
//! `shuffle_max_tracks` : publié par `GET /system/config` avec ses bornes
//! (`shuffle_max_tracks_min` / `_max`), écrit par `PATCH /system/config`, lu
//! par `shuffle_all`. Le défaut n'a pas bougé — 500 pour qui ne touche à rien.
//!
//! # Ce que cette épreuve ajoute
//!
//! Le `PATCH` ne validait rien. `{"shuffle_max_tracks": 100000}` s'installait
//! en base, répondait `{"ok": true}`, et la LECTURE ramenait ensuite 5 000 en
//! silence. L'utilisateur croyait avoir levé un plafond qui n'avait pas bougé :
//! il lançait une lecture aléatoire sur 30 000 pistes, en recevait 5 000, et
//! cherchait la panne ailleurs. Exactement le défaut muet que #4154 a refermé
//! sur les plafonds d'indexation.
//!
//! Les trois preuves, dans cet ordre :
//!
//! 1. **le défaut n'a pas bougé** — base vierge, 500, avec ses deux bornes ;
//! 2. **une valeur réglée mord sur le chemin qui plafonne VRAIMENT** — la file
//!    écrite en base après `POST /playback/shuffle-all`, pas un compteur de la
//!    réponse ;
//! 3. **hors bornes ⇒ 400**, et la base ne bouge pas.
//!
//! La preuve n° 2 se lit dans `GET /zones/{id}/queue` et non dans le corps de
//! `shuffle-all` : sur un banc sans vraie sortie audio, la lecture échoue après
//! l'écriture de la file, et le corps serait alors celui d'une erreur de
//! lecture. La file, elle, est déjà écrite — et c'est elle, le plafond.
//!
//! ⚠️ `tune-server` porte `autotests = false` : ce fichier est une cible
//! `[[test]]` déclarée dans `Cargo.toml`. Sans elle il ne serait JAMAIS
//! compilé, et la garde serait verte contre rien.

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

use tune_core::db::models::Track;
use tune_core::db::track_repo::TrackRepo;
use tune_core::db::zone_repo::ZoneRepo;
use tune_core::playback::queue::{
    SHUFFLE_MAX_TRACKS_CEILING, SHUFFLE_MAX_TRACKS_DEFAULT, SHUFFLE_MAX_TRACKS_FLOOR,
    SHUFFLE_MAX_TRACKS_KEY,
};
use tune_server::state::AppState;

/// Combien de pistes le banc publie. **Strictement plus que le plafond posé**,
/// sinon la troncature n'aurait pas lieu et l'épreuve serait verte pour rien.
const PISTES_DU_BANC: usize = 9;
/// Le plafond posé pendant l'épreuve.
const PLAFOND_POSE: i64 = 3;

/// Le plancher du détecteur : un banc retombé sous le plafond ne mesurerait
/// plus rien, et un défaut déplacé rendrait la preuve n° 1 tautologique.
#[test]
fn le_banc_et_les_bornes_tiennent_debout() {
    assert!(
        (PISTES_DU_BANC as i64) > PLAFOND_POSE,
        "le banc ({PISTES_DU_BANC}) doit dépasser le plafond posé ({PLAFOND_POSE}), \
         sinon rien n'est tronqué et l'épreuve ne mesure rien"
    );
    assert!(
        (PISTES_DU_BANC as i64) < SHUFFLE_MAX_TRACKS_DEFAULT,
        "le banc doit tenir SOUS le défaut ({SHUFFLE_MAX_TRACKS_DEFAULT}) : c'est ce qui \
         distingue « le défaut ne tronque pas » de « le banc est trop petit »"
    );
    assert_eq!(
        SHUFFLE_MAX_TRACKS_DEFAULT, 500,
        "le défaut est la garantie donnée à Jean Valjean (#2228) : il ne se déplace pas"
    );
}

// ── Plomberie ────────────────────────────────────────────────────────────────

fn etat() -> AppState {
    AppState::new(":memory:", 0, Default::default()).expect("état serveur isolé")
}

fn zone(etat: &AppState) -> i64 {
    ZoneRepo::with_backend(etat.backend.clone())
        .create("Banc", Some("mock"), Some("sortie-essai"))
        .expect("création de zone")
}

/// Le banc : `PISTES_DU_BANC` pistes locales distinctes, toutes tirables.
fn remplir_la_bibliotheque(etat: &AppState) {
    let pistes = TrackRepo::with_backend(etat.backend.clone());
    for i in 0..PISTES_DU_BANC {
        let mut t = Track::new(format!("Piste {i}"));
        t.file_path = Some(format!("/musique/piste-{i}.flac"));
        t.format = Some("flac".into());
        t.duration_ms = 180_000;
        pistes.create(&t).expect("insertion de piste");
    }
}

async fn envoyer(app: &Router, requete: Request<Body>) -> (StatusCode, Value) {
    let reponse = app
        .clone()
        .oneshot(requete)
        .await
        .expect("routeur en échec");
    let statut = reponse.status();
    let octets = axum::body::to_bytes(reponse.into_body(), usize::MAX)
        .await
        .expect("corps lisible");
    (
        statut,
        serde_json::from_slice(&octets).unwrap_or(Value::Null),
    )
}

async fn lire(app: &Router, chemin: &str) -> (StatusCode, Value) {
    envoyer(app, Request::get(chemin).body(Body::empty()).unwrap()).await
}

async fn patcher(app: &Router, corps: Value) -> (StatusCode, Value) {
    envoyer(
        app,
        Request::patch("/api/v1/system/config")
            .header("content-type", "application/json")
            .body(Body::from(corps.to_string()))
            .unwrap(),
    )
    .await
}

/// Lance la lecture aléatoire de toute la bibliothèque sur `zone_id`.
///
/// Le statut n'est PAS vérifié : sur un banc sans sortie audio réelle, la
/// lecture peut échouer après que la file a été écrite. C'est la file qui est
/// éprouvée, pas la lecture.
async fn lecture_aleatoire(app: &Router, zone_id: i64) {
    let _ = envoyer(
        app,
        Request::post(format!("/api/v1/playback/shuffle-all?zone_id={zone_id}"))
            .header("content-type", "application/json")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
}

/// Combien de lignes la file de la zone porte réellement, lues par la route
/// que le client lit — pas par le dépôt, pour que la garde tienne aussi si
/// quelqu'un intercale une couche entre les deux.
async fn longueur_de_la_file(app: &Router, zone_id: i64) -> usize {
    let (statut, corps) = lire(app, &format!("/api/v1/zones/{zone_id}/queue")).await;
    assert_eq!(statut, StatusCode::OK, "la file se lit : {corps}");
    corps["tracks"]
        .as_array()
        .map(Vec::len)
        .unwrap_or_else(|| panic!("la file doit porter un tableau `tracks` : {corps}"))
}

// ── Les épreuves ─────────────────────────────────────────────────────────────

/// **1. Base vierge : le défaut est publié, avec ses deux bornes.**
///
/// Les bornes sont publiées pour que le contrôle du client web n'ait pas à les
/// écrire en dur. Un client qui les coderait à la main proposerait demain un
/// intervalle que le serveur n'honore plus.
#[tokio::test(flavor = "multi_thread")]
async fn i2901_une_base_vierge_publie_500_et_ses_bornes() {
    let app = tune_server::routes::router(etat());

    let (statut, config) = lire(&app, "/api/v1/system/config").await;
    assert_eq!(statut, StatusCode::OK);
    assert_eq!(
        config[SHUFFLE_MAX_TRACKS_KEY],
        json!(SHUFFLE_MAX_TRACKS_DEFAULT),
        "sans réglage, le plafond publié doit rester 500 — corps {config}"
    );
    assert_eq!(
        config["shuffle_max_tracks_min"],
        json!(SHUFFLE_MAX_TRACKS_FLOOR),
        "le plancher doit être publié — corps {config}"
    );
    assert_eq!(
        config["shuffle_max_tracks_max"],
        json!(SHUFFLE_MAX_TRACKS_CEILING),
        "le plafond doit être publié — corps {config}"
    );
}

/// **2. Sans réglage, le défaut ne tronque pas un banc plus petit que lui.**
///
/// Contre-épreuve de l'épreuve n° 3 : sans elle, une file de 3 pourrait venir
/// d'un bug et non du réglage.
#[tokio::test(flavor = "multi_thread")]
async fn i2901_au_defaut_le_banc_entier_entre_dans_la_file() {
    let etat = etat();
    let zone_id = zone(&etat);
    remplir_la_bibliotheque(&etat);
    let app = tune_server::routes::router(etat);

    lecture_aleatoire(&app, zone_id).await;
    assert_eq!(
        longueur_de_la_file(&app, zone_id).await,
        PISTES_DU_BANC,
        "au défaut ({SHUFFLE_MAX_TRACKS_DEFAULT}), les {PISTES_DU_BANC} pistes du banc \
         doivent toutes entrer"
    );
}

/// **3. Le réglage mord là où la file se fabrique.**
///
/// C'est la seule preuve qui compte : un réglage qui se relit mais que la
/// lecture aléatoire ignore serait un champ de saisie décoratif.
#[tokio::test(flavor = "multi_thread")]
async fn i2901_le_plafond_regle_borne_reellement_la_file() {
    let etat = etat();
    let zone_id = zone(&etat);
    remplir_la_bibliotheque(&etat);
    let app = tune_server::routes::router(etat);

    let (statut, reponse) = patcher(&app, json!({ SHUFFLE_MAX_TRACKS_KEY: PLAFOND_POSE })).await;
    assert_eq!(statut, StatusCode::OK, "le PATCH est accepté : {reponse}");

    let (_, config) = lire(&app, "/api/v1/system/config").await;
    assert_eq!(
        config[SHUFFLE_MAX_TRACKS_KEY],
        json!(PLAFOND_POSE),
        "le réglage doit se souvenir — corps {config}"
    );

    lecture_aleatoire(&app, zone_id).await;
    assert_eq!(
        longueur_de_la_file(&app, zone_id).await,
        PLAFOND_POSE as usize,
        "la file doit porter exactement le plafond réglé, sur un banc de \
         {PISTES_DU_BANC} pistes"
    );
}

/// **4. Hors bornes : 400, le refus NOMME les bornes, et la base ne bouge pas.**
///
/// « Ramené en silence » et « refusé » se ressemblent tant qu'on ne regarde
/// que la valeur appliquée : les deux donnent 5 000. Ce qui les sépare, c'est
/// ce que l'utilisateur apprend. On mesure donc les trois à la fois : le
/// statut, la phrase, et la valeur restée en base.
#[tokio::test(flavor = "multi_thread")]
async fn i2901_une_valeur_hors_bornes_est_refusee_et_ne_s_installe_pas() {
    let app = tune_server::routes::router(etat());

    // Un réglage valide d'abord : c'est LUI qui doit survivre aux refus.
    let (statut, _) = patcher(&app, json!({ SHUFFLE_MAX_TRACKS_KEY: 1_200 })).await;
    assert_eq!(statut, StatusCode::OK);

    for valeur in [json!(0), json!(-1), json!(100_000), json!("beaucoup")] {
        let (statut, corps) = patcher(&app, json!({ SHUFFLE_MAX_TRACKS_KEY: valeur })).await;
        assert_eq!(
            statut,
            StatusCode::BAD_REQUEST,
            "{valeur} doit être REFUSÉE, pas acceptée puis ramenée en silence : {corps}"
        );
        let phrase = corps.to_string();
        assert!(
            phrase.contains(&SHUFFLE_MAX_TRACKS_FLOOR.to_string())
                && phrase.contains(&SHUFFLE_MAX_TRACKS_CEILING.to_string()),
            "le refus de {valeur} doit nommer les deux bornes, sinon il laisse \
             l'utilisateur deviner quoi écrire : {corps}"
        );
    }

    let (_, config) = lire(&app, "/api/v1/system/config").await;
    assert_eq!(
        config[SHUFFLE_MAX_TRACKS_KEY],
        json!(1_200),
        "un PATCH refusé ne doit RIEN écrire : le réglage précédent reste — {config}"
    );
}

/// **5. Les deux bornes elles-mêmes passent.**
///
/// Un `<` mis pour un `<=` rendrait 5 000 injoignable — c'est précisément le
/// maximum que l'écran propose.
#[tokio::test(flavor = "multi_thread")]
async fn i2901_les_bornes_incluses_sont_acceptees() {
    let app = tune_server::routes::router(etat());

    for valeur in [SHUFFLE_MAX_TRACKS_FLOOR, SHUFFLE_MAX_TRACKS_CEILING] {
        let (statut, corps) = patcher(&app, json!({ SHUFFLE_MAX_TRACKS_KEY: valeur })).await;
        assert_eq!(
            statut,
            StatusCode::OK,
            "{valeur} est une borne INCLUSE et doit être acceptée : {corps}"
        );
        let (_, config) = lire(&app, "/api/v1/system/config").await;
        assert_eq!(
            config[SHUFFLE_MAX_TRACKS_KEY],
            json!(valeur),
            "la borne acceptée doit aussi être celle qui s'applique — {config}"
        );
    }
}
