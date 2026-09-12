//! Une sortie PULL sert nos octets tels quels — le panneau doit le dire (#2189).
//!
//! ## Le fait
//!
//! Alex Campbell, Tune 0.9.98, Linux, sortie **HQPlayer**, fil 1524 :
//! « When playing local **or streaming** music files to HQPlayer, Tune is
//! reporting that it is transcoding. »
//!
//! Le « local **ou** streaming » est le point qui tranche : le symptôme est
//! INCONDITIONNEL, ce qu'aucune règle dépendant du format de la source ne
//! produirait.
//!
//! ## Le mécanisme
//!
//! `build_signal_path` (`tune-server/src/routes/zones.rs`) décidait le verdict
//! de transport dans un `match output_type` dont les bras nommaient
//! `dlna`/`openhome`, `oaat`, `airplay`, `chromecast`, `bluos`, `squeezebox`,
//! `browser` et `local`. **Tout le reste** tombait sur
//!
//! ```text
//! other => (false, other, format_name),
//! ```
//!
//! dont le premier membre est `transport_bit_perfect` — faux quoi qu'il
//! arrive — et dont le second devient le LIBELLÉ affiché, c'est-à-dire la
//! chaîne brute minuscule `"hqplayer"`.
//!
//! Or le chemin AUDIO dit exactement l'inverse : `pull_output_needs_dsp_
//! transcode` (`tune-core/src/orchestrator.rs`) range `hqplayer` dans la
//! famille des sorties PULL, et n'y force un transcodage QUE si un égaliseur,
//! une correction de pièce ou un ReplayGain est armé. Sans traitement, le
//! fichier part octet pour octet.
//!
//! Décideur et miroir répondaient donc à deux questions différentes sur la
//! même zone — la faute déjà commise en #3183.
//!
//! ## Ce que ce fichier cloue
//!
//! Le CORPS JSON rendu par `GET /api/v1/zones/{id}`, jamais la condition : un
//! test qui rejoue le `match` le recopie au lieu de le garder. Les zones sont
//! créées par la vraie route `POST /api/v1/zones` et la lecture est armée par
//! le vrai gestionnaire (`state.playback.play`).
//!
//! Trois paires, chacune avec son TÉMOIN — la même zone, le même fichier, un
//! égaliseur armé — parce que c'est là que le transcodage a vraiment lieu et
//! que le verdict doit vraiment tomber. Sans ce témoin, un correctif qui
//! dirait « bit-perfect » partout passerait au vert.
//!
//! ⚠️ `tune-server` porte `autotests = false` — ce fichier n'est compilé que
//! parce qu'il est déclaré dans l'agrégateur `server_contracts.rs`. Voir
//! `tests_orphelins.rs`.

use std::collections::BTreeMap;
use std::path::Path;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::playback::NowPlaying;
use tune_server::state::AppState;

