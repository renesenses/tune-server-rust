//! Les gardes de l'Assistant IA, par ses ROUTES, avec un FAUX fournisseur —
//! aucun appel réseau réel. Le seul trafic est local : un faux serveur
//! « Anthropic » sur 127.0.0.1, pour éprouver le vrai client HTTP.

use std::io::Write;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_assistant_ia::fournisseur::{
    Acces, Demande, ErreurFournisseur, Fabrique, Fournisseur, Reponse, fabrique_reelle,
};
use tune_assistant_ia::reglages::{CLE_API_CHIFFREE, CLE_URL};
use tune_assistant_ia::{CritereMoteur, Etat, router_vers};
use tune_core::db::backend::DbBackend;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::db::sqlite::SqliteDb;

const CLE: &str = "sk-ant-api03-TRES-SECRETE-0123456789-wxyz";

fn criteres() -> Vec<CritereMoteur> {
    tune_smart_http::criteres::criteres_exposes()
        .into_iter()
        .map(|c| CritereMoteur {
            collection: c.collection.map(String::from),
            piste: c.piste.map(String::from),
            famille: c.famille.to_string(),
            operateurs: c.operateurs.iter().map(|s| s.to_string()).collect(),
        })
        .collect()
}

fn base() -> Arc<dyn DbBackend> {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    tune_core::db::migrations::run_migrations(&db).unwrap();
    db.execute_batch(
        "INSERT INTO artists (id, name, musicbrainz_id) VALUES (2, 'Miles Davis', 'mb-artist-miles');
         INSERT INTO albums (id, title, artist_id, genre, musicbrainz_release_id)
                VALUES (2, 'Kind of Blue', 2, 'Jazz', 'mb-release-kob');
         INSERT INTO albums (id, title, artist_id, genre) VALUES (3, 'Sketches', 2, NULL);",
    )
    .unwrap();
    Arc::new(db)
}

/// Le faux fournisseur : il compte ses appels et rend un texte fixé.
#[derive(Clone)]
struct Faux {
    appels: Arc<AtomicUsize>,
    texte: Arc<Mutex<String>>,
    jetons_entree: u64,
}

#[async_trait]
impl Fournisseur for Faux {
    async fn completer(&self, _d: &Demande) -> Result<Reponse, ErreurFournisseur> {
        self.appels.fetch_add(1, Ordering::SeqCst);
        Ok(Reponse {
            texte: self.texte.lock().unwrap().clone(),
            jetons_entree: self.jetons_entree,
            jetons_sortie: 100,
        })
    }
}

struct Banc {
    app: axum::Router,
    db: Arc<dyn DbBackend>,
    appels: Arc<AtomicUsize>,
    texte: Arc<Mutex<String>>,
    _dossier: tempfile::TempDir,
}

fn banc_avec(fabrique: Option<Fabrique>, jetons_entree: u64) -> Banc {
    let db = base();
    let dossier = tempfile::tempdir().unwrap();
    let appels = Arc::new(AtomicUsize::new(0));
    let texte = Arc::new(Mutex::new(String::new()));
    let faux = Faux {
        appels: appels.clone(),
        texte: texte.clone(),
        jetons_entree,
    };
    let fabrique = fabrique.unwrap_or_else(|| {
        Arc::new(move |_a: Acces| -> Box<dyn Fournisseur> { Box::new(faux.clone()) })
    });
    let etat = Etat::new(
        db.clone(),
        dossier.path(),
        dossier.path().join("cache"),
        criteres(),
        fabrique,
    );
    Banc {
        app: router_vers(etat),
        db,
        appels,
        texte,
        _dossier: dossier,
    }
}

fn banc() -> Banc {
    banc_avec(None, 1_000)
}

async fn appel(
    app: &axum::Router,
    methode: &str,
    uri: &str,
    corps: Option<Value>,
) -> (StatusCode, Value) {
    let mut req = Request::builder().method(methode).uri(uri);
    let body = match corps {
        Some(c) => {
            req = req.header("content-type", "application/json");
            Body::from(c.to_string())
        }
        None => Body::empty(),
    };
    let rep = app.clone().oneshot(req.body(body).unwrap()).await.unwrap();
    let statut = rep.status();
    let octets = axum::body::to_bytes(rep.into_body(), 1 << 20)
        .await
        .unwrap();
    let v = serde_json::from_slice(&octets).unwrap_or(Value::Null);
    (statut, v)
}

