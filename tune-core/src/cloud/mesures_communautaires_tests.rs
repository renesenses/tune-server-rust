//! #5594 — les mesures communautaires, éprouvées contre un FAUX site local
//! qui suit le contrat de `site-mozaiklabs#338`. Aucun appel au vrai site.
//!
//! Chaque test passe par [`passe`], le seul chemin que la tâche de fond
//! appelle, et lit ce que le faux site a REÇU.

use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use super::*;
use crate::db::migrations;
use crate::db::sqlite::SqliteDb;

const INSTANCE: &str = "6f1c0a52-8d3e-4b7a-9c11-2a4f5e6d7c8b";
/// Deux clés du seau `ab`, une du seau `cd`.
const CLE_A: &str = "flac-md5-v1:ab3f0123456789abcdef0123456789ab:11025000:44100:2:16";
const CLE_B: &str = "flac-md5-v1:ab00ffeeddccbbaa99887766554433aa:8820000:44100:2:16";
const CLE_C: &str = "flac-md5-v1:cd11223344556677889900aabbccddee:9600000:96000:2:24";

fn base() -> Arc<dyn DbBackend> {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    migrations::run_migrations(&db).unwrap();
    Arc::new(db)
}

fn reglages(b: &Arc<dyn DbBackend>) -> SettingsRepo {
    SettingsRepo::with_backend(b.clone())
}

fn piste(b: &Arc<dyn DbBackend>, id: i64, cle: &str) {
    let chemin = format!("/srv/musique/secret/piste-{id}.flac");
    let titre = format!("Titre secret {id}");
    let cle = cle.to_string();
    b.execute(
        "INSERT INTO tracks (id, title, file_path, format, source, audio_pcm_key) \
         VALUES (?, ?, ?, 'flac', 'local', ?)",
        &[
            &id as &dyn ToSqlValue,
            &titre as &dyn ToSqlValue,
            &chemin as &dyn ToSqlValue,
            &cle as &dyn ToSqlValue,
        ],
    )
    .unwrap();
}

fn meta(b: &Arc<dyn DbBackend>, id: i64, cle: &str, valeur: &str) {
    TrackMetadataRepo::with_backend(b.clone())
        .set(id, cle, valeur)
        .unwrap();
}

fn vecteur(premier: f32) -> Vec<f32> {
    let mut v = vec![0.0f32; EMBED_DIM];
    v[0] = premier;
    v[1] = 1.0;
    v
}

fn empreinte(b: &Arc<dyn DbBackend>, id: i64, source: Option<&str>) {
    let blob = Some(to_bytes(&vecteur(0.5)));
    let source = source.map(str::to_string);
    b.execute(
        "INSERT INTO track_audio_embedding (track_id, model, embedding, analyzed_at, source) \
         VALUES (?, ?, ?, 1, ?)",
        &[
            &id as &dyn ToSqlValue,
            &MODEL_ID as &dyn ToSqlValue,
            &blob as &dyn ToSqlValue,
            &source as &dyn ToSqlValue,
        ],
    )
    .unwrap();
}

/// Une piste MESURÉE par Tune : gain, crête, plage dynamique, empreinte.
fn mesuree(b: &Arc<dyn DbBackend>, id: i64, cle: &str) {
    piste(b, id, cle);
    meta(b, id, "rg_track_gain", "-7.25 dB");
    meta(b, id, "rg_track_peak", "0.950000");
    meta(b, id, "rg_track_source", "analysis");
    meta(b, id, "rg_algo", RG_ALGO);
    meta(b, id, "dr_track", "9");
    meta(b, id, "dr_source", "analysis");
    meta(b, id, "dr_algo", DR_ALGO);
    empreinte(b, id, None);
}

#[derive(Default)]
struct FauxSite {
    /// `(méthode, chemin, requête, corps)` de CHAQUE appel reçu.
    appels: Vec<(String, String, String, Value)>,
    /// Interrupteur du site fermé : 503 partout sauf l'effacement.
    ferme: bool,
    /// Lignes servies à la lecture.
    servies: Vec<Value>,
}

type Site = Arc<Mutex<FauxSite>>;

