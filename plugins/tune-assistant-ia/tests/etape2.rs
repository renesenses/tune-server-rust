//! Étape 2 de l'Assistant IA, par ses ROUTES et par la passerelle de l'hôte,
//! avec un FAUX fournisseur — aucun appel réseau réel. Le seul trafic est
//! local : un faux serveur « Anthropic » sur 127.0.0.1 pour la passerelle de
//! l'ancien `/ai/query`.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_assistant_ia::fournisseur::{
    Acces, Demande, ErreurFournisseur, Fabrique, Fournisseur, Reponse,
};
use tune_assistant_ia::reglages::{CLE_API_CHIFFREE, CLE_FOURNISSEUR, CLE_PLAFOND, CLE_URL};
use tune_assistant_ia::{CritereMoteur, Etat, Passerelle, router_vers};
use tune_core::db::backend::DbBackend;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::db::sqlite::SqliteDb;
use tune_core::db::track_metadata_repo::TrackMetadataRepo;
use tune_http_types::AuthUser;

const CLE: &str = "sk-ant-api03-ETAPE-DEUX-0123456789-wxyz";

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
        "INSERT INTO artists (id, name) VALUES (1, 'Karajan');
         INSERT INTO albums (id, title, artist_id, genre) VALUES (10, 'Beethoven 9', 1, 'Classical');
         INSERT INTO albums (id, title, artist_id, genre) VALUES (11, 'Brahms 1', 1, 'Classical');
         INSERT INTO albums (id, title, artist_id, genre) VALUES (12, 'Mahler 5', 1, 'Classical');
         INSERT INTO albums (id, title, artist_id, genre) VALUES (13, 'Bruckner 7', 1, 'Classical');
         INSERT INTO tracks (id, title, album_id, artist_id, file_path, track_number)
                VALUES (101, 'Allegro ma non troppo', 10, 1, '/m/101.flac', 1);
         INSERT INTO tracks (id, title, album_id, artist_id, file_path, track_number)
                VALUES (102, 'Molto vivace', 10, 1, '/m/102.flac', 2);",
    )
    .unwrap();
    Arc::new(db)
}

/// Le faux fournisseur : il compte ses appels, retient la dernière demande
/// et rend un texte fixé.
#[derive(Clone)]
struct Faux {
    appels: Arc<AtomicUsize>,
    texte: Arc<Mutex<String>>,
    json_demande: Arc<Mutex<Option<bool>>>,
    jetons_entree: u64,
}

#[async_trait]
impl Fournisseur for Faux {
    async fn completer(&self, d: &Demande) -> Result<Reponse, ErreurFournisseur> {
        self.appels.fetch_add(1, Ordering::SeqCst);
        *self.json_demande.lock().unwrap() = Some(d.json);
        Ok(Reponse {
            texte: self.texte.lock().unwrap().clone(),
            jetons_entree: self.jetons_entree,
            jetons_sortie: 0,
        })
    }
}

impl Faux {
    fn new(jetons_entree: u64) -> Self {
        Self {
            appels: Arc::new(AtomicUsize::new(0)),
            texte: Arc::new(Mutex::new(String::new())),
            json_demande: Arc::new(Mutex::new(None)),
            jetons_entree,
        }
    }
    fn fabrique(&self) -> Fabrique {
        let f = self.clone();
        Arc::new(move |_a: Acces| -> Box<dyn Fournisseur> { Box::new(f.clone()) })
    }
    fn repondre(&self, v: Value) {
        *self.texte.lock().unwrap() = v.to_string();
    }
    fn appels(&self) -> usize {
        self.appels.load(Ordering::SeqCst)
    }
}

struct Banc {
    app: axum::Router,
    db: Arc<dyn DbBackend>,
    faux: Faux,
    _dossier: tempfile::TempDir,
}

/// 1 M de jetons d'entrée sur Haiku 5.5 coûtent 0,10 $ = 0,10 € (taux 1).
const UN_MILLION: u64 = 1_000_000;

fn banc(jetons_entree: u64) -> Banc {
    let db = base();
    let dossier = tempfile::tempdir().unwrap();
    let faux = Faux::new(jetons_entree);
    let etat = Etat::new(
        db.clone(),
        dossier.path(),
        dossier.path().join("cache"),
        criteres(),
        faux.fabrique(),
    );
    Banc {
        app: router_vers(etat),
        db,
        faux,
        _dossier: dossier,
    }
}

