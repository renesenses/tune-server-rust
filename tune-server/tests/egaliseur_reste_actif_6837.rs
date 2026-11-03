//! #6837 (fil 2396) — « j'active l'égaliseur, je lance un morceau, je change
//! d'onglet, je lance un autre morceau, et l'onglet Égaliseur dit inactif ».
//!
//! ## Ce que ce fichier tient
//!
//! L'écran Égaliseur (`EqualizerV2.svelte`, client web) n'invente pas
//! « inactif » : au retour sur l'onglet, il relit `GET /zones/{id}/eq` et
//! affiche `enabled`. Ce témoin rejoue le signalement côté SERVEUR, par le
//! routeur réel, avec le corps exact que l'écran envoie à l'activation
//! (`save()` : la grille de 10 bandes, gains à 0, et `enabled: true`), puis
//! deux lectures de morceaux de la bibliothèque sur la même zone, puis un
//! redémarrage (un nouveau processus sur la même base).
//!
//! À chaque étape, la relecture que fait l'onglet doit dire « actif ».
//!
//! ## Ce qu'il ne tient pas
//!
//! Le correctif de #6924 (déjà dans le lot) change la fiche du GREFFON
//! (`GET /plugins/equalizer`, champ `enabled`), que l'écran Égaliseur ne lit
//! pas pour son interrupteur : il ne peut ni causer ni régler #6837. Ce
//! témoin vérifie seulement que le serveur garde l'égaliseur actif de la
//! zone ; les causes restantes (l'écran relit une autre zone, ou l'activation
//! a été refusée sans que l'écran le montre) sont du côté du client web.
//!
//! ⚠️ `tune-server` porte `autotests = false` : ce fichier n'existe pour
//! `cargo test` que par son bloc `[[test]]` du manifeste.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::db::models::Track;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::db::track_repo::TrackRepo;
use tune_core::db::zone_repo::ZoneRepo;
use tune_server::state::AppState;

/// La grille graphique de 10 bandes de l'écran Égaliseur (`GRIDS[10]`), Q = 1.
const GRILLE_10: [f64; 10] = [
    31.0, 63.0, 125.0, 250.0, 500.0, 1000.0, 2000.0, 4000.0, 8000.0, 16000.0,
];

/// Un WAV PCM 16 bits stéréo de 0,1 s de silence : une vraie piste lisible.
fn ecrire_wav(chemin: &std::path::Path) {
    let (debit, canaux, bits) = (44_100u32, 2u16, 16u16);
    let octets_par_trame = u32::from(canaux) * u32::from(bits / 8);
    let donnees = debit / 10 * octets_par_trame;
    let mut wav = Vec::new();
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + donnees).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&canaux.to_le_bytes());
    wav.extend_from_slice(&debit.to_le_bytes());
    wav.extend_from_slice(&(debit * octets_par_trame).to_le_bytes());
    wav.extend_from_slice(&(octets_par_trame as u16).to_le_bytes());
    wav.extend_from_slice(&bits.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&donnees.to_le_bytes());
    wav.resize(wav.len() + donnees as usize, 0);
    std::fs::write(chemin, wav).unwrap();
}

/// Un serveur Premium, l'égaliseur installé et actif (l'état de #6924), sur
/// la base `base`.
async fn serveur(base: &str) -> (axum::Router, AppState) {
    let state = AppState::new(base, 0, Default::default()).unwrap();
    state.license.set_account_premium(true, None).await;
    let s = SettingsRepo::with_backend(state.backend.clone());
    s.set("plugin_equalizer_installed", "true").unwrap();
    s.set("plugin_equalizer_enabled", "true").unwrap();
    (tune_server::routes::router(state.clone()), state)
}