async fn faux_site() -> (String, Site) {
    use axum::extract::{RawQuery, State};
    use axum::http::{Method, StatusCode, Uri};
    use axum::response::IntoResponse;
    use axum::routing::{any, post};
    use axum::{Json, Router};

    async fn mesures(
        State(s): State<Site>,
        methode: Method,
        uri: Uri,
        RawQuery(q): RawQuery,
        corps: String,
    ) -> axum::response::Response {
        let corps: Value = serde_json::from_str(&corps).unwrap_or(Value::Null);
        let mut s = s.lock().unwrap();
        s.appels.push((
            methode.to_string(),
            uri.path().to_string(),
            q.clone().unwrap_or_default(),
            corps.clone(),
        ));
        if s.ferme {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error": "community_measures_disabled"})),
            )
                .into_response();
        }
        if methode == Method::POST {
            let n: usize = corps["measures"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .map(|m| {
                            ["rg", "dr", "clap"]
                                .iter()
                                .filter(|k| m.get(**k).is_some())
                                .count()
                        })
                        .sum()
                })
                .unwrap_or(0);
            return Json(json!({"status": "ok", "accepted": n})).into_response();
        }
        let prefixe = q
            .unwrap_or_default()
            .split('&')
            .find_map(|kv| kv.strip_prefix("prefix=").map(str::to_string))
            .unwrap_or_default();
        let lignes: Vec<Value> = s
            .servies
            .iter()
            .filter(|l| {
                l["audio_pcm_key"]
                    .as_str()
                    .is_some_and(|k| k[12..].starts_with(&prefixe))
            })
            .cloned()
            .collect();
        Json(json!({"prefix": prefixe, "measures": lignes, "next": null})).into_response()
    }
    async fn effacer(State(s): State<Site>, Json(corps): Json<Value>) -> Json<Value> {
        s.lock().unwrap().appels.push((
            "POST".into(),
            "/api/v1/community/measures/erase".into(),
            String::new(),
            corps,
        ));
        Json(json!({"status": "erased", "contributions": 3}))
    }
    let etat: Site = Arc::new(Mutex::new(FauxSite::default()));
    let app = Router::new()
        .route("/api/v1/community/measures/erase", post(effacer))
        .route("/api/v1/community/measures", any(mesures))
        // Toute autre route compte aussi : rien ne doit partir ailleurs.
        .fallback(any(mesures))
        .with_state(etat.clone());
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let adresse = ecoute.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(ecoute, app).await.ok();
    });
    (format!("http://{adresse}"), etat)
}

fn http() -> reqwest::Client {
    crate::http::client::shared().clone()
}

fn brancher(b: &Arc<dyn DbBackend>, url: &str) {
    let s = reglages(b);
    s.set("mozaik_base_url", url).unwrap();
    s.set("instance_id", INSTANCE).unwrap();
}

/// 🔴 Le témoin de l'opt-in : une bibliothèque toute mesurée, un site
/// joignable, et AUCUNE option — aucun appel, d'aucune sorte.
#[tokio::test]
async fn rien_ne_part_sans_option_explicite() {
    let (url, site) = faux_site().await;
    let b = base();
    brancher(&b, &url);
    mesuree(&b, 1, CLE_A);
    piste(&b, 2, CLE_B);

    let bilan = passe(&b, &http()).await;
    assert_eq!(bilan.appels, 0, "{bilan:?}");
    assert!(
        site.lock().unwrap().appels.is_empty(),
        "sans option explicite, rien ne doit partir : {:?}",
        site.lock().unwrap().appels
    );
    assert!(!reception_activee(&reglages(&b)));
    assert!(!contribution_activee(&reglages(&b)));
}