async fn appel_en(
    app: &axum::Router,
    methode: &str,
    uri: &str,
    corps: Option<Value>,
    qui: Option<AuthUser>,
) -> (StatusCode, Value) {
    let mut req = Request::builder().method(methode).uri(uri);
    if let Some(u) = qui {
        req = req.extension(u);
    }
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
    (
        statut,
        serde_json::from_slice(&octets).unwrap_or(Value::Null),
    )
}

async fn appel(
    app: &axum::Router,
    methode: &str,
    uri: &str,
    corps: Option<Value>,
) -> (StatusCode, Value) {
    appel_en(app, methode, uri, corps, None).await
}

async fn configurer(b: &Banc, plafond: &str) {
    let (s, v) = appel(
        &b.app,
        "PUT",
        "/reglages",
        Some(json!({"fournisseur": "anthropic", "cle": CLE, "plafond_eur": plafond})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
}

fn suggestion_de_genre() -> Value {
    json!({"suggestions": [
        {"field": "subgenre", "value": "Symphony", "confidence": 0.9, "reason": "r"}
    ]})
}

// ---------------------------------------------------------------------------
// 5. Réglages réservés aux administrateurs
// ---------------------------------------------------------------------------

fn utilisateur(role: &str) -> AuthUser {
    AuthUser {
        user_id: 2,
        role: role.into(),
    }
}

#[tokio::test]
async fn les_reglages_sont_reserves_aux_administrateurs_quand_l_authentification_est_active() {
    let b = banc(0);
    SettingsRepo::with_backend(b.db.clone())
        .set("auth_enabled", "true")
        .unwrap();
    let modif = json!({"fournisseur": "anthropic", "cle": CLE, "plafond_eur": "5"});

    // Un profil ordinaire : refusé, et RIEN n'est écrit.
    let (s, v) = appel_en(
        &b.app,
        "PUT",
        "/reglages",
        Some(modif.clone()),
        Some(utilisateur("user")),
    )
    .await;
    assert_eq!(
        (s, v["error"].as_str()),
        (StatusCode::FORBIDDEN, Some("admin_requis")),
        "{v}"
    );
    let s_repo = SettingsRepo::with_backend(b.db.clone());
    assert_eq!(
        s_repo.get(CLE_API_CHIFFREE).unwrap(),
        None,
        "la clé ne doit pas être posée"
    );
    assert_eq!(s_repo.get(CLE_FOURNISSEUR).unwrap(), None);
    // La clé d'API du serveur ne porte pas de rôle : refusée. Jamais 401.
    let (s, _) = appel_en(&b.app, "PUT", "/reglages", Some(modif.clone()), None).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    // « Tester la clé » dépense : réservé aussi.
    let (s, _) = appel_en(&b.app, "POST", "/tester", None, Some(utilisateur("user"))).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    assert_eq!(b.faux.appels(), 0);
    // Lire reste permis, et dit que l'écran ne peut pas modifier.
    let (s, v) = appel_en(&b.app, "GET", "/reglages", None, Some(utilisateur("user"))).await;
    assert_eq!(
        (s, v["modifiable"].as_bool()),
        (StatusCode::OK, Some(false)),
        "{v}"
    );

    // L'administrateur, lui, peut.
    let (s, v) = appel_en(
        &b.app,
        "PUT",
        "/reglages",
        Some(modif),
        Some(utilisateur("admin")),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["modifiable"], true);
    assert_eq!(v["cle_definie"], true);
    let (s, _) = appel_en(&b.app, "POST", "/tester", None, Some(utilisateur("admin"))).await;
    assert_eq!(s, StatusCode::OK);
    // Les usages, eux, restent ouverts à tous les profils.
    b.faux.repondre(suggestion_de_genre());
    let (s, _) = appel_en(
        &b.app,
        "POST",
        "/albums/10/suggestions",
        None,
        Some(utilisateur("user")),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
}

#[tokio::test]
async fn sans_authentification_tout_est_ouvert_comme_le_reste_du_serveur() {
    let b = banc(0);
    let (s, v) = appel(&b.app, "GET", "/reglages", None).await;
    assert_eq!((s, v["modifiable"].as_bool()), (StatusCode::OK, Some(true)));
    configurer(&b, "5").await;
}

// ---------------------------------------------------------------------------
// 1. Un lot d'albums : arrêt net au plafond
// ---------------------------------------------------------------------------

#[tokio::test]
async fn un_lot_s_arrete_net_au_plafond_mensuel() {
    let b = banc(UN_MILLION);
    // 0,15 € : deux appels à 0,10 € passent (le second part à 0,10 < 0,15),
    // le troisième trouve le plafond franchi.
    configurer(&b, "0,15").await;
    b.faux.repondre(suggestion_de_genre());
    let (s, v) = appel(
        &b.app,
        "POST",
        "/lot/suggestions",
        Some(json!({"album_ids": [10, 11, 12, 13, 10]})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let traites: Vec<i64> = v["resultats"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["album_id"].as_i64().unwrap())
        .collect();
    assert_eq!(traites, vec![10, 11]);
    assert_eq!(v["arret"]["error"], "plafond_atteint", "{v}");
    assert_eq!(
        v["non_traites"],
        json!([12, 13]),
        "le doublon 10 n'est compté qu'une fois"
    );
    assert_eq!(b.faux.appels(), 2, "aucun appel ne part plafond atteint");
    assert!((v["cout_eur"].as_f64().unwrap() - 0.2).abs() < 1e-9, "{v}");
    // Chaque résultat porte l'écart avant/après et la confiance.
    let s0 = &v["resultats"][0]["suggestions"][0];
    assert_eq!(s0["champ"], "subgenre");
    assert_eq!(s0["avant"], Value::Null);
    assert_eq!(s0["apres"], "Symphony");
    assert_eq!(s0["confiance"], 0.9);
}

#[tokio::test]
async fn un_lot_s_arrete_a_son_propre_budget() {
    let b = banc(UN_MILLION);
    configurer(&b, "5").await;
    b.faux.repondre(suggestion_de_genre());
    let (s, v) = appel(
        &b.app,
        "POST",
        "/lot/suggestions",
        Some(json!({"album_ids": [10, 11, 12], "budget_eur": "0,05"})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["resultats"].as_array().unwrap().len(), 1);
    assert_eq!(v["arret"]["error"], "budget_du_lot_atteint");
    assert_eq!(v["non_traites"], json!([11, 12]));
    assert_eq!(b.faux.appels(), 1);
}

#[tokio::test]
async fn un_album_introuvable_n_arrete_pas_le_lot_et_un_lot_trop_grand_est_refuse() {
    let b = banc(0);
    configurer(&b, "5").await;
    b.faux.repondre(suggestion_de_genre());
    let (_, v) = appel(
        &b.app,
        "POST",
        "/lot/suggestions",
        Some(json!({"album_ids": [999, 10]})),
    )
    .await;
    assert_eq!(v["resultats"][0]["error"], "album_introuvable");
    assert_eq!(v["resultats"][1]["ok"], true);
    assert_eq!(v["arret"], Value::Null);
    let trop: Vec<i64> = (1..=51).collect();
    let (s, v) = appel(
        &b.app,
        "POST",
        "/lot/suggestions",
        Some(json!({"album_ids": trop})),
    )
    .await;
    assert_eq!(
        (s, v["error"].as_str()),
        (StatusCode::BAD_REQUEST, Some("lot_invalide"))
    );
}

#[tokio::test]
async fn appliquer_puis_annuler_un_lot() {
    let b = banc(0);
    let (s, v) = appel(
        &b.app,
        "POST",
        "/lot/appliquer",
        Some(json!({"albums": [
            {"album_id": 10, "lignes": [{"champ": "subgenre", "valeur": "Symphony"}]},
            {"album_id": 11, "lignes": [{"champ": "genre", "valeur": "Romantic"}]},
            {"album_id": 999, "lignes": [{"champ": "genre", "valeur": "X"}]}
        ]})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let r = v["rapports"].as_array().unwrap();
    assert_eq!(r[0]["rapport"]["appliquees"][0]["apres"], "Symphony");
    assert_eq!(r[2]["error"], "album_introuvable");
    let operations: Vec<Value> = r[..2]
        .iter()
        .map(|x| json!({"album_id": x["album_id"], "operation_id": x["rapport"]["operation_id"]}))
        .collect();
    let (s, v) = appel(
        &b.app,
        "POST",
        "/lot/annuler",
        Some(json!({ "operations": operations })),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let genre =
        b.db.query_one("SELECT genre FROM albums WHERE id = 11", &[])
            .unwrap()
            .unwrap();
    assert_eq!(genre[0].as_string().as_deref(), Some("Classical"));
}

// ---------------------------------------------------------------------------
// 1. Les pistes : œuvre, mouvement
// ---------------------------------------------------------------------------

#[tokio::test]
async fn les_pistes_recoivent_oeuvre_et_mouvement_sans_ecraser_une_valeur_presente() {
    let b = banc(0);
    configurer(&b, "5").await;
    TrackMetadataRepo::with_backend(b.db.clone())
        .set(102, "work", "Neuvième")
        .unwrap();
    b.faux.repondre(json!({"tracks": [
        {"id": 101, "field": "work", "value": "Symphony No. 9 in D minor, Op. 125", "confidence": 0.95, "reason": "r"},
        {"id": 101, "field": "movement", "value": "I. Allegro ma non troppo", "confidence": 0.9, "reason": "r"},
        {"id": 102, "field": "work", "value": "Symphony No. 9 in D minor, Op. 125", "confidence": 0.95, "reason": "r"},
        {"id": 555, "field": "work", "value": "Autre album", "confidence": 0.9, "reason": "r"}
    ]}));
    let (s, v) = appel(&b.app, "POST", "/albums/10/pistes/suggestions", None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let retenues: Vec<(i64, &str)> = v["suggestions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| {
            (
                x["piste_id"].as_i64().unwrap(),
                x["champ"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(retenues, vec![(101, "work"), (101, "movement")]);
    let raisons: Vec<&str> = v["ecartees"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x["raison"].as_str().unwrap())
        .collect();
    assert_eq!(raisons, vec!["deja_rempli", "piste_inconnue"]);

    let (s, v) = appel(
        &b.app,
        "POST",
        "/albums/10/pistes/appliquer",
        Some(json!({"lignes": [
            {"piste_id": 101, "champ": "movement", "valeur": "I. Allegro ma non troppo"},
            {"piste_id": 102, "champ": "work", "valeur": "Écrasement"}
        ]})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["rapport"]["appliquees"].as_array().unwrap().len(), 1);
    assert_eq!(v["rapport"]["refusees"][0]["raison"], "deja_rempli");
    let meta = TrackMetadataRepo::with_backend(b.db.clone());
    assert_eq!(
        meta.get_all(102).unwrap().get("work").map(String::as_str),
        Some("Neuvième")
    );
    // Annulable par la route de l'album.
    let (_, v) = appel(&b.app, "POST", "/albums/10/annuler", None).await;
    assert_eq!(v["rapport"]["appliquees"][0]["piste_id"], 101);
    assert!(!meta.get_all(101).unwrap().contains_key("movement"));
}

// ---------------------------------------------------------------------------
// 2. Le genre accepté vaut pour les pistes
// ---------------------------------------------------------------------------

#[tokio::test]
async fn le_genre_accepte_est_recopie_sur_les_pistes_puis_rendu_a_l_annulation() {
    let b = banc(0);
    let (_, v) = appel(
        &b.app,
        "POST",
        "/albums/10/appliquer",
        Some(json!({"lignes": [{"champ": "genre", "valeur": "Symphonic"}]})),
    )
    .await;
    assert_eq!(v["rapport"]["pistes_genre"], 2, "{v}");
    let genres = |db: &Arc<dyn DbBackend>| -> Vec<Option<String>> {
        db.query_many(
            "SELECT genre FROM tracks WHERE album_id = 10 ORDER BY id",
            &[],
        )
        .unwrap()
        .iter()
        .map(|l| l[0].as_string())
        .collect()
    };
    assert_eq!(
        genres(&b.db),
        vec![Some("Symphonic".into()), Some("Symphonic".into())]
    );
    let (_, v) = appel(&b.app, "POST", "/albums/10/annuler", None).await;
    assert_eq!(v["rapport"]["pistes_genre"], 2, "{v}");
    assert_eq!(genres(&b.db), vec![None, None]);
}

// ---------------------------------------------------------------------------
// 3. Étiquettes et collections par leur nom
// ---------------------------------------------------------------------------

#[tokio::test]
async fn une_etiquette_nommee_par_le_modele_devient_son_identifiant() {
    let b = banc(0);
    configurer(&b, "5").await;
    b.db.execute_batch("INSERT INTO tags (id, name) VALUES (4, 'Vinyle');")
        .unwrap();
    b.faux.repondre(json!({
        "name": "Mes vinyles", "match_mode": "all",
        "rules": [
            {"field": "tag", "op": "is", "value": "vinyle"},
            {"field": "tag", "op": "is", "value": "Cassette"}
        ],
        "untranslated": []
    }));
    let (s, v) = appel(
        &b.app,
        "POST",
        "/playlist",
        Some(json!({"phrase": "mes vinyles"})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(
        v["proposition"]["rules"],
        json!([{"field": "tag", "op": "is", "value": "4"}])
    );
    assert_eq!(
        v["proposition"]["refusees"][0]["raison"],
        "reference_inconnue"
    );
}

// ---------------------------------------------------------------------------
// 4. La passerelle : ancien `/ai/query` et traduction des ambiances
// ---------------------------------------------------------------------------

/// Un faux « Anthropic » local qui RÉPOND (200), compte les requêtes et
/// retient la clé reçue.
async fn faux_anthropic_qui_repond() -> (String, Arc<Mutex<Vec<String>>>) {
    let recues = Arc::new(Mutex::new(Vec::new()));
    let r2 = recues.clone();
    let app = axum::Router::new().route(
        "/v1/messages",
        axum::routing::post(
            move |entetes: axum::http::HeaderMap, axum::Json(corps): axum::Json<Value>| {
                let r2 = r2.clone();
                async move {
                    assert!(
                        corps["tools"].is_array(),
                        "les outils doivent partir : {corps}"
                    );
                    r2.lock().unwrap().push(
                        entetes
                            .get("x-api-key")
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or("")
                            .to_string(),
                    );
                    axum::Json(json!({
                        "content": [{"type": "text", "text": "Je lance Kind of Blue."}],
                        "stop_reason": "end_turn",
                        "usage": {"input_tokens": 1_000_000, "output_tokens": 0}
                    }))
                }
            },
        ),
    );
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let adresse = format!("http://{}", ecoute.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(ecoute, app).await.unwrap();
    });
    (adresse, recues)
}

#[tokio::test]
async fn l_ancien_assistant_prend_la_cle_migree_et_respecte_le_plafond() {
    let db = base();
    let dossier = tempfile::tempdir().unwrap();
    let (adresse, recues) = faux_anthropic_qui_repond().await;
    let s = SettingsRepo::with_backend(db.clone());
    // L'état d'un serveur d'avant l'assistant : la clé EN CLAIR.
    s.set("anthropic_api_key", CLE).unwrap();
    s.set(CLE_URL, &adresse).unwrap();
    s.set(CLE_PLAFOND, "0,05").unwrap();
    let p = Passerelle::new(db.clone(), dossier.path());
    assert!(
        !p.anthropic_pret(),
        "avant migration, l'assistant n'a pas de clé"
    );

    p.migrer();
    assert!(p.anthropic_pret());
    assert_eq!(
        s.get("anthropic_api_key").unwrap(),
        None,
        "la clé en clair doit partir"
    );

    let messages = json!([{"role": "user", "content": "joue du jazz"}]);
    let outils = json!([{"name": "play", "description": "d", "input_schema": {"type": "object"}}]);
    let v = p
        .discuter_anthropic("système", &messages, &outils, 512)
        .await
        .expect("premier tour");
    assert_eq!(v["content"][0]["text"], "Je lance Kind of Blue.");
    assert_eq!(*recues.lock().unwrap(), vec![CLE.to_string()]);
    // 1 M de jetons sur Haiku : 0,10 €, au-delà du plafond de 0,05 €.
    let conso: Value =
        serde_json::from_str(&s.get("assistant_ia_consommation").unwrap().unwrap()).unwrap();
    assert_eq!(conso["appels"], 1);

    let e = p
        .discuter_anthropic("système", &messages, &outils, 512)
        .await
        .unwrap_err();
    assert_eq!(e.code(), "plafond_atteint");
    assert_eq!(
        recues.lock().unwrap().len(),
        1,
        "aucune requête ne part plafond atteint"
    );
}

#[tokio::test]
async fn la_traduction_passe_par_le_fournisseur_de_l_assistant_et_son_plafond() {
    let db = base();
    let dossier = tempfile::tempdir().unwrap();
    let faux = Faux::new(UN_MILLION);
    faux.repondre(json!("warm intimate jazz"));
    let s = SettingsRepo::with_backend(db.clone());
    let p = Passerelle::avec_fabrique(db.clone(), dossier.path(), faux.fabrique());
    assert!(!p.traduction_disponible(), "rien de configuré");
    // Une clé Gemini héritée : la traduction s'annonce AVANT la migration…
    s.set("gemini_api_key", "AIza-cle-google").unwrap();
    assert!(p.traduction_disponible());
    s.set(CLE_PLAFOND, "0,05").unwrap();

    let t = p.traduire("Translate.", "jazz feutré").await;

    // …et la première traduction la migre, puis part chez Gemini (préréglage
    // de l'assistant), comptée au tarif de la table.
    assert_eq!(t.as_deref(), Some("\"warm intimate jazz\""));
    assert_eq!(s.get("gemini_api_key").unwrap(), None);
    assert_eq!(s.get(CLE_FOURNISSEUR).unwrap().as_deref(), Some("gemini"));
    assert_eq!(
        *faux.json_demande.lock().unwrap(),
        Some(false),
        "une traduction n'est pas du JSON"
    );
    assert_eq!(faux.appels(), 1);
    // Plafond franchi : plus rien ne part, l'appelant garde la requête brute.
    assert_eq!(p.traduire("Translate.", "rock progressif").await, None);
    assert_eq!(faux.appels(), 1);
}
