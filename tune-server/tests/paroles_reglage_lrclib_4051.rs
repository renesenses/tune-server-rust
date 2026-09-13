//! #4051 — le réglage « Paroles en ligne (LRCLIB) » se lit, et son refus se dit.
//!
//! ## Ce que le ticket a mesuré, et ce qu'il n'a pas pu trancher
//!
//! Belkadi Yacine (fil forum 1776, ticket support 116, Tune 0.9.147 Fedora)
//! bascule « Paroles en ligne (LRCLIB) » dans Réglages → Bibliothèque. La case
//! reste cochée à l'écran, le serveur continue de répondre comme si elle était
//! éteinte, et sa table `settings` ne porte aucune ligne `%lrc%`. Trois heures
//! d'investigation, et le tri n'a pas su dire OÙ la valeur se perd :
//!
//! - l'écriture ? `PATCH /system/config` accepte n'importe quelle clé et
//!   l'écrit — **mesuré, elle fonctionne** ;
//! - la relecture ? `GET /system/config` republie toutes les lignes — elle
//!   fonctionne aussi, **une fois la ligne posée** ;
//! - l'affichage ? `lyricsOnline.ts` lit `lyrics_lrclib_enabled` dans
//!   `GET /system/config` et tient trois états (`true`/`false`/`null`).
//!
//! Rien n'était mesurable **depuis le serveur** : la porte d'écriture des
//! réglages ne journalise pas une ligne, et la branche « recherche en ligne
//! éteinte » des deux routes paroles rend un 404 `no_lyrics` strictement
//! identique à celui d'un titre sans paroles, sans un `tracing::` nulle part.
//! `journalctl -u tune.service | grep -i lrc` reste donc vide **quoi qu'il
//! arrive** — c'est l'argument dont le testeur a conclu, à tort, que le serveur
//! n'essayait pas. Ce fichier est la garde de ces trois trous.
//!
//! ## Les quatre témoins
//!
//! 1. Sur une base vierge, `GET /system/config` **publie** le réglage à
//!    `false`. Il l'omettait : une clé absente se lit « je ne sais pas », pas
//!    « non » — la règle que ce même fichier applique déjà à
//!    `local_exclusive_mode_supported` et `replaygain_analysis_enabled`, et que
//!    `GET /system/profile` respecte pour CETTE clé-ci depuis #3577.
//! 2. L'aller-retour complet : `PATCH` puis `GET` rendent `true`, et la ligne
//!    en base porte bien la chaîne `"true"` que les lecteurs comparent.
//! 3. Le refus se DIT : les deux routes paroles laissent une ligne nommant le
//!    réglage quand elles s'abstiennent.
//! 4. Le journal des écritures nomme les CLÉS, jamais les VALEURS — un réglage
//!    secret écrit par la même porte n'y laisse pas son contenu.
//!
//! Déclaré dans `tune-server/Cargo.toml` : `autotests = false`, un fichier de
//! `tests/` sans `[[test]]` n'est jamais compilé.

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_server::state::AppState;

/// Le nom du réglage, tel que le serveur le stocke et le publie.
const CLE: &str = "lyrics_lrclib_enabled";

// ── Capture du journal ───────────────────────────────────────────────────
//
// Même montage que `tests/journal_sondage_hqplayer.rs` : l'abonné global n'est
// posé qu'une fois, donc ce binaire ne contient qu'UN test.

#[derive(Clone, Default)]
struct JournalCapture(Arc<Mutex<Vec<u8>>>);