/// Le dépôt ne porte que les MESURES de Tune, de la version courante, et
/// aucune identité : ni chemin, ni titre, ni compte.
#[tokio::test]
async fn le_depot_ne_porte_que_les_mesures_et_aucune_identite() {
    let (url, site) = faux_site().await;
    let b = base();
    brancher(&b, &url);
    reglages(&b).set(CLE_CONTRIBUTION, "true").unwrap();
    mesuree(&b, 1, CLE_A);
    // Un gain lu dans les TAGS, et une vieille version de DR : rien à envoyer.
    piste(&b, 2, CLE_B);
    meta(&b, 2, "rg_track_gain", "-3.00 dB");
    meta(&b, 2, "rg_track_peak", "0.800000");
    meta(&b, 2, "rg_track_source", "tag");
    meta(&b, 2, "dr_track", "11");
    meta(&b, 2, "dr_source", "analysis");
    meta(&b, 2, "dr_algo", "tt-dr-v0");
    // Une empreinte REÇUE de la communauté : elle ne repart jamais.
    piste(&b, 3, CLE_C);
    empreinte(&b, 3, Some(SOURCE_COMMUNAUTE));

    let bilan = passe(&b, &http()).await;
    assert_eq!(bilan.echec, None, "{bilan:?}");
    let appels = site.lock().unwrap().appels.clone();
    assert_eq!(appels.len(), 1, "un seul dépôt : {appels:?}");
    let (methode, chemin, _, corps) = &appels[0];
    assert_eq!((methode.as_str(), chemin.as_str()), ("POST", CHEMIN_API));

    let cles: Vec<&String> = corps.as_object().unwrap().keys().collect();
    assert_eq!(cles, ["instance_id", "measures"], "{corps}");
    assert_eq!(corps["instance_id"], INSTANCE);
    let elements = corps["measures"].as_array().unwrap();
    assert_eq!(elements.len(), 1, "seule la piste mesurée part : {corps}");
    let e = &elements[0];
    assert_eq!(e["audio_pcm_key"], CLE_A);
    assert_eq!(
        e["rg"],
        json!({"algo": RG_ALGO, "source": "analysis", "track_gain_db": -7.25, "track_peak": 0.95})
    );
    assert_eq!(
        e["dr"],
        json!({"algo": DR_ALGO, "source": "analysis", "value": 9})
    );
    assert_eq!(e["clap"]["model_id"], MODEL_ID);
    let octets = base64::engine::general_purpose::STANDARD
        .decode(e["clap"]["embedding"].as_str().unwrap())
        .unwrap();
    assert_eq!(octets.len(), EMBED_DIM * 4);

    let texte = corps.to_string();
    for interdit in ["/srv/musique", "secret", "Titre", "file_path", "title"] {
        assert!(
            !texte.contains(interdit),
            "« {interdit} » est parti : {texte}"
        );
    }
    assert_eq!(
        reglages(&b)
            .get(CLE_CONTRIBUTION_TRANSMISE)
            .unwrap()
            .as_deref(),
        Some("true")
    );

    // Deux passages de plus : la marque posée, rien n'est renvoyé.
    passe(&b, &http()).await;
    passe(&b, &http()).await;
    assert_eq!(
        site.lock().unwrap().appels.len(),
        1,
        "une mesure envoyée ne repart pas"
    );

    // Une nouvelle mesure sur une autre piste part au tour suivant.
    mesuree(&b, 4, CLE_C.replace(":cd11", ":cd99").as_str());
    passe(&b, &http()).await;
    passe(&b, &http()).await;
    let appels = site.lock().unwrap().appels.clone();
    assert_eq!(appels.len(), 2, "{appels:?}");
    assert_eq!(appels[1].3["measures"].as_array().unwrap().len(), 1);
}

/// Un refus de télémétrie coupe la contribution même cochée (#3383).
#[tokio::test]
async fn le_refus_de_telemetrie_coupe_la_contribution() {
    let (url, site) = faux_site().await;
    let b = base();
    brancher(&b, &url);
    mesuree(&b, 1, CLE_A);
    let s = reglages(&b);
    s.set(CLE_CONTRIBUTION, "true").unwrap();
    s.set(crate::cloud::telemetry::TELEMETRY_SETTING_KEY, "false")
        .unwrap();
    passe(&b, &http()).await;
    assert!(site.lock().unwrap().appels.is_empty());
}