async fn configurer(b: &Banc, plafond: &str) {
    let (s, _) = appel(
        &b.app,
        "PUT",
        "/reglages",
        Some(json!({"fournisseur": "anthropic", "cle": CLE, "plafond_eur": plafond})),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
}

fn reponse_playlist() -> String {
    json!({
        "name": "Jazz calme",
        "match_mode": "all",
        "rules": [
            {"field": "genre", "op": "contains", "value": "Jazz"},
            {"field": "year", "op": "between", "value": [1960, 1969]},
            {"field": "ambiance", "op": "=", "value": "calme"}
        ],
        "untranslated": ["sans les live"]
    })
    .to_string()
}

// ---------------------------------------------------------------------------

#[tokio::test]
async fn une_regle_inconnue_est_refusee_par_la_route() {
    let b = banc();
    configurer(&b, "5").await;
    let avant = nombre_de_smart_playlists(&b.db);
    *b.texte.lock().unwrap() = reponse_playlist();
    let (s, v) = appel(
        &b.app,
        "POST",
        "/playlist",
        Some(json!({"phrase": "jazz calme des années 60 sans les live"})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let p = &v["proposition"];
    let champs: Vec<&str> = p["rules"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["field"].as_str().unwrap())
        .collect();
    assert_eq!(champs, vec!["genre", "year"]);
    assert_eq!(p["refusees"][0]["regle"]["field"], "ambiance");
    assert_eq!(p["refusees"][0]["raison"], "critere_inconnu");
    // Rien n'est créé : le nombre de playlists intelligentes n'a pas bougé
    // (les migrations en posent quelques-unes par défaut).
    assert_eq!(nombre_de_smart_playlists(&b.db), avant);
}

fn nombre_de_smart_playlists(db: &Arc<dyn DbBackend>) -> i64 {
    db.query_one("SELECT COUNT(*) FROM smart_playlists", &[])
        .unwrap()
        .and_then(|l| l.first().and_then(|v| v.as_i64()))
        .unwrap()
}

#[tokio::test]
async fn le_cache_evite_un_second_appel() {
    let b = banc();
    configurer(&b, "5").await;
    *b.texte.lock().unwrap() = reponse_playlist();
    let (s1, v1) = appel(
        &b.app,
        "POST",
        "/playlist",
        Some(json!({"phrase": "Jazz calme des années 60"})),
    )
    .await;
    // Même question, autre écriture : espaces et casse ne comptent pas.
    let (s2, v2) = appel(
        &b.app,
        "POST",
        "/playlist",
        Some(json!({"phrase": "  jazz CALME   des années 60 "})),
    )
    .await;
    assert_eq!((s1, s2), (StatusCode::OK, StatusCode::OK));
    assert_eq!(
        b.appels.load(Ordering::SeqCst),
        1,
        "le second appel a été payé"
    );
    assert_eq!(v1["depuis_cache"], false);
    assert_eq!(v2["depuis_cache"], true);
    assert_eq!(v1["proposition"], v2["proposition"]);
    let (_, r) = appel(&b.app, "GET", "/reglages", None).await;
    assert_eq!(r["consommation"]["appels"], 1);
    assert_eq!(r["consommation"]["depuis_cache"], 1);
}

#[tokio::test]
async fn le_plafond_atteint_bloque_tout_appel() {
    // 1 M de jetons d'entrée sur Haiku 5.5 : 0,10 $, au-delà d'un plafond de
    // 0,05 €.
    let b = banc_avec(None, 1_000_000);
    configurer(&b, "0,05").await;
    *b.texte.lock().unwrap() = reponse_playlist();
    let (s1, _) = appel(
        &b.app,
        "POST",
        "/playlist",
        Some(json!({"phrase": "première"})),
    )
    .await;
    assert_eq!(s1, StatusCode::OK);
    let (s2, v2) = appel(
        &b.app,
        "POST",
        "/playlist",
        Some(json!({"phrase": "seconde"})),
    )
    .await;
    assert_eq!(s2, StatusCode::CONFLICT, "{v2}");
    assert_eq!(v2["error"], "plafond_atteint");
    assert!(v2["message"].as_str().unwrap().contains("Plafond"));
    let (s3, v3) = appel(&b.app, "POST", "/albums/3/suggestions", None).await;
    assert_eq!(
        (s3, v3["error"].as_str()),
        (StatusCode::CONFLICT, Some("plafond_atteint"))
    );
    let (s4, v4) = appel(&b.app, "POST", "/tester", None).await;
    assert_eq!(
        (s4, v4["error"].as_str()),
        (StatusCode::CONFLICT, Some("plafond_atteint"))
    );
    assert_eq!(
        b.appels.load(Ordering::SeqCst),
        1,
        "un appel est parti plafond atteint"
    );
    let (_, r) = appel(&b.app, "GET", "/reglages", None).await;
    assert_eq!(r["consommation"]["plafond_atteint"], true);
}

#[tokio::test]
async fn un_plafond_a_zero_ne_laisse_rien_partir() {
    let b = banc();
    configurer(&b, "0").await;
    let (s, v) = appel(&b.app, "POST", "/tester", None).await;
    assert_eq!(
        (s, v["error"].as_str()),
        (StatusCode::CONFLICT, Some("plafond_atteint"))
    );
    assert_eq!(b.appels.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn un_modele_sans_tarif_connu_est_refuse_plutot_que_compte_zero() {
    let b = banc();
    appel(
        &b.app,
        "PUT",
        "/reglages",
        Some(json!({"fournisseur": "compatible_openai", "modele": "un-modele", "cle": CLE})),
    )
    .await;
    let (s, v) = appel(&b.app, "POST", "/tester", None).await;
    assert_eq!(
        (s, v["error"].as_str()),
        (StatusCode::BAD_REQUEST, Some("tarif_inconnu"))
    );
    assert_eq!(b.appels.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn la_donnee_musicbrainz_n_est_jamais_proposee_en_ecrasement() {
    let b = banc();
    configurer(&b, "5").await;
    *b.texte.lock().unwrap() = json!({"suggestions": [
        {"field": "genre", "value": "Modal Jazz", "confidence": 0.95, "reason": "x"},
        {"field": "title", "value": "Kind Of Blue", "confidence": 0.9, "reason": "x"},
        {"field": "label", "value": "Columbia", "confidence": 0.9, "reason": "x"}
    ]})
    .to_string();
    let (s, v) = appel(&b.app, "POST", "/albums/2/suggestions", None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let proposes: Vec<&str> = v["suggestions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["champ"].as_str().unwrap())
        .collect();
    assert_eq!(proposes, vec!["label"]);
    assert!(
        v["ecartees"]
            .as_array()
            .unwrap()
            .iter()
            .all(|e| e["raison"] == "musicbrainz")
    );
    // Même forcée par l'écran, l'application refuse.
    let (_, a) = appel(
        &b.app,
        "POST",
        "/albums/2/appliquer",
        Some(json!({"lignes": [{"champ": "genre", "valeur": "Modal Jazz"}, {"champ": "label", "valeur": "Columbia"}]})),
    )
    .await;
    assert_eq!(a["rapport"]["appliquees"][0]["champ"], "label");
    assert_eq!(a["rapport"]["refusees"][0]["raison"], "musicbrainz");
    // Annulable : le label redevient vide.
    let (_, u) = appel(&b.app, "POST", "/albums/2/annuler", None).await;
    assert_eq!(u["rapport"]["appliquees"][0]["champ"], "label");
    let (_, h) = appel(&b.app, "GET", "/albums/2/historique", None).await;
    assert_eq!(h["operations"][0]["annulee"], true);
}

// ---------------------------------------------------------------------------
// La clé : jamais dans une réponse d'API, jamais dans un journal.
// ---------------------------------------------------------------------------

#[derive(Clone, Default)]
struct Tampon(Arc<Mutex<Vec<u8>>>);

impl Write for Tampon {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Un faux « Anthropic » local : 401 pour toute clé, et il retient l'en-tête
/// reçu — preuve que la clé part bien au fournisseur, et seulement là.
async fn faux_anthropic() -> (String, Arc<Mutex<Option<String>>>) {
    let recue = Arc::new(Mutex::new(None));
    let r2 = recue.clone();
    let app = axum::Router::new().route(
        "/v1/messages",
        axum::routing::post(move |entetes: axum::http::HeaderMap| {
            let r2 = r2.clone();
            async move {
                *r2.lock().unwrap() = entetes
                    .get("x-api-key")
                    .and_then(|v| v.to_str().ok())
                    .map(String::from);
                (
                    StatusCode::UNAUTHORIZED,
                    axum::Json(json!({"type": "error", "error": {"type": "authentication_error", "message": "invalid x-api-key"}})),
                )
            }
        }),
    );
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let adresse = format!("http://{}", ecoute.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(ecoute, app).await.unwrap();
    });
    (adresse, recue)
}

#[tokio::test]
async fn la_cle_n_apparait_ni_dans_une_reponse_ni_dans_un_journal() {
    let journal = Tampon::default();
    let j2 = journal.clone();
    let abonne = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .with_writer(move || j2.clone())
        .finish();
    let _garde = tracing::subscriber::set_default(abonne);

    let b = banc_avec(Some(fabrique_reelle()), 0);
    let (adresse, recue) = faux_anthropic().await;
    let mut reponses = Vec::new();

    let (s, v) = appel(
        &b.app,
        "PUT",
        "/reglages",
        Some(json!({"fournisseur": "anthropic", "cle": CLE, "plafond_eur": "5", "url_base": adresse})),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["cle_fin"], "wxyz");
    assert_eq!(v["cle_definie"], true);
    reponses.push(v);
    reponses.push(appel(&b.app, "GET", "/reglages", None).await.1);

    // « Tester la clé » avec le VRAI client : la clé part au fournisseur…
    let (s, v) = appel(&b.app, "POST", "/tester", None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    assert_eq!(v["error"], "cle_refusee");
    assert_eq!(recue.lock().unwrap().as_deref(), Some(CLE));
    reponses.push(v);
    reponses.push(
        appel(
            &b.app,
            "POST",
            "/playlist",
            Some(json!({"phrase": "du jazz"})),
        )
        .await
        .1,
    );
    reponses.push(appel(&b.app, "POST", "/albums/3/suggestions", None).await.1);

    // …et nulle part ailleurs.
    let fragment = "TRES-SECRETE";
    for r in &reponses {
        assert!(
            !r.to_string().contains(fragment),
            "clé dans une réponse : {r}"
        );
    }
    let texte = String::from_utf8(journal.0.lock().unwrap().clone()).unwrap();
    assert!(
        texte.contains("assistant_ia_fournisseur_erreur"),
        "le journal n'a rien capté : la garde serait aveugle\n{texte}"
    );
    assert!(!texte.contains(fragment), "clé dans le journal :\n{texte}");

    // Au repos : scellée, et sous un nom que la liste des secrets reconnaît.
    let s = SettingsRepo::with_backend(b.db.clone());
    let scelle = s.get(CLE_API_CHIFFREE).unwrap().unwrap();
    assert!(!scelle.contains(fragment));
    assert!(tune_core::secrets::est_secret(CLE_API_CHIFFREE));
    assert!(s.get(CLE_URL).unwrap().is_some());
}

#[tokio::test]
async fn effacer_la_cle_la_retire() {
    let b = banc();
    configurer(&b, "5").await;
    let (_, v) = appel(&b.app, "PUT", "/reglages", Some(json!({"cle": ""}))).await;
    assert_eq!(v["cle_definie"], false);
    let (s, v) = appel(&b.app, "POST", "/tester", None).await;
    assert_eq!(
        (s, v["error"].as_str()),
        (StatusCode::BAD_REQUEST, Some("non_configure"))
    );
}

// ---------------------------------------------------------------------------
// Gemini : le préréglage compatible OpenAI, contre un faux serveur local.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn gemini_part_en_bearer_sans_json_object_et_compte_au_tarif_de_la_table() {
    let recu = Arc::new(Mutex::new(None::<(Option<String>, Value)>));
    let r2 = recu.clone();
    let app = axum::Router::new().route(
        "/chat/completions",
        axum::routing::post(move |entetes: axum::http::HeaderMap, axum::Json(corps): axum::Json<Value>| {
            let r2 = r2.clone();
            async move {
                let auth = entetes
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .map(String::from);
                *r2.lock().unwrap() = Some((auth, corps));
                axum::Json(json!({
                    "choices": [{"message": {"role": "assistant", "content": "{\"ok\": true}"}}],
                    "usage": {"prompt_tokens": 1000000, "completion_tokens": 0}
                }))
            }
        }),
    );
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let adresse = format!("http://{}", ecoute.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(ecoute, app).await.unwrap();
    });

    let b = banc_avec(Some(fabrique_reelle()), 0);
    // L'utilisateur ne choisit que le fournisseur et colle sa clé ; l'adresse
    // est remplacée ici par le faux serveur.
    let (_, v) = appel(
        &b.app,
        "PUT",
        "/reglages",
        Some(json!({"fournisseur": "gemini", "cle": CLE, "plafond_eur": "5", "url_base": adresse})),
    )
    .await;
    assert_eq!(v["modele_effectif"], "gemini-3.5-flash-lite");
    assert_eq!(v["tarif_connu"], true);
    let (s, t) = appel(&b.app, "POST", "/tester", None).await;
    assert_eq!(s, StatusCode::OK, "{t}");
    let (auth, corps) = recu.lock().unwrap().clone().unwrap();
    assert_eq!(auth, Some(format!("Bearer {CLE}")));
    assert_eq!(corps["model"], "gemini-3.5-flash-lite");
    assert!(corps.get("response_format").is_none(), "{corps}");
    // 1 M de jetons d'entrée à 0,30 $ : 0,30 € au taux 1.
    assert_eq!(t["cout_eur"], 0.3);
}

#[tokio::test]
async fn le_prereglage_gemini_pointe_sur_google() {
    let b = banc();
    let (_, v) = appel(
        &b.app,
        "PUT",
        "/reglages",
        Some(json!({"fournisseur": "gemini"})),
    )
    .await;
    assert_eq!(
        v["url_par_defaut"],
        "https://generativelanguage.googleapis.com/v1beta/openai"
    );
}