fn app() -> (axum::Router, AppState) {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let router = tune_server::routes::router(state.clone());
    (router, state)
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

/// Crée une zone du type demandé par la VRAIE route, et rend son id.
async fn creer_zone(app: &axum::Router, nom: &str, output_type: &str) -> i64 {
    let corps = json!({
        "name": nom,
        "output_type": output_type,
        "output_device_id": format!("{output_type}-2189"),
    });
    let (status, body) = envoyer(
        app,
        Request::builder()
            .method("POST")
            .uri("/api/v1/zones")
            .header("Content-Type", "application/json")
            .body(Body::from(corps.to_string()))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "création de zone : {body}");
    body["id"].as_i64().expect("un id de zone")
}

/// Une piste FLAC 44,1 kHz / 16 bits en lecture — le cas d'Alex : un fichier
/// que rien n'oblige à transcoder, vers une sortie qui va le chercher.
async fn jouer_du_flac(state: &AppState, zone_id: i64) {
    state
        .playback
        .play(
            zone_id,
            NowPlaying {
                title: "Piste 2189".into(),
                source: "library".into(),
                format: Some("flac".into()),
                sample_rate: Some(44_100),
                bit_depth: Some(16),
                duration_ms: 300_000,
                ..Default::default()
            },
        )
        .await;
}

/// Un égaliseur ARMÉ sur la zone, écrit là où le chemin audio le lit
/// (`Orchestrator::zone_has_active_eq` et `active_zone_eq_profile`).
fn armer_l_eq(state: &AppState, zone_id: i64) {
    let profile = tune_core::audio::eq::EqProfile {
        enabled: true,
        bands: vec![tune_core::audio::eq::EqBandSpec {
            gain: 6.0,
            ..Default::default()
        }],
        ..Default::default()
    };
    SettingsRepo::with_backend(state.backend.clone())
        .set(
            &format!("zone_{zone_id}_eq_profile"),
            &serde_json::to_string(&profile).unwrap(),
        )
        .unwrap();
}

/// Le chemin du signal tel que la ROUTE le publie.
async fn chemin_du_signal(app: &axum::Router, zone_id: i64) -> Value {
    let (status, fiche) = envoyer(
        app,
        Request::get(format!("/api/v1/zones/{zone_id}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "GET /zones/{zone_id} : {fiche}");
    let sp = fiche["signal_path"].clone();
    assert!(
        !sp.is_null(),
        "GET /zones/{zone_id} ne publie aucun `signal_path` alors qu'une piste \
         est en lecture — le reste du fichier ne garderait rien. Fiche : {fiche}"
    );
    sp
}

/// La description de l'étape nommée, telle qu'elle part au client.
fn etape(sp: &Value, nom: &str) -> String {
    sp["steps"]
        .as_array()
        .unwrap_or_else(|| panic!("`steps` doit être un tableau : {sp}"))
        .iter()
        .find(|s| s["name"] == nom)
        .unwrap_or_else(|| panic!("étape « {nom} » absente du chemin : {sp}"))["description"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

/// ⭐ #2189 — une zone HQPlayer sur un FLAC 44,1/16 sans traitement.
///
/// Rien ne transcode : ni forçage réseau (HQPlayer n'est pas une sortie push),
/// ni égaliseur, ni ReplayGain, ni plafond de fréquence. Le verdict doit le
/// dire, et le transport doit porter un nom présentable.
#[tokio::test]
async fn hqplayer_sur_du_flac_sans_traitement_est_bit_perfect() {
    let (app, state) = app();
    let id = creer_zone(&app, "Salon HQPlayer", "hqplayer").await;
    jouer_du_flac(&state, id).await;

    let sp = chemin_du_signal(&app, id).await;

    assert_eq!(
        sp["bit_perfect"].as_bool(),
        Some(true),
        "une zone HQPlayer servie en FLAC 44,1/16 sans EQ ni ReplayGain reçoit \
         le fichier octet pour octet : le panneau annonçait un transcodage qui \
         n'a pas lieu (#2189, Alex Campbell, fil 1524). Chemin : {sp}"
    );
    assert_eq!(
        etape(&sp, "Transport"),
        "HQPlayer",
        "le bras par défaut rendait la chaîne BRUTE de la base comme nom de \
         transport : « hqplayer » en minuscules, là où toutes les autres \
         sorties affichent « DLNA/UPnP » ou « BluOS ». Chemin : {sp}"
    );
    let resume = sp["summary"].as_str().unwrap_or_default();
    assert!(
        !resume.contains("transcode"),
        "le résumé annonce encore un transcodage : « {resume} »"
    );
}

/// ⭐ LE TÉMOIN — la même zone, le même fichier, un égaliseur ARMÉ.
///
/// Là, `pull_output_needs_dsp_transcode` force pour de vrai le chemin
/// transcodé (c'est #1430) : le verdict DOIT tomber. Ce témoin est ce qui
/// interdit la correction paresseuse « bit-perfect partout ».
#[tokio::test]
async fn temoin_hqplayer_avec_un_eq_arme_nest_pas_bit_perfect() {
    let (app, state) = app();
    let id = creer_zone(&app, "Salon HQPlayer", "hqplayer").await;
    armer_l_eq(&state, id);
    jouer_du_flac(&state, id).await;

    let sp = chemin_du_signal(&app, id).await;

    assert_eq!(
        sp["bit_perfect"].as_bool(),
        Some(false),
        "un EQ armé sur une sortie PULL est réellement appliqué — \
         l'orchestrateur force le transcodage. Le verdict doit tomber. \
         Chemin : {sp}"
    );
    assert!(
        etape(&sp, "DSP").starts_with("EQ actif"),
        "l'étape DSP doit nommer le traitement qui a lieu : {sp}"
    );
}

/// ⭐ #2189 — même faute sur `slimproto`, et elle mordait plus fort.
///
/// `tune-core/src/slimproto/mod.rs` crée de VRAIES lignes de zone
/// (`get_or_create(&player_name, Some("slimproto"), …)`), et
/// `orchestrator::is_network_output_type` range déjà `slimproto` avec les
/// renderers réseau. Le panneau, lui, ne le connaissait pas : ni dans sa copie
/// de la liste réseau, ni dans les bras du `match`.
#[tokio::test]
async fn slimproto_sur_du_flac_sans_traitement_est_bit_perfect() {
    let (app, state) = app();
    let id = creer_zone(&app, "Chambre Squeezebox", "slimproto").await;
    jouer_du_flac(&state, id).await;

    let sp = chemin_du_signal(&app, id).await;

    assert_eq!(
        sp["bit_perfect"].as_bool(),
        Some(true),
        "le FLAC passe sans transcodage vers un lecteur Slimproto, exactement \
         comme vers une zone `squeezebox` — le panneau disait le contraire \
         (#2189). Chemin : {sp}"
    );
    assert_eq!(
        etape(&sp, "Transport"),
        "Slimproto",
        "le transport affichait la chaîne brute « slimproto ». Chemin : {sp}"
    );
}

/// ⭐ LE TÉMOIN de la paire Slimproto.
#[tokio::test]
async fn temoin_slimproto_avec_un_eq_arme_nest_pas_bit_perfect() {
    let (app, state) = app();
    let id = creer_zone(&app, "Chambre Squeezebox", "slimproto").await;
    armer_l_eq(&state, id);
    jouer_du_flac(&state, id).await;

    let sp = chemin_du_signal(&app, id).await;

    assert_eq!(
        sp["bit_perfect"].as_bool(),
        Some(false),
        "un EQ armé sur une zone Slimproto est appliqué : le verdict doit \
         tomber. Chemin : {sp}"
    );
}

/// `airplay2` tombait dans le même bras — mais son verdict, lui, était JUSTE.
///
/// Le protocole impose de l'ALAC 44,1/16 : la conversion a réellement lieu,
/// `false` est la bonne réponse. C'est le LIBELLÉ qui mentait, et une
/// correction qui déclarerait « bit-perfect » toute sortie hors bras nommé
/// aurait cassé ce cas-là. Ce test est donc la contre-épreuve du correctif.
#[tokio::test]
async fn airplay2_garde_son_verdict_et_gagne_son_nom() {
    let (app, state) = app();
    let id = creer_zone(&app, "Sonos Era 100", "airplay2").await;
    jouer_du_flac(&state, id).await;

    let sp = chemin_du_signal(&app, id).await;

    assert_eq!(
        sp["bit_perfect"].as_bool(),
        Some(false),
        "AirPlay 2 encode en ALAC 44,1/16 : le verdict `false` est vrai et ne \
         doit PAS être emporté par le correctif #2189. Chemin : {sp}"
    );
    assert_eq!(
        etape(&sp, "Transport"),
        "AirPlay 2",
        "le transport affichait la chaîne brute « airplay2 ». Chemin : {sp}"
    );
}

/// Le RECENSEMENT, figé.
///
/// La divergence entre le chemin audio et son miroir d'affichage s'est
/// reformée trois fois dans ce dépôt. Les relevés d'inventaire de ce même
/// dépôt disent pourquoi : « 2 annoncés / 8 réels ». Une liste écrite à la
/// main dans un commentaire est un plancher, pas une garde.
///
/// Ce test relit `tune-core/src/outputs/` et impose que l'ensemble des
/// `output_type()` déclarés soit EXACTEMENT celui recensé le 03/09/2026, avec
/// pour chacun la façon dont `build_signal_path` le traite. Ajouter une sortie
/// rend ce test rouge, et son auteur doit alors décider — plutôt que de
/// laisser la nouvelle valeur tomber en silence dans le fourre-tout, ce qui
/// est précisément ce qui est arrivé à `hqplayer`, `airplay2` et `slimproto`.
///
/// Même procédé que `tests_orphelins.rs` : on relit les sources du dépôt.
#[test]
fn le_recensement_des_types_de_sortie_est_complet() {
    // valeur → comment `build_signal_path` la traite aujourd'hui.
    let recensement: BTreeMap<&str, &str> = BTreeMap::from([
        ("airplay", "bras nommé « AirPlay »"),
        ("airplay2", "bras nommé « AirPlay 2 » (ajouté par #2189)"),
        ("bluos", "bras nommé « BluOS »"),
        ("chromecast", "bras nommé « Chromecast »"),
        ("dlna", "bras nommé « DLNA/UPnP »"),
        ("hqplayer", "famille PULL + libellé « HQPlayer » (#2189)"),
        // Ce recensement a été écrit à la main d'abord, et il lui manquait
        // celui-ci : un double de test (`LegacyNoopOutput`, dans
        // `capabilities_test.rs`). Il n'atteint aucune ligne de zone, mais son
        // absence de la liste écrite à la main est la démonstration même de
        // ce que ce test garde — une liste tenue à la main est un PLANCHER.
        ("legacy", "double de test, aucune ligne de zone"),
        ("local", "bras nommé, libellé = moteur audio"),
        ("oaat", "bras nommé « OAAT »"),
        (
            "oaat-multiroom",
            "famille PULL, libellé brut — aucun chemin connu n'écrit cette \
             valeur dans `zones.output_type` (le groupe est enregistré comme \
             SORTIE, la zone est créée à part). À classer si un jour elle y \
             arrive.",
        ),
        ("openhome", "bras nommé « DLNA/UPnP »"),
        (
            "slimproto",
            "bras « Squeezebox », libellé « Slimproto » (#2189)",
        ),
        ("squeezebox", "bras nommé « Squeezebox »"),
    ]);

    let outputs = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("tune-core")
        .join("src")
        .join("outputs");
    assert!(
        outputs.is_dir(),
        "{} introuvable — le garde-fou tournerait contre rien",
        outputs.display()
    );

    let mut trouves: BTreeMap<String, String> = BTreeMap::new();
    let mut dynamiques: Vec<String> = Vec::new();
    let mut a_visiter = vec![outputs.clone()];
    while let Some(dossier) = a_visiter.pop() {
        for entree in std::fs::read_dir(&dossier).expect("dossier outputs/ lisible") {
            let chemin = entree.expect("entrée lisible").path();
            if chemin.is_dir() {
                a_visiter.push(chemin);
                continue;
            }
            if chemin.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            let source = std::fs::read_to_string(&chemin).expect("source lisible");
            let lignes: Vec<&str> = source.lines().collect();
            for (i, ligne) in lignes.iter().enumerate() {
                if !ligne.contains("fn output_type(&self) -> &str") {
                    continue;
                }
                let corps = lignes.get(i + 1).map(|l| l.trim()).unwrap_or_default();
                let nom = chemin
                    .strip_prefix(&outputs)
                    .unwrap_or(&chemin)
                    .display()
                    .to_string();
                match corps.strip_prefix('"').and_then(|r| r.split_once('"')) {
                    Some((valeur, _)) => {
                        trouves.insert(valeur.to_string(), nom);
                    }
                    // `bridge.rs` et `mock.rs` rendent un champ : leur valeur
                    // vient de l'appelant, aucune constante à recenser.
                    None => dynamiques.push(nom),
                }
            }
        }
    }

    assert!(
        !trouves.is_empty(),
        "aucun `output_type()` lu dans {} — le lecteur du garde-fou est cassé",
        outputs.display()
    );

    let attendus: Vec<&str> = recensement.keys().copied().collect();
    let lus: Vec<&str> = trouves.keys().map(String::as_str).collect();
    assert_eq!(
        lus, attendus,
        "le recensement des `output_type()` de tune-core/src/outputs/ a bougé.\n\
         Lu     : {lus:?}\n\
         Attendu: {attendus:?}\n\
         Sources: {trouves:?}\n\
         Types à valeur dynamique (non recensables) : {dynamiques:?}\n\n\
         Une sortie AJOUTÉE ici tombe dans le bras par défaut de \
         `build_signal_path` (tune-server/src/routes/zones.rs) et y est \
         déclarée d'office selon la famille PULL, avec sa chaîne BRUTE pour \
         libellé. C'est exactement ce qui est arrivé à `hqplayer`, `airplay2` \
         et `slimproto` (#2189). Classez la nouvelle valeur, puis mettez ce \
         recensement à jour."
    );
}