/// Interrupteur du site fermé (503) : rien n'avance, rien n'est noté transmis.
#[tokio::test]
async fn le_site_ferme_n_avance_rien() {
    let (url, site) = faux_site().await;
    site.lock().unwrap().ferme = true;
    let b = base();
    brancher(&b, &url);
    reglages(&b).set(CLE_CONTRIBUTION, "true").unwrap();
    mesuree(&b, 1, CLE_A);
    passe(&b, &http()).await;
    assert_eq!(site.lock().unwrap().appels.len(), 1);
    let s = reglages(&b);
    assert_eq!(s.get(CLE_CONTRIBUTION_TRANSMISE).unwrap(), None);
    assert_eq!(s.get(CLE_CURSEUR_ENVOI).unwrap(), None);
    // Rouvert : la même mesure part.
    site.lock().unwrap().ferme = false;
    passe(&b, &http()).await;
    assert_eq!(site.lock().unwrap().appels.len(), 2);
    assert_eq!(
        s.get(CLE_CONTRIBUTION_TRANSMISE).unwrap().as_deref(),
        Some("true")
    );
}

/// Retirer l'option « contribuer » après un envoi fait appeler l'effacement,
/// avec le seul `instance_id`.
#[tokio::test]
async fn le_retrait_de_la_contribution_appelle_l_effacement() {
    let (url, site) = faux_site().await;
    let b = base();
    brancher(&b, &url);
    mesuree(&b, 1, CLE_A);
    let s = reglages(&b);
    s.set(CLE_CONTRIBUTION, "true").unwrap();
    passe(&b, &http()).await;
    assert_eq!(site.lock().unwrap().appels.len(), 1);

    s.set(CLE_CONTRIBUTION, "false").unwrap();
    let bilan = passe(&b, &http()).await;
    assert!(bilan.efface, "{bilan:?}");
    let appels = site.lock().unwrap().appels.clone();
    assert_eq!(appels.len(), 2, "{appels:?}");
    assert_eq!(appels[1].1, "/api/v1/community/measures/erase");
    assert_eq!(appels[1].3, json!({"instance_id": INSTANCE}));
    assert_eq!(
        s.get(CLE_CONTRIBUTION_TRANSMISE).unwrap().as_deref(),
        Some("false")
    );
    // Une fois effacé, plus rien ne part.
    passe(&b, &http()).await;
    assert_eq!(site.lock().unwrap().appels.len(), 2);
}