async fn appel(
    app: &axum::Router,
    methode: &str,
    uri: String,
    corps: Value,
) -> (StatusCode, Value) {
    let requete = Request::builder()
        .method(methode)
        .uri(uri)
        .header("content-type", "application/json");
    let requete = if methode == "GET" {
        requete.body(Body::empty())
    } else {
        requete.body(Body::from(corps.to_string()))
    }
    .unwrap();
    let res = app.clone().oneshot(requete).await.unwrap();
    let statut = res.status();
    let octets = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        statut,
        serde_json::from_slice(&octets).unwrap_or(Value::Null),
    )
}

/// Ce que relit l'onglet Égaliseur en revenant : `GET /zones/{id}/eq`.
async fn relecture_de_l_onglet(app: &axum::Router, zone: i64) -> Value {
    let (statut, eq) = appel(app, "GET", format!("/api/v1/zones/{zone}/eq"), Value::Null).await;
    assert_eq!(statut, StatusCode::OK, "{eq}");
    eq
}

fn piste(state: &AppState, titre: &str, fichier: &std::path::Path) -> i64 {
    ecrire_wav(fichier);
    let mut t = Track::new(titre.into());
    t.format = Some("wav".into());
    t.file_path = Some(fichier.to_string_lossy().into_owned());
    t.duration_ms = 100;
    TrackRepo::with_backend(state.backend.clone())
        .create(&t)
        .expect("insertion de piste")
}

#[tokio::test]
async fn temoin_6837_l_egaliseur_active_reste_actif_apres_deux_lectures_et_un_redemarrage() {
    let dossier = tempfile::tempdir().unwrap();
    let base = dossier.path().join("tune.db");
    let base = base.to_string_lossy();
    let (app, state) = serveur(&base).await;
    let zone = ZoneRepo::with_backend(state.backend.clone())
        .create("Salon", Some("local"), Some("local:tune-6837-absente"))
        .unwrap();
    let premiere = piste(&state, "Premier morceau", &dossier.path().join("a.wav"));
    let seconde = piste(&state, "Second morceau", &dossier.path().join("b.wav"));

    // 1. « J'active l'égaliseur » : le corps de `save()` après `toggle()`.
    let bandes: Vec<Value> = GRILLE_10
        .iter()
        .map(|f| json!({"freq": f, "gain": 0.0, "q": 1.0}))
        .collect();
    let (statut, reponse) = appel(
        &app,
        "POST",
        format!("/api/v1/zones/{zone}/eq"),
        json!({"bands": bandes, "enabled": true}),
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "activation refusée : {reponse}");
    let eq = relecture_de_l_onglet(&app, zone).await;
    assert_eq!(eq["enabled"], true, "actif dès l'activation : {eq}");

    // 2. « Je lance un morceau », puis « je change d'onglet et je lance un
    // nouveau morceau » : deux lectures de la bibliothèque sur la zone.
    // Le son n'est pas mesuré ici (pas de carte son) : on n'exige que la
    // route, pas le succès de la sortie.
    for (rang, track_id) in [premiere, seconde].into_iter().enumerate() {
        let (statut, corps) = appel(
            &app,
            "POST",
            format!("/api/v1/zones/{zone}/play"),
            json!({"track_id": track_id}),
        )
        .await;
        assert_ne!(
            statut,
            StatusCode::NOT_FOUND,
            "la route de lecture : {corps}"
        );
        let eq = relecture_de_l_onglet(&app, zone).await;
        assert_eq!(
            eq["enabled"],
            true,
            "égaliseur activé mais relu « inactif » après la lecture n° {} : {eq}",
            rang + 1
        );
        assert_eq!(
            eq["bands"].as_array().map(Vec::len),
            Some(10),
            "la courbe activée a perdu ses bandes à la lecture n° {} : {eq}",
            rang + 1
        );
    }

    // 3. Redémarrage : un nouveau processus sur la même base (le cas de
    // #6924). L'égaliseur de la zone reste actif.
    drop(app);
    let (app, _state) = serveur(&base).await;
    let eq = relecture_de_l_onglet(&app, zone).await;
    assert_eq!(
        eq["enabled"], true,
        "égaliseur activé mais relu « inactif » après le redémarrage : {eq}"
    );
}