impl JournalCapture {
    fn texte(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

impl std::io::Write for JournalCapture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for JournalCapture {
    type Writer = JournalCapture;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

// ── Plomberie HTTP ───────────────────────────────────────────────────────

fn app() -> (axum::Router, AppState) {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let router = tune_server::routes::router(state.clone());
    (router, state)
}

async fn get(app: &axum::Router, path: &str) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(Request::get(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

async fn patch(app: &axum::Router, path: &str, corps: Value) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::patch(path)
                .header("content-type", "application/json")
                .body(Body::from(corps.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

/// Une piste SANS fichier ni étiquette : la cascade paroles ne peut donc
/// s'arrêter que sur la garde du consentement, celle que ce test mesure.
///
/// L'artiste doit exister en base (`tracks` n'a pas de colonne `artist_name`,
/// il vient d'une jointure) — sinon la route s'arrêterait plus loin, sur la
/// garde `artist.is_empty()`, et le témoin serait vert pour la mauvaise raison.
fn inserer_piste(state: &AppState) -> i64 {
    let artistes = tune_core::db::artist_repo::ArtistRepo::with_backend(state.backend.clone());
    let aid = artistes
        .create(&tune_core::db::models::Artist::new("Miles Davis".into()))
        .expect("insert artiste");
    let pistes = tune_core::db::track_repo::TrackRepo::with_backend(state.backend.clone());
    let mut t = tune_core::db::models::Track::new("So What".into());
    t.artist_id = Some(aid);
    t.duration_ms = 545_000;
    let tid = pistes.create(&t).expect("insert piste");
    let relue = pistes.get(tid).expect("relire").expect("piste");
    assert_eq!(
        relue.artist_name.as_deref(),
        Some("Miles Davis"),
        "sans artiste la route s'arrêterait APRÈS la garde mesurée ici"
    );
    tid
}

fn lignes(texte: &str, marqueur: &str) -> usize {
    texte.lines().filter(|l| l.contains(marqueur)).count()
}

#[tokio::test(flavor = "current_thread")]
async fn le_reglage_lrclib_se_lit_et_son_refus_se_dit() {
    let capture = JournalCapture::default();
    let abonne = tracing_subscriber::fmt()
        .with_writer(capture.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::INFO)
        .finish();
    tracing::subscriber::set_global_default(abonne)
        .expect("ce binaire ne contient qu'un test : l'abonné global est libre");

    let (app, state) = app();
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());

    // ── Témoin 1 — la clé EXISTE dans la réponse, sur une base vierge ────
    //
    // ROUGE avant le correctif : `.get(CLE)` rendait `None`. Le client web ne
    // pouvait pas distinguer « éteint » de « pas lu », et son troisième état
    // (`null`, « pas établi ») devenait indiscernable du second.
    let (st, config) = get(&app, "/api/v1/system/config").await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(
        config.get(CLE),
        Some(&json!(false)),
        "sur une base vierge, GET /system/config doit PUBLIER le réglage à false, \
         pas omettre la clé — une clé absente se lit « je ne sais pas », pas « non »"
    );
    // Le voisin immédiat de la même section le fait depuis toujours : ce n'est
    // pas une convention inventée ici.
    assert_eq!(
        config.get("enrich_on_scan"),
        Some(&json!(true)),
        "le voisin de section publie bien son défaut"
    );

    // Et la fiche support dit la MÊME chose que la config — les deux surfaces
    // du même serveur ne peuvent plus diverger sur ce réglage (#3577 / #4051).
    let (st, profil) = get(&app, "/api/v1/system/profile").await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(
        profil.pointer(&format!("/settings/{CLE}")),
        Some(&json!(false)),
        "la fiche système publie le même défaut que /system/config"
    );

    // ── Témoin 3 (avant l'écriture) — le refus se DIT ────────────────────
    //
    // ROUGE avant : les deux routes rendaient `404 no_lyrics` sans une ligne
    // de journal, exactement comme un titre réellement sans paroles.
    let tid = inserer_piste(&state);
    let (st, corps) = get(&app, &format!("/api/v1/library/tracks/{tid}/lyrics")).await;
    assert_eq!(st, StatusCode::NOT_FOUND, "réglage éteint : 404 no_lyrics");
    assert_eq!(corps.get("error"), Some(&json!("no_lyrics")));

    let (st, _) = get(
        &app,
        "/api/v1/lyrics/by-meta?title=So%20What&artist=Miles%20Davis",
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND, "by-meta : même refus");

    let texte = capture.texte();
    assert_eq!(
        lignes(&texte, "paroles_recherche_en_ligne_desactivee"),
        2,
        "les DEUX routes paroles doivent nommer leur abstention — \
         `journalctl | grep -i lrc` ne départageait rien.\nJournal :\n{texte}"
    );
    assert!(
        texte.contains(CLE),
        "la ligne doit nommer le réglage en cause, pas seulement se plaindre.\n\
         Journal :\n{texte}"
    );

    // ── Témoin 2 — l'aller-retour complet ────────────────────────────────
    //
    // Exactement le corps que l'interface V1 envoie : un booléen JSON.
    let (st, reponse) = patch(&app, "/api/v1/system/config", json!({ CLE: true })).await;
    assert_eq!(st, StatusCode::OK, "le PATCH est accepté : {reponse}");

    assert_eq!(
        settings.get(CLE).expect("lecture base").as_deref(),
        Some("true"),
        "la ligne posée doit porter la CHAÎNE \"true\" — c'est ce que \
         `lrclib_consent_given` compare"
    );

    let (_, config) = get(&app, "/api/v1/system/config").await;
    assert_eq!(
        config.get(CLE),
        Some(&json!(true)),
        "la relecture rend le réglage posé"
    );

    // ── Témoin 4 — le journal nomme les clés, JAMAIS les valeurs ─────────
    //
    // L'écriture est désormais journalisée ; la table `settings` porte des
    // secrets. Cette garde est le prix de la précédente.
    const SECRET: &str = "valeur-parfaitement-reconnaissable-4051";
    let (st, _) = patch(
        &app,
        "/api/v1/system/config",
        json!({ "jwt_secret": SECRET }),
    )
    .await;
    assert_eq!(st, StatusCode::OK);

    let texte = capture.texte();
    assert!(
        texte.contains("reglages_ecrits"),
        "la porte d'écriture des réglages ne peut plus être muette : c'est ce \
         silence qui a rendu #4051 indiagnosticable.\nJournal :\n{texte}"
    );
    assert!(
        lignes(&texte, "reglages_ecrits") >= 2,
        "chaque PATCH qui pose une clé laisse sa ligne.\nJournal :\n{texte}"
    );
    assert!(
        texte.contains("jwt_secret"),
        "le NOM du réglage écrit est journalisé.\nJournal :\n{texte}"
    );
    assert!(
        !texte.contains(SECRET),
        "la VALEUR d'un réglage ne doit jamais entrer dans le journal \
         (cf. tests/cles_developpeur_hors_journal.rs).\nJournal :\n{texte}"
    );
}