/// La réception lit des SEAUX de deux chiffres, ne comble que les VIDES, ne
/// retient que notre version et le quorum, et marque `community`.
#[tokio::test]
async fn la_reception_comble_les_vides_sans_rien_remplacer() {
    let (url, site) = faux_site().await;
    let b = base();
    brancher(&b, &url);
    reglages(&b).set(CLE_RECEPTION, "true").unwrap();
    // A : rien. B : un gain de TAG et un DR de tag. C : autre seau.
    piste(&b, 1, CLE_A);
    piste(&b, 2, CLE_B);
    meta(&b, 2, "rg_track_gain", "-3.00 dB");
    meta(&b, 2, "rg_track_source", "tag");
    meta(&b, 2, "dr_track", "12");
    meta(&b, 2, "dr_source", "tag");
    piste(&b, 3, CLE_C);
    let embedding = base64::engine::general_purpose::STANDARD.encode(to_bytes(&vecteur(3.0)));
    site.lock().unwrap().servies = vec![
        json!({"audio_pcm_key": CLE_A, "kind": "rg", "algo": RG_ALGO,
               "track_gain_db": -7.4, "track_peak": 0.97, "installations": 2}),
        json!({"audio_pcm_key": CLE_A, "kind": "dr", "algo": DR_ALGO, "dr": 9, "installations": 2}),
        json!({"audio_pcm_key": CLE_A, "kind": "clap", "algo": MODEL_ID,
               "embedding": embedding, "installations": 3}),
        json!({"audio_pcm_key": CLE_B, "kind": "rg", "algo": RG_ALGO,
               "track_gain_db": -9.0, "track_peak": 0.5, "installations": 2}),
        json!({"audio_pcm_key": CLE_B, "kind": "dr", "algo": DR_ALGO, "dr": 5, "installations": 2}),
        // Une autre version, et une seule installation : écartées.
        json!({"audio_pcm_key": CLE_C, "kind": "rg", "algo": "bs1770-tp4x-v1",
               "track_gain_db": -1.0, "track_peak": 0.5, "installations": 2}),
        json!({"audio_pcm_key": CLE_C, "kind": "dr", "algo": DR_ALGO, "dr": 7, "installations": 1}),
    ];

    let bilan = passe(&b, &http()).await;
    assert_eq!(bilan.echec, None, "{bilan:?}");
    assert_eq!(bilan.recues, 3, "{bilan:?}");

    let appels = site.lock().unwrap().appels.clone();
    for (methode, chemin, requete, _) in &appels {
        assert_eq!((methode.as_str(), chemin.as_str()), ("GET", CHEMIN_API));
        let prefixe = requete
            .split('&')
            .find_map(|kv| kv.strip_prefix("prefix="))
            .unwrap();
        assert_eq!(prefixe.len(), 2, "un seau de deux chiffres : {requete}");
        assert!(
            requete.split('&').all(|kv| kv.starts_with("prefix=")
                || kv.starts_with("kinds=")
                || kv.starts_with("after=")),
            "rien d'autre ne part : {requete}"
        );
    }
    assert_eq!(appels.len(), 2, "seaux ab et cd : {appels:?}");

    let m = TrackMetadataRepo::with_backend(b.clone());
    let a = m.get_all(1).unwrap();
    assert_eq!(a.get("rg_track_gain").map(String::as_str), Some("-7.40 dB"));
    assert_eq!(a.get("rg_track_peak").map(String::as_str), Some("0.970000"));
    assert_eq!(
        a.get("rg_track_source").map(String::as_str),
        Some(SOURCE_COMMUNAUTE)
    );
    assert_eq!(a.get("rg_algo").map(String::as_str), Some(RG_ALGO));
    assert_eq!(a.get("dr_track").map(String::as_str), Some("9"));
    assert_eq!(
        a.get("dr_source").map(String::as_str),
        Some(SOURCE_COMMUNAUTE)
    );
    assert_eq!(
        a.get("audio_embed_analyzed").map(String::as_str),
        Some(MODEL_ID)
    );
    assert_eq!(
        crate::audio::replaygain::stored_gain_source(
            &b,
            1,
            crate::audio::replaygain::ReplayGainMode::Track
        ),
        Some(crate::audio::replaygain::GainSource::Community),
        "le chemin du signal doit dire « communauté », pas « tags du fichier »"
    );
    let source: Option<String> = b
        .query_one(
            "SELECT source FROM track_audio_embedding WHERE track_id = 1",
            &[],
        )
        .unwrap()
        .and_then(|r| r.first().and_then(|v| v.as_string()));
    assert_eq!(source.as_deref(), Some(SOURCE_COMMUNAUTE));

    // Le tag de B fait foi : rien n'est remplacé.
    let bb = m.get_all(2).unwrap();
    assert_eq!(
        bb.get("rg_track_gain").map(String::as_str),
        Some("-3.00 dB")
    );
    assert_eq!(bb.get("dr_track").map(String::as_str), Some("12"));
    // C : version étrangère et quorum manquant, rien.
    let c = m.get_all(3).unwrap();
    assert!(
        !c.contains_key("rg_track_gain") && !c.contains_key("dr_track"),
        "{c:?}"
    );
}

#[test]
fn la_cle_pcm_suit_la_regle_du_site() {
    assert!(cle_pcm_valide(CLE_A));
    for mauvaise in [
        "flac-md5-v1:00000000000000000000000000000000:1:44100:2:16",
        "flac-md5-v1:AB3F0123456789ABCDEF0123456789AB:1:44100:2:16",
        "flac-md5-v1:ab3f0123456789abcdef0123456789ab:0:44100:2:16",
        "flac-md5-v1:ab3f0123456789abcdef0123456789ab:1:44100:9:16",
        "flac-md5-v1:ab3f0123456789abcdef0123456789ab:1:44100:2:33",
        "flac-md5-v1:ab3f0123456789abcdef0123456789ab:1:44100:2:3",
        "mbid:ab3f0123456789abcdef0123456789ab:1:44100:2:16",
        "flac-md5-v1:ab3f:1:44100:2:16",
    ] {
        assert!(!cle_pcm_valide(mauvaise), "{mauvaise}");
    }
}
