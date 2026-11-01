//! « Ma collection physique » (#7195) de bout en bout, SANS RÉSEAU : l'hôte
//! d'essai rejoue des réponses enregistrées (`tests/fixtures`).
//!
//! Les fixtures :
//! * `collection_page_{1,2}.json` : deux pages d'une collection, au format
//!   relevé le 30/10/2026 sur l'API Discogs (`/users/{nom}/collection/
//!   folders/0/releases`, `per_page=3`). Les disques sont réels (catalogue
//!   public) ; le compte (`exemple`), les `instance_id` et les dates d'ajout
//!   sont inventés — la collection d'une vraie personne n'a rien à faire
//!   dans un dépôt public ;
//! * `release_1050.json` (relevée le 30/10/2026, réduite) et
//!   `release_249504.json` (celle de `tune-core/tests/fixtures/discogs`) :
//!   deux fiches `GET /releases/{id}`, pour leurs codes-barres ;
//! * `mb_url_release_249504.json`, `mb_url_master_96559.json` : deux réponses
//!   MusicBrainz `/ws/2/url?resource=…`, relevées le 30/10/2026 ;
//! * `discogs_collection_privee_403.json` : la réponse de Discogs à la
//!   collection privée d'un autre compte (403), relevée le 30/10/2026.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_collection_physique::correspondance::{AlbumDeService, AlbumLocal};
use tune_collection_physique::{Cadence, Hote, Image, Jeton, Moteur, Panne, Reponse, router};
use tune_core::db::backend::DbBackend;
use tune_core::db::sqlite::SqliteDb;

fn fixture(nom: &str) -> Value {
    let chemin = format!("{}/tests/fixtures/{nom}", env!("CARGO_MANIFEST_DIR"));
    serde_json::from_str(&std::fs::read_to_string(&chemin).unwrap()).unwrap()
}

/// La forme d'une vignette de la page de collection Discogs.
fn vignette_essai(release: u64) -> String {
    format!("https://i.discogs.com/essai/rs:fit/g:sm/q:40/h:150/w:150/{release}.jpeg")
}

/// La forme de la grande image (`cover_image`) de la page de collection.
fn pochette_essai(release: u64) -> String {
    format!("https://i.discogs.com/essai/rs:fit/g:sm/q:90/h:600/w:600/{release}.jpeg")
}

/// Une image JPEG carrée de `cote` px : ce que le CDN de Discogs rendrait.
fn jpeg(cote: u32) -> Vec<u8> {
    let img = image::RgbImage::from_pixel(cote, cote, image::Rgb([20, 120, 200]));
    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 90)
        .encode_image(&img)
        .unwrap();
    out
}

const RICK_RELEASE_MB: &str = "dc919562-19c7-42c8-961c-7ed5520f00bb";
const RICK_GROUPE_MB: &str = "19a8b6e5-f753-36c6-a3c6-189390d09935";

struct HoteEssai {
    jeton: Jeton,
    appels: Mutex<Vec<String>>,
    services: Vec<String>,
    albums: Mutex<Vec<AlbumLocal>>,
    recherches: HashMap<(String, String), Vec<AlbumDeService>>,
    /// Combien de 429 rendre avant de répondre.
    limites_a_rendre: Mutex<u32>,
    /// Les pages de collection portent-elles `thumb` ? (Les fixtures ont
    /// des vignettes vides ; l'API réelle en donne une par disque.)
    vignettes: Mutex<bool>,
    /// MusicBrainz : `None` répond ; `Some(true)` refuse (503 → `Limite`) ;
    /// `Some(false)` ne répond pas (panne de transport).
    mb_refuse: Mutex<Option<bool>>,
    /// MusicBrainz répond à autant d'appels, puis se tait (`None` : sans fin).
    mb_avant_panne: Mutex<Option<usize>>,
    /// Les services qui ne répondent pas (panne de transport).
    services_muets: Mutex<Vec<String>>,
    /// Combien de 429 le CDN d'images rend avant de répondre.
    images_limites: Mutex<u32>,
    /// Le CDN d'images répond à autant d'appels, puis se tait.
    images_avant_panne: Mutex<Option<usize>>,
    /// Le dernier disque de la page 2 a été retiré de la collection.
    disque_retire: Mutex<bool>,
}

impl HoteEssai {
    fn new() -> Self {
        let mut recherches = HashMap::new();
        recherches.insert(
            (
                "tidal".to_string(),
                "Janice McClain / The Ritchie Family Giving My Love / Brazil".to_string(),
            ),
            vec![AlbumDeService {
                id: "T-42".into(),
                titre: "Giving My Love / Brazil".into(),
                artiste: "Janice McClain".into(),
            }],
        );
        Self {
            jeton: Jeton::Personnel,
            appels: Mutex::new(Vec::new()),
            services: vec!["qobuz".into(), "tidal".into()],
            albums: Mutex::new(bibliotheque()),
            recherches,
            limites_a_rendre: Mutex::new(0),
            vignettes: Mutex::new(false),
            mb_refuse: Mutex::new(None),
            mb_avant_panne: Mutex::new(None),
            services_muets: Mutex::new(Vec::new()),
            images_limites: Mutex::new(0),
            images_avant_panne: Mutex::new(None),
            disque_retire: Mutex::new(false),
        }
    }

    /// Une page de collection, avec une vignette par disque si demandé.
    fn page(&self, nom: &str) -> Value {
        let mut p = fixture(nom);
        if *self.vignettes.lock().unwrap() {
            for r in p["releases"].as_array_mut().unwrap() {
                let id = r["id"].as_u64().unwrap();
                r["basic_information"]["thumb"] = json!(vignette_essai(id));
                r["basic_information"]["cover_image"] = json!(pochette_essai(id));
            }
        }
        if *self.disque_retire.lock().unwrap() {
            p["pagination"]["items"] = json!(4);
            if nom == "collection_page_2.json" {
                p["releases"].as_array_mut().unwrap().pop();
            }
        }
        p
    }

    fn appels(&self, prefixe: &str) -> usize {
        self.appels
            .lock()
            .unwrap()
            .iter()
            .filter(|a| a.starts_with(prefixe))
            .count()
    }
}

/// La bibliothèque : Rick Astley par son GROUPE MusicBrainz (sous un autre
/// titre), le CD 1050 par son code-barre, « Silent Mirrors » par artiste +
/// titre. « The Haçienda » (2 CD) et « Giving My Love » n'y sont pas.
fn bibliotheque() -> Vec<AlbumLocal> {
    vec![
        AlbumLocal {
            id: 1,
            titre: "Never Gonna Give You Up (Remastered)".into(),
            artiste: Some("Rick Astley".into()),
            mb_groupe: Some(RICK_GROUPE_MB.into()),
            qualite: Some("cd".into()),
            format: Some("flac".into()),
            pochette: Some("a1b2c3.jpg".into()),
            ..Default::default()
        },
        AlbumLocal {
            id: 2,
            titre: "Montreal Mix Sessions Volume 4".into(),
            artiste: Some("Laflèche".into()),
            code_barre: Some("0671665000928".into()),
            qualite: Some("cd".into()),
            ..Default::default()
        },
        AlbumLocal {
            id: 3,
            titre: "Silent Mirrors".into(),
            artiste: Some("Mihail P".into()),
            qualite: Some("hi-res".into()),
            sample_rate: Some(96_000),
            bit_depth: Some(24),
            ..Default::default()
        },
    ]
}

#[async_trait]
impl Hote for HoteEssai {
    fn jeton_discogs(&self) -> Jeton {
        self.jeton
    }

    async fn discogs(&self, chemin: &str, params: &[(&str, String)]) -> Result<Reponse, Panne> {
        let page = params
            .iter()
            .find(|(k, _)| *k == "page")
            .map(|(_, v)| v.as_str())
            .unwrap_or("");
        self.appels
            .lock()
            .unwrap()
            .push(format!("discogs {chemin} {page}"));
        {
            let mut l = self.limites_a_rendre.lock().unwrap();
            if *l > 0 {
                *l -= 1;
                return Ok(Reponse::Limite);
            }
        }
        Ok(match (chemin, page) {
            ("/users/exemple/collection/folders/0/releases", "1") => {
                Reponse::Json(self.page("collection_page_1.json"))
            }
            ("/users/exemple/collection/folders/0/releases", "2") => {
                Reponse::Json(self.page("collection_page_2.json"))
            }
            ("/users/prive/collection/folders/0/releases", _) => Reponse::Refusee,
            ("/users/panne/collection/folders/0/releases", _) => return Err(Panne),
            ("/releases/249504", _) => Reponse::Json(fixture("release_249504.json")),
            ("/releases/1050", _) => Reponse::Json(fixture("release_1050.json")),
            ("/oauth/identity", _) => Reponse::Json(json!({
                "id": 1, "username": "exemple",
                "resource_url": "https://api.discogs.com/users/exemple",
                "consumer_name": "Tune"
            })),
            _ => Reponse::Introuvable,
        })
    }

    async fn musicbrainz(&self, chemin: &str, params: &[(&str, String)]) -> Result<Reponse, Panne> {
        let resource = params
            .iter()
            .find(|(k, _)| *k == "resource")
            .map(|(_, v)| v.clone())
            .unwrap_or_default();
        self.appels
            .lock()
            .unwrap()
            .push(format!("mb {chemin} {resource}"));
        match *self.mb_refuse.lock().unwrap() {
            Some(true) => return Ok(Reponse::Limite),
            Some(false) => return Err(Panne),
            None => {}
        }
        if let Some(n) = self.mb_avant_panne.lock().unwrap().as_mut() {
            if *n == 0 {
                return Err(Panne);
            }
            *n -= 1;
        }
        Ok(match resource.as_str() {
            "https://www.discogs.com/release/249504" => {
                Reponse::Json(fixture("mb_url_release_249504.json"))
            }
            "https://www.discogs.com/master/96559" => {
                Reponse::Json(fixture("mb_url_master_96559.json"))
            }
            _ => Reponse::Introuvable,
        })
    }

    async fn services_connectes(&self) -> Vec<String> {
        self.services.clone()
    }

    async fn chercher_albums(
        &self,
        service: &str,
        requete: &str,
    ) -> Result<Vec<AlbumDeService>, Panne> {
        self.appels
            .lock()
            .unwrap()
            .push(format!("service {service} {requete}"));
        if self
            .services_muets
            .lock()
            .unwrap()
            .iter()
            .any(|m| m == service)
        {
            return Err(Panne);
        }
        Ok(self
            .recherches
            .get(&(service.to_string(), requete.to_string()))
            .cloned()
            .unwrap_or_default())
    }

    async fn albums_locaux(&self) -> Result<Vec<AlbumLocal>, String> {
        Ok(self.albums.lock().unwrap().clone())
    }

    async fn image_discogs(&self, url: &str) -> Result<Image, Panne> {
        self.appels.lock().unwrap().push(format!("image {url}"));
        {
            let mut l = self.images_limites.lock().unwrap();
            if *l > 0 {
                *l -= 1;
                return Ok(Image::Limite);
            }
        }
        if let Some(n) = self.images_avant_panne.lock().unwrap().as_mut() {
            if *n == 0 {
                return Err(Panne);
            }
            *n -= 1;
        }
        Ok(Image::Octets(jpeg(600)))
    }
}

fn base() -> Arc<dyn DbBackend> {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    tune_core::db::migrations::run_migrations(&db).unwrap();
    Arc::new(db)
}

struct Banc {
    hote: Arc<HoteEssai>,
    moteur: Arc<Moteur>,
    app: axum::Router,
    backend: Arc<dyn DbBackend>,
    _dossier: tempfile::TempDir,
}

fn banc_avec(hote: HoteEssai) -> Banc {
    banc_avec_cadence(hote, Cadence::immediate())
}

/// Le recul de l'affinage MusicBrainz, court mais non nul : la tâche
/// d'affinage ne tourne pas à vide pendant que MusicBrainz se tait.
fn cadence_recul_court() -> Cadence {
    Cadence {
        attente_limite: std::time::Duration::from_millis(2),
        attente_max: std::time::Duration::from_millis(10),
        ..Cadence::immediate()
    }
}

fn banc_avec_cadence(hote: HoteEssai, cadence: Cadence) -> Banc {
    let dossier = tempfile::tempdir().unwrap();
    let hote = Arc::new(hote);
    let moteur = Arc::new(Moteur::new(
        hote.clone(),
        dossier.path().to_path_buf(),
        cadence,
    ));
    let backend = base();
    let app = router(backend.clone(), moteur.clone());
    Banc {
        hote,
        moteur,
        app,
        backend,
        _dossier: dossier,
    }
}

async fn appel(
    app: &axum::Router,
    methode: &str,
    chemin: &str,
    corps: Option<Value>,
) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(methode)
        .uri(chemin)
        .header("content-type", "application/json")
        .body(
            corps
                .map(|c| Body::from(c.to_string()))
                .unwrap_or_else(Body::empty),
        )
        .unwrap();
    let rep = app.clone().oneshot(req).await.unwrap();
    let code = rep.status();
    let octets = axum::body::to_bytes(rep.into_body(), usize::MAX)
        .await
        .unwrap();
    (code, serde_json::from_slice(&octets).unwrap_or(Value::Null))
}

async fn suivre(b: &Banc, nom: &str) {
    let (code, r) = appel(
        &b.app,
        "PUT",
        "/reglages",
        Some(json!({"nom_utilisateur": nom})),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{r}");
}

/// La synchronisation, puis l'affinage MusicBrainz, attendus jusqu'au bout.
async fn synchro_complete(b: &Banc, nom: &str) {
    b.moteur.synchroniser_maintenant(nom).await.unwrap();
    b.moteur.affiner_maintenant().await;
}

/// Attend (2 s au plus) que `cond` soit vraie.
async fn attendre_que(mut cond: impl FnMut() -> bool, quoi: &str) {
    for _ in 0..400 {
        if cond() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    panic!("jamais arrivé : {quoi}");
}

fn disque<'a>(c: &'a Value, titre: &str) -> &'a Value {
    c["disques"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["titre"] == titre)
        .unwrap_or_else(|| panic!("{titre} absent de {c}"))
}

#[tokio::test]
async fn la_collection_se_compare_a_la_bibliotheque_et_aux_services() {
    let b = banc_avec(HoteEssai::new());
    suivre(&b, "exemple").await;
    synchro_complete(&b, "exemple").await;

    let (code, c) = appel(&b.app, "GET", "/collection", None).await;
    assert_eq!(code, StatusCode::OK, "{c}");
    assert_eq!(c["statut"], "ok");
    assert_eq!(c["attribution"]["texte"], "Data provided by Discogs");
    assert_eq!(c["compteurs"]["total"], 5, "deux pages lues : {c}");
    assert_eq!(c["compteurs"]["par_etat"]["bibliotheque"], 3, "{c}");
    assert_eq!(c["compteurs"]["par_etat"]["streaming"], 1, "{c}");
    assert_eq!(c["compteurs"]["par_etat"]["nulle_part"], 1, "{c}");
    assert_eq!(c["compteurs"]["par_etat"]["a_verifier"], 0, "{c}");
    assert_eq!(c["compteurs"]["par_format"]["vinyle"], 3, "{c}");
    assert_eq!(c["compteurs"]["par_format"]["cd"], 2, "{c}");
    assert_eq!(c["compteurs"]["cd_non_copies"], 1, "{c}");

    // Rick Astley : par le master Discogs, relié au GROUPE MusicBrainz.
    let rick = disque(&c, "Never Gonna Give You Up");
    assert_eq!(rick["bibliotheque"]["album_id"], 1, "{rick}");
    assert_eq!(rick["bibliotheque"]["par"], "musicbrainz_album");
    assert_eq!(rick["format"], "vinyle");
    assert_eq!(rick["copie"], Value::Null, "un vinyle ne se copie pas");
    assert_eq!(rick["codes_barres"], json!(["5012394144777"]));
    assert_eq!(
        rick["lien_discogs"],
        "https://www.discogs.com/release/249504"
    );

    // Le CD 1050 : par son code-barre (deux écritures du même code, une seule
    // gardée), et donc COPIÉ.
    let cd = disque(&c, "Montreal Mix Sessions Vol. 4");
    assert_eq!(cd["bibliotheque"]["album_id"], 2, "{cd}");
    assert_eq!(cd["bibliotheque"]["par"], "code_barre");
    assert_eq!(cd["codes_barres"], json!(["671665000928"]));
    assert_eq!(cd["copie"], true);
    assert_eq!(cd["proposer_extraction"], false);

    // Par artiste + titre, avec la qualité de l'album local.
    let sm = disque(&c, "Silent Mirrors");
    assert_eq!(sm["bibliotheque"]["par"], "artiste_titre");
    assert_eq!(sm["bibliotheque"]["qualite"], "hi-res");
    assert_eq!(sm["bibliotheque"]["bit_depth"], 24);

    // En streaming, chez TIDAL : l'artiste joint comme Discogs le joint.
    let gm = disque(&c, "Giving My Love / Brazil");
    assert_eq!(gm["artiste"], "Janice McClain / The Ritchie Family");
    assert_eq!(gm["etat"], "streaming");
    assert_eq!(
        gm["streaming"],
        json!([{"service": "tidal", "id": "T-42", "titre": "Giving My Love / Brazil",
                "artiste": "Janice McClain", "par": "artiste_titre"}])
    );

    // Nulle part, et un CD (2 CD) possédé mais pas copié.
    let h = disque(&c, "The Haçienda - Play By 01/96");
    assert_eq!(h["etat"], "nulle_part", "{h}");
    assert_eq!(h["artiste"], "Various");
    assert_eq!(h["copie"], false);
    assert_eq!(h["proposer_extraction"], true);

    // Les services ne sont interrogés QUE pour les disques absents de la
    // bibliothèque : 2 disques × 2 services, sans code-barre connu.
    assert_eq!(
        b.hote.appels("service "),
        4,
        "{:?}",
        b.hote.appels.lock().unwrap()
    );

    // Les filtres.
    let (_, cds) = appel(&b.app, "GET", "/collection?format=cd", None).await;
    assert_eq!(cds["disques"].as_array().unwrap().len(), 2, "{cds}");
    assert_eq!(
        cds["compteurs"]["total"], 5,
        "les compteurs portent sur toute la collection"
    );
    let (_, st) = appel(&b.app, "GET", "/collection?etat=streaming", None).await;
    assert_eq!(st["disques"].as_array().unwrap().len(), 1, "{st}");
    let (code, r) = appel(&b.app, "GET", "/collection?format=laserdisc", None).await;
    assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(r["error"], "collection_physique.format_inconnu");
}

#[tokio::test]
async fn la_deuxieme_synchronisation_ne_relit_que_la_liste() {
    let b = banc_avec(HoteEssai::new());
    suivre(&b, "exemple").await;
    synchro_complete(&b, "exemple").await;
    assert_eq!(
        b.hote.appels("discogs /releases/"),
        5,
        "une fiche par disque"
    );
    let mb = b.hote.appels("mb ");
    assert_eq!(
        mb,
        5 + 3,
        "une release par disque, puis un appel par master distinct (Rick, 1050 et 7024 en ont un) — {mb}"
    );
    synchro_complete(&b, "exemple").await;
    assert_eq!(
        b.hote.appels("discogs /releases/"),
        5,
        "incrémentale : aucune fiche relue"
    );
    assert_eq!(
        b.hote.appels("discogs /users/"),
        3,
        "la liste est relue PAR LE HAUT : sa première page touche la liste connue, la seconde n'est pas redemandée"
    );
    assert_eq!(
        b.hote.appels("service "),
        4,
        "streaming gardé : rien de redemandé"
    );
    // Les liens MusicBrainz absents se revoient dans trente jours, pas avant.
    assert_eq!(b.hote.appels("mb "), mb);
}

#[tokio::test]
async fn la_memoire_survit_au_redemarrage() {
    let dossier = tempfile::tempdir().unwrap();
    let hote = Arc::new(HoteEssai::new());
    let backend = base();
    let m1 = Arc::new(Moteur::new(
        hote.clone(),
        dossier.path().to_path_buf(),
        Cadence::immediate(),
    ));
    let app = router(backend.clone(), m1.clone());
    let (code, _) = appel(
        &app,
        "PUT",
        "/reglages",
        Some(json!({"nom_utilisateur": "exemple"})),
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    m1.synchroniser_maintenant("exemple").await.unwrap();
    drop(app);
    let m2 = Arc::new(Moteur::new(
        hote.clone(),
        dossier.path().to_path_buf(),
        Cadence::immediate(),
    ));
    let app = router(backend, m2);
    let (_, c) = appel(&app, "GET", "/collection", None).await;
    assert_eq!(c["compteurs"]["total"], 5, "{c}");
}

#[tokio::test]
async fn une_collection_privee_d_un_autre_compte_est_refusee_et_dite() {
    let b = banc_avec(HoteEssai::new());
    suivre(&b, "prive").await;
    assert_eq!(
        b.moteur.synchroniser_maintenant("prive").await,
        Err("collection_privee")
    );
    let (_, e) = appel(&b.app, "GET", "/etat", None).await;
    assert_eq!(e["derniere_erreur"], "collection_privee", "{e}");
    assert_eq!(e["disques"], 0);
    let (_, c) = appel(&b.app, "GET", "/collection", None).await;
    assert_eq!(c["statut"], "a_synchroniser", "{c}");
}

#[tokio::test]
async fn une_panne_de_discogs_garde_la_liste_precedente() {
    let b = banc_avec(HoteEssai::new());
    suivre(&b, "exemple").await;
    b.moteur.synchroniser_maintenant("exemple").await.unwrap();
    // Même compte, Discogs en panne : la liste lue reste montrée.
    *b.hote.limites_a_rendre.lock().unwrap() = 100;
    assert_eq!(
        b.moteur.synchroniser_maintenant("exemple").await,
        Err("discogs_limite")
    );
    let (_, c) = appel(&b.app, "GET", "/collection", None).await;
    assert_eq!(c["compteurs"]["total"], 5, "{c}");
    let (_, e) = appel(&b.app, "GET", "/etat", None).await;
    assert_eq!(e["derniere_erreur"], "discogs_limite");
}

#[tokio::test]
async fn un_429_se_reessaie() {
    let h = HoteEssai::new();
    *h.limites_a_rendre.lock().unwrap() = 2;
    let b = banc_avec(h);
    suivre(&b, "exemple").await;
    b.moteur.synchroniser_maintenant("exemple").await.unwrap();
    let (_, c) = appel(&b.app, "GET", "/collection", None).await;
    assert_eq!(c["compteurs"]["total"], 5, "{c}");
}

#[tokio::test]
async fn changer_de_compte_oublie_la_liste_de_l_ancien() {
    let b = banc_avec(HoteEssai::new());
    suivre(&b, "exemple").await;
    b.moteur.synchroniser_maintenant("exemple").await.unwrap();
    suivre(&b, "quelquun").await;
    let (_, c) = appel(&b.app, "GET", "/collection", None).await;
    assert_eq!(c["statut"], "a_synchroniser", "{c}");
    assert_eq!(c["disques"], json!([]));
    let (code, r) = appel(
        &b.app,
        "PUT",
        "/reglages",
        Some(json!({"nom_utilisateur": "a/../b"})),
    )
    .await;
    assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(r["error"], "collection_physique.nom_invalide");
    let (_, r) = appel(
        &b.app,
        "PUT",
        "/reglages",
        Some(json!({"nom_utilisateur": null})),
    )
    .await;
    assert_eq!(r["nom_utilisateur"], Value::Null);
    let (_, c) = appel(&b.app, "GET", "/collection", None).await;
    assert_eq!(c["statut"], "aucun_compte");
    let (code, r) = appel(&b.app, "POST", "/synchroniser", None).await;
    assert_eq!(code, StatusCode::CONFLICT);
    assert_eq!(r["error"], "collection_physique.aucun_compte");
}

#[tokio::test]
async fn le_compte_du_jeton_personnel_se_retient_et_le_jeton_ne_sort_jamais() {
    let b = banc_avec(HoteEssai::new());
    let (code, r) = appel(&b.app, "POST", "/reglages/identite", None).await;
    assert_eq!(code, StatusCode::OK, "{r}");
    assert_eq!(
        r,
        json!({"nom_utilisateur": "exemple", "jeton": "personnel"})
    );

    let mut h = HoteEssai::new();
    h.jeton = Jeton::Integre;
    let b = banc_avec(h);
    let (code, r) = appel(&b.app, "POST", "/reglages/identite", None).await;
    assert_eq!(code, StatusCode::CONFLICT);
    assert_eq!(r["error"], "collection_physique.jeton_personnel_absent");
}

#[tokio::test]
async fn un_cd_insere_se_reconnait_et_dit_s_il_est_copie() {
    let b = banc_avec(HoteEssai::new());
    suivre(&b, "exemple").await;
    b.moteur.synchroniser_maintenant("exemple").await.unwrap();
    let (_, r) = appel(&b.app, "GET", "/cd?code_barre=0671665000928", None).await;
    assert_eq!(r["dans_la_collection"], true, "{r}");
    assert_eq!(r["copie"], true);
    // Le même disque, retiré de la bibliothèque : possédé, pas copié.
    b.hote.albums.lock().unwrap().retain(|a| a.id != 2);
    let (_, r) = appel(&b.app, "GET", "/cd?code_barre=6%2071665%2000092%208", None).await;
    assert_eq!(r["copie"], false, "{r}");
    assert_eq!(r["disque"]["proposer_extraction"], true);
    // Un vinyle n'est pas un CD inséré, même par son pressage MusicBrainz.
    let (_, r) = appel(
        &b.app,
        "GET",
        &format!("/cd?mb_release={RICK_RELEASE_MB}"),
        None,
    )
    .await;
    assert_eq!(r["dans_la_collection"], false, "{r}");
    let (code, _) = appel(&b.app, "GET", "/cd", None).await;
    assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn sans_service_connecte_un_disque_absent_est_nulle_part() {
    let mut h = HoteEssai::new();
    h.services = vec![];
    let b = banc_avec(h);
    suivre(&b, "exemple").await;
    b.moteur.synchroniser_maintenant("exemple").await.unwrap();
    let (_, c) = appel(&b.app, "GET", "/collection", None).await;
    assert_eq!(c["compteurs"]["par_etat"]["nulle_part"], 2, "{c}");
    assert_eq!(c["compteurs"]["par_etat"]["a_verifier"], 0, "{c}");
    assert_eq!(b.hote.appels("service "), 0);
}

/// Essai réel du 31/10/2026 : aucune pochette à l'écran. Chaque disque porte
/// désormais la vignette de la PAGE DE COLLECTION (aucun appel de plus), et
/// un disque de la bibliothèque, la pochette de l'album local.
#[tokio::test]
async fn chaque_disque_porte_sa_vignette_et_l_album_local_sa_pochette() {
    let hote = HoteEssai::new();
    *hote.vignettes.lock().unwrap() = true;
    let b = banc_avec(hote);
    suivre(&b, "exemple").await;
    b.moteur.synchroniser_maintenant("exemple").await.unwrap();
    let (_, c) = appel(&b.app, "GET", "/collection", None).await;

    let h = disque(&c, "The Haçienda - Play By 01/96");
    assert_eq!(h["pochette"], true, "{h}");
    let rick = disque(&c, "Never Gonna Give You Up");
    assert_eq!(rick["bibliotheque"]["cover_path"], "a1b2c3.jpg", "{rick}");
    assert_eq!(rick["pochette"], true);
    // Cache du 01/11/2026 : aucune adresse du CDN de Discogs ne sort vers
    // l'écran, qui demande la pochette au greffon.
    assert!(!c.to_string().contains("i.discogs.com"), "{c}");
    // Un album local sans pochette : `null`, l'écran prend la vignette.
    let sm = disque(&c, "Silent Mirrors");
    assert_eq!(sm["bibliotheque"]["cover_path"], Value::Null, "{sm}");
    // Rien de plus qu'avant : une fiche par disque, deux pages de liste.
    assert_eq!(b.hote.appels("discogs /releases/"), 5);
    assert_eq!(b.hote.appels("discogs /users/"), 2);
}

/// Une collection synchronisée AVANT les pochettes (mémoire d'une forme de
/// liste plus ancienne) les reçoit à la relecture suivante — relue EN
/// ENTIER, pas par le haut —, sans que rien d'autre ne soit refait.
#[tokio::test]
async fn une_synchronisation_d_avant_les_vignettes_les_recoit_a_la_suivante() {
    let b = banc_avec(HoteEssai::new());
    suivre(&b, "exemple").await;
    b.moteur.synchroniser_maintenant("exemple").await.unwrap();
    let (_, c) = appel(&b.app, "GET", "/collection", None).await;
    assert!(
        c["disques"]
            .as_array()
            .unwrap()
            .iter()
            .all(|d| d["pochette"] == false),
        "{c}"
    );
    let fiches = b.hote.appels("discogs /releases/");
    let mb = b.hote.appels("mb ");
    let services = b.hote.appels("service ");
    let pages = b.hote.appels("discogs /users/");

    // La mémoire telle qu'une version d'avant l'a écrite : sans
    // `version_liste`, sans `liste_complete_le`.
    let fichier = b._dossier.path().join("collection.json");
    let mut brut: Value = serde_json::from_slice(&std::fs::read(&fichier).unwrap()).unwrap();
    brut.as_object_mut().unwrap().remove("version_liste");
    brut.as_object_mut().unwrap().remove("liste_complete_le");
    std::fs::write(&fichier, brut.to_string()).unwrap();
    let moteur = Arc::new(Moteur::new(
        b.hote.clone(),
        b._dossier.path().to_path_buf(),
        Cadence::immediate(),
    ));
    let app = router(b.backend.clone(), moteur.clone());

    *b.hote.vignettes.lock().unwrap() = true;
    moteur.synchroniser_maintenant("exemple").await.unwrap();
    let (_, c) = appel(&app, "GET", "/collection", None).await;
    assert!(
        c["disques"]
            .as_array()
            .unwrap()
            .iter()
            .all(|d| d["pochette"] == true),
        "{c}"
    );
    assert_eq!(
        b.hote.appels("discogs /users/"),
        pages + 2,
        "une liste d'une forme ancienne se relit en entier"
    );
    assert_eq!(
        b.hote.appels("discogs /releases/"),
        fiches,
        "aucune fiche relue"
    );
    assert_eq!(b.hote.appels("mb "), mb, "aucun lien MusicBrainz relu");
    assert_eq!(
        b.hote.appels("service "),
        services,
        "aucun service réinterrogé"
    );
}

/// Essai réel du 31/10/2026 sur le .18 (908 disques, 680 à rapprocher) :
/// MusicBrainz passait AVANT les services, 6 disques en 10 minutes, et 531
/// disques restaient « à vérifier » pendant des heures. Désormais la
/// synchronisation ne l'attend plus du tout : « En streaming » se remplit
/// MusicBrainz en panne, et aucun message d'erreur ne reste.
async fn musicbrainz_en_panne_le_streaming_se_remplit_quand_meme(refus: bool) {
    let hote = HoteEssai::new();
    *hote.mb_refuse.lock().unwrap() = Some(refus);
    let b = banc_avec_cadence(hote, cadence_recul_court());
    suivre(&b, "exemple").await;
    let r = b.moteur.synchroniser_maintenant("exemple").await;
    assert_eq!(r, Ok(()), "MusicBrainz est un plus, pas une condition");
    assert_eq!(
        b.hote.appels("mb "),
        0,
        "la synchronisation n'attend pas MusicBrainz"
    );
    assert!(b.hote.appels("service ") > 0, "Qobuz / TIDAL interrogés");

    let (_, e) = appel(&b.app, "GET", "/etat", None).await;
    assert_eq!(e["derniere_erreur"], Value::Null, "{e}");
    assert!(e["derniere_synchro"].is_i64(), "{e}");
    assert_eq!(
        e["affinage_musicbrainz"],
        json!({"en_cours": false, "restants": 5}),
        "{e}"
    );
    let (_, c) = appel(&b.app, "GET", "/collection", None).await;
    let gm = disque(&c, "Giving My Love / Brazil");
    assert_eq!(gm["etat"], "streaming", "{gm}");
    // Le code-barre et l'artiste + titre suffisent à la bibliothèque.
    let cd = disque(&c, "Montreal Mix Sessions Vol. 4");
    assert_eq!(cd["bibliotheque"]["par"], "code_barre", "{cd}");
    let rick = disque(&c, "Never Gonna Give You Up");
    assert_eq!(rick["bibliotheque"]["par"], "artiste_titre", "{rick}");

    // L'affinage en tâche de fond : MusicBrainz muet, il recule sans erreur.
    assert!(b.moteur.lancer_affinage());
    attendre_que(|| b.hote.appels("mb ") >= 3, "trois essais MusicBrainz").await;
    let (_, e) = appel(&b.app, "GET", "/etat", None).await;
    assert_eq!(e["affinage_musicbrainz"]["en_cours"], true, "{e}");
    assert_eq!(e["affinage_musicbrainz"]["restants"], 5, "{e}");
    assert_eq!(e["derniere_erreur"], Value::Null, "{e}");

    // MusicBrainz revient : l'affinage finit seul, Rick est reconnu par son
    // groupe MusicBrainz, et aucune fiche Discogs n'est relue.
    let fiches = b.hote.appels("discogs /releases/");
    *b.hote.mb_refuse.lock().unwrap() = None;
    attendre_que(
        || b.moteur.affinage().restants == 0 && !b.moteur.affinage().en_cours,
        "affinage fini",
    )
    .await;
    assert_eq!(b.hote.appels("discogs /releases/"), fiches);
    let (_, c) = appel(&b.app, "GET", "/collection", None).await;
    let rick = disque(&c, "Never Gonna Give You Up");
    assert_eq!(rick["bibliotheque"]["par"], "musicbrainz_album", "{rick}");
    assert_eq!(c["affinage_musicbrainz"]["restants"], 0, "{c}");
}

#[tokio::test]
async fn musicbrainz_qui_refuse_n_empeche_pas_le_streaming() {
    musicbrainz_en_panne_le_streaming_se_remplit_quand_meme(true).await;
}

#[tokio::test]
async fn musicbrainz_qui_ne_repond_pas_n_empeche_pas_le_streaming() {
    musicbrainz_en_panne_le_streaming_se_remplit_quand_meme(false).await;
}

/// Contre-épreuve : « En streaming » vient bien des services. Sans service
/// connecté, la même synchronisation (MusicBrainz en panne) n'en trouve
/// aucun.
#[tokio::test]
async fn contre_epreuve_sans_service_rien_en_streaming() {
    let mut hote = HoteEssai::new();
    hote.services = vec![];
    *hote.mb_refuse.lock().unwrap() = Some(true);
    let b = banc_avec(hote);
    suivre(&b, "exemple").await;
    b.moteur.synchroniser_maintenant("exemple").await.unwrap();
    let (_, c) = appel(&b.app, "GET", "/collection", None).await;
    assert_eq!(c["compteurs"]["par_etat"]["streaming"], 0, "{c}");
    assert_eq!(disque(&c, "Giving My Love / Brazil")["etat"], "nulle_part");
}

/// Une mémoire d'avant le correctif, arrêtée sur `musicbrainz_indisponible`,
/// ne garde pas ce message rouge.
#[tokio::test]
async fn l_ancien_message_musicbrainz_indisponible_s_efface() {
    let dossier = tempfile::tempdir().unwrap();
    let m = tune_collection_physique::memoire::Memoire {
        derniere_erreur: Some("musicbrainz_indisponible".into()),
        ..Default::default()
    };
    m.ecrire(dossier.path()).unwrap();
    let moteur = Moteur::new(
        Arc::new(HoteEssai::new()),
        dossier.path().to_path_buf(),
        Cadence::immediate(),
    );
    assert_eq!(moteur.memoire().derniere_erreur, None);
    // Contre-épreuve : une vraie erreur de Discogs, elle, reste dite.
    let m = tune_collection_physique::memoire::Memoire {
        derniere_erreur: Some("discogs_limite".into()),
        ..Default::default()
    };
    m.ecrire(dossier.path()).unwrap();
    let moteur = Moteur::new(
        Arc::new(HoteEssai::new()),
        dossier.path().to_path_buf(),
        Cadence::immediate(),
    );
    assert_eq!(
        moteur.memoire().derniere_erreur.as_deref(),
        Some("discogs_limite")
    );
}

/// Les recherches d'un disque chez un service, dans l'ordre.
fn recherches(hote: &HoteEssai, service: &str) -> Vec<String> {
    let prefixe = format!("service {service} ");
    hote.appels
        .lock()
        .unwrap()
        .iter()
        .filter_map(|a| a.strip_prefix(&prefixe).map(str::to_string))
        .collect()
}

/// Par code-barre (UPC / EAN de la fiche Discogs) d'abord, puis par
/// artiste + titre. Le CD 1050, retiré de la bibliothèque : Qobuz le trouve
/// par son code-barre (aucune recherche de plus), TIDAL ne le trouve pas
/// par code-barre et est alors interrogé par artiste + titre.
#[tokio::test]
async fn le_code_barre_d_abord_puis_artiste_et_titre() {
    let mut hote = HoteEssai::new();
    hote.albums.lock().unwrap().retain(|a| a.id != 2);
    hote.recherches.insert(
        ("qobuz".into(), "671665000928".into()),
        vec![AlbumDeService {
            id: "Q-1050".into(),
            titre: "Montreal Mix Sessions Vol. 4".into(),
            artiste: "Laflèche".into(),
        }],
    );
    let b = banc_avec(hote);
    suivre(&b, "exemple").await;
    b.moteur.synchroniser_maintenant("exemple").await.unwrap();
    let (_, c) = appel(&b.app, "GET", "/collection", None).await;
    let cd = disque(&c, "Montreal Mix Sessions Vol. 4");
    assert_eq!(cd["etat"], "streaming", "{cd}");
    assert_eq!(cd["streaming"][0]["service"], "qobuz", "{cd}");
    assert_eq!(cd["streaming"][0]["par"], "code_barre", "{cd}");

    let qobuz = recherches(&b.hote, "qobuz");
    let tidal = recherches(&b.hote, "tidal");
    let i = qobuz.iter().position(|r| r == "671665000928").unwrap();
    assert!(
        !qobuz[i + 1..]
            .first()
            .is_some_and(|r| r.ends_with("Montreal Mix Sessions Vol. 4")),
        "trouvé par code-barre : pas de recherche par artiste + titre — {qobuz:?}"
    );
    let j = tidal.iter().position(|r| r == "671665000928").unwrap();
    assert!(
        tidal[j + 1].ends_with("Montreal Mix Sessions Vol. 4"),
        "TIDAL : le code-barre, PUIS artiste + titre — {tidal:?}"
    );

    // Contre-épreuve : un disque SANS code-barre (la fiche de « The
    // Haçienda » n'en donne pas) est cherché d'emblée par artiste + titre.
    let h: Vec<&String> = qobuz.iter().filter(|r| r.contains("Haçienda")).collect();
    assert_eq!(h.len(), 1, "{qobuz:?}");
    assert!(h[0].starts_with("Various "), "{h:?}");
}

/// « Nulle part » seulement quand TOUS les services connectés ont répondu.
#[tokio::test]
async fn nulle_part_seulement_apres_la_reponse_de_tous_les_services() {
    let hote = HoteEssai::new();
    *hote.services_muets.lock().unwrap() = vec!["tidal".into()];
    let b = banc_avec(hote);
    suivre(&b, "exemple").await;
    b.moteur.synchroniser_maintenant("exemple").await.unwrap();
    let (_, c) = appel(&b.app, "GET", "/collection", None).await;
    let h = disque(&c, "The Haçienda - Play By 01/96");
    assert_eq!(h["etat"], "a_verifier", "Qobuz a répondu, TIDAL non : {h}");
    assert_eq!(c["compteurs"]["par_etat"]["nulle_part"], 0, "{c}");

    // Contre-épreuve : TIDAL répond à la synchronisation suivante, qui
    // reprend le disque (et lui seul côté services) : « nulle part ».
    b.hote.services_muets.lock().unwrap().clear();
    b.moteur.synchroniser_maintenant("exemple").await.unwrap();
    let (_, c) = appel(&b.app, "GET", "/collection", None).await;
    let h = disque(&c, "The Haçienda - Play By 01/96");
    assert_eq!(h["etat"], "nulle_part", "{h}");
    let gm = disque(&c, "Giving My Love / Brazil");
    assert_eq!(gm["etat"], "streaming", "{gm}");
}

/// L'affinage MusicBrainz reprend après un redémarrage là où il s'était
/// arrêté : les disques déjà lus ne sont pas redemandés, Discogs n'est pas
/// rappelé.
#[tokio::test]
async fn l_affinage_reprend_apres_un_redemarrage() {
    let dossier = tempfile::tempdir().unwrap();
    let h1 = HoteEssai::new();
    *h1.mb_avant_panne.lock().unwrap() = Some(3);
    let h1 = Arc::new(h1);
    let m1 = Arc::new(Moteur::new(
        h1.clone(),
        dossier.path().to_path_buf(),
        cadence_recul_court(),
    ));
    m1.synchroniser_maintenant("exemple").await.unwrap();
    assert!(m1.lancer_affinage());
    // MusicBrainz se tait après trois réponses : l'affinage recule, et a
    // rangé ce qu'il a appris avant de reculer.
    attendre_que(|| h1.appels("mb ") >= 5, "l'affinage recule").await;
    let lus: Vec<u64> = tune_collection_physique::memoire::Memoire::lire(dossier.path())
        .liens_mb
        .keys()
        .copied()
        .collect();
    assert!(!lus.is_empty() && lus.len() < 5, "{lus:?}");
    m1.arreter();
    assert!(!m1.affinage().en_cours);
    drop(m1);

    // Le « redémarrage » : un nouveau moteur sur le même dossier.
    let h2 = Arc::new(HoteEssai::new());
    let m2 = Arc::new(Moteur::new(
        h2.clone(),
        dossier.path().to_path_buf(),
        Cadence::immediate(),
    ));
    assert_eq!(m2.affinage().restants, 5 - lus.len());
    m2.affiner_maintenant().await;
    assert_eq!(m2.affinage().restants, 0);
    assert_eq!(h2.appels("discogs "), 0, "rien de relu chez Discogs");
    for r in &lus {
        assert_eq!(
            h2.appels(&format!("mb url https://www.discogs.com/release/{r}")),
            0,
            "{r} déjà lu avant le redémarrage"
        );
    }
    assert_eq!(
        h2.appels("mb url https://www.discogs.com/release/"),
        5 - lus.len()
    );

    // Contre-épreuve : sans la mémoire, les 5 disques sont redemandés.
    let vide = tempfile::tempdir().unwrap();
    let h3 = Arc::new(HoteEssai::new());
    let m3 = Moteur::new(h3.clone(), vide.path().to_path_buf(), Cadence::immediate());
    m3.synchroniser_maintenant("exemple").await.unwrap();
    m3.affiner_maintenant().await;
    assert_eq!(h3.appels("mb url https://www.discogs.com/release/"), 5);
}

// ─── Le cache (demande de Bertrand du 01/11/2026) ──────────────────────────

/// Une requête brute sur le routeur : statut, en-têtes, octets.
async fn brut(
    app: &axum::Router,
    methode: &str,
    chemin: &str,
    entetes: &[(&str, &str)],
) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
    let mut req = Request::builder().method(methode).uri(chemin);
    for (k, v) in entetes {
        req = req.header(*k, *v);
    }
    let rep = app
        .clone()
        .oneshot(req.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let code = rep.status();
    let h = rep.headers().clone();
    let octets = axum::body::to_bytes(rep.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec();
    (code, h, octets)
}

fn banc_avec_pochettes() -> Banc {
    let hote = HoteEssai::new();
    *hote.vignettes.lock().unwrap() = true;
    banc_avec(hote)
}

fn tous_les_appels(h: &HoteEssai) -> usize {
    h.appels.lock().unwrap().len()
}

/// Deuxième ouverture de l'écran, après un redémarrage : la liste, les
/// états et les pochettes viennent du cache. AUCUN appel à Discogs (ni API
/// ni CDN), ni à MusicBrainz, ni aux services.
#[tokio::test]
async fn la_deuxieme_ouverture_n_appelle_pas_discogs() {
    let b = banc_avec_pochettes();
    suivre(&b, "exemple").await;
    synchro_complete(&b, "exemple").await;
    assert_eq!(b.moteur.ranger_les_pochettes().await, 5);
    let (_, avant) = appel(&b.app, "GET", "/collection", None).await;
    let appels = tous_les_appels(&b.hote);
    assert!(b.hote.appels("image ") == 5, "une image par disque");

    // Redémarrage : un nouveau moteur sur le même dossier.
    let moteur = Arc::new(Moteur::new(
        b.hote.clone(),
        b._dossier.path().to_path_buf(),
        Cadence::immediate(),
    ));
    let app = router(b.backend.clone(), moteur.clone());
    let (_, c) = appel(&app, "GET", "/collection", None).await;
    assert_eq!(c["statut"], "ok", "{c}");
    assert_eq!(c["compteurs"], avant["compteurs"], "{c}");
    let (_, e) = appel(&app, "GET", "/etat", None).await;
    assert_eq!(e["pochettes"]["en_cache"], 5, "{e}");
    assert_eq!(e["pochettes"]["restantes"], 0, "{e}");
    for d in c["disques"].as_array().unwrap() {
        let r = d["release_id"].as_u64().unwrap();
        let (code, _, _) = brut(&app, "GET", &format!("/pochettes/{r}"), &[]).await;
        assert_eq!(code, StatusCode::OK, "pochette {r}");
    }
    assert!(!moteur.lancer_pochettes(), "rien à préremplir");
    assert_eq!(
        tous_les_appels(&b.hote),
        appels,
        "aucun appel de plus : {:?}",
        &b.hote.appels.lock().unwrap()[appels..]
    );
}

/// La pochette est servie depuis le disque : vignette de 300 px en JPEG,
/// originale à part, `Cache-Control`, `ETag`, et 304 sur `If-None-Match`.
#[tokio::test]
async fn la_pochette_est_servie_en_local_avec_etag_et_304() {
    let b = banc_avec_pochettes();
    suivre(&b, "exemple").await;
    b.moteur.synchroniser_maintenant("exemple").await.unwrap();
    assert_eq!(b.moteur.ranger_les_pochettes().await, 5);
    assert_eq!(b.hote.appels("image "), 5);
    assert_eq!(
        b.hote.appels(&format!("image {}", pochette_essai(249504))),
        1,
        "la GRANDE image est celle qu'on télécharge"
    );

    let (code, h, octets) = brut(&b.app, "GET", "/pochettes/249504", &[]).await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(h["content-type"], "image/jpeg");
    assert!(
        h["cache-control"].to_str().unwrap().contains("max-age="),
        "{h:?}"
    );
    let tag = h["etag"].to_str().unwrap().to_string();
    let v = image::load_from_memory(&octets).unwrap();
    assert_eq!((v.width(), v.height()), (300, 300), "vignette de 300 px");

    let (code, h2, octets) = brut(
        &b.app,
        "GET",
        "/pochettes/249504",
        &[("if-none-match", tag.as_str())],
    )
    .await;
    assert_eq!(code, StatusCode::NOT_MODIFIED);
    assert!(octets.is_empty());
    assert_eq!(h2["etag"].to_str().unwrap(), tag);

    // Contre-épreuve : un autre ETag rend l'image.
    let (code, _, _) = brut(
        &b.app,
        "GET",
        "/pochettes/249504",
        &[("if-none-match", "\"autre\"")],
    )
    .await;
    assert_eq!(code, StatusCode::OK);

    let (code, _, octets) = brut(&b.app, "GET", "/pochettes/249504?taille=originale", &[]).await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(image::load_from_memory(&octets).unwrap().width(), 600);

    // Contre-épreuve : une release hors de la collection n'est pas servie,
    // et ne fait aucun appel.
    let (code, _, _) = brut(&b.app, "GET", "/pochettes/424242", &[]).await;
    assert_eq!(code, StatusCode::NOT_FOUND);
    assert_eq!(b.hote.appels("image "), 5, "rien de retéléchargé");
}

/// Une pochette demandée par l'écran avant le préremplissage passe par la
/// file : téléchargée une fois, servie en local ensuite.
#[tokio::test]
async fn une_pochette_demandee_avant_son_tour_se_telecharge_une_fois() {
    let b = banc_avec_pochettes();
    suivre(&b, "exemple").await;
    b.moteur.synchroniser_maintenant("exemple").await.unwrap();
    assert_eq!(
        b.hote.appels("image "),
        0,
        "la synchronisation seule ne télécharge rien"
    );
    for _ in 0..3 {
        let (code, _, _) = brut(&b.app, "GET", "/pochettes/1050", &[]).await;
        assert_eq!(code, StatusCode::OK);
    }
    assert_eq!(b.hote.appels("image "), 1);
    assert_eq!(
        b.moteur.ranger_les_pochettes().await,
        4,
        "les quatre autres"
    );
}

/// Un 429 du CDN : recul (doublé à chaque refus), puis la pochette arrive.
#[tokio::test]
async fn un_429_sur_une_pochette_fait_reculer_puis_reussit() {
    let hote = HoteEssai::new();
    *hote.vignettes.lock().unwrap() = true;
    let b = banc_avec_cadence(
        hote,
        Cadence {
            attente_limite: std::time::Duration::from_millis(40),
            attente_max: std::time::Duration::from_secs(1),
            ..Cadence::immediate()
        },
    );
    suivre(&b, "exemple").await;
    b.moteur.synchroniser_maintenant("exemple").await.unwrap();
    *b.hote.images_limites.lock().unwrap() = 2;
    let debut = std::time::Instant::now();
    let (code, _, _) = brut(&b.app, "GET", "/pochettes/1050", &[]).await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(b.hote.appels("image "), 3, "deux refus, puis l'image");
    assert!(
        debut.elapsed() >= std::time::Duration::from_millis(40 + 80),
        "recul de 40 ms puis 80 ms : {:?}",
        debut.elapsed()
    );

    // Contre-épreuve : un CDN qui refuse toujours. Au plus `essais_limite`
    // nouveaux essais, puis rien n'est rangé, et l'écran reçoit un 404.
    *b.hote.images_limites.lock().unwrap() = 100;
    let (code, _, _) = brut(&b.app, "GET", "/pochettes/249504", &[]).await;
    assert_eq!(code, StatusCode::NOT_FOUND);
    assert_eq!(
        b.hote.appels("image "),
        3 + 4,
        "1 + 3 essais, pas un de plus"
    );
    assert!(
        !b._dossier.path().join("pochettes/249504.jpg").exists(),
        "rien de rangé"
    );
}

/// Le préremplissage s'arrête sur une panne et reprend, après un
/// redémarrage, aux seules pochettes qui manquent.
#[tokio::test]
async fn les_pochettes_reprennent_apres_un_redemarrage() {
    let b = banc_avec_pochettes();
    suivre(&b, "exemple").await;
    b.moteur.synchroniser_maintenant("exemple").await.unwrap();
    *b.hote.images_avant_panne.lock().unwrap() = Some(2);
    assert_eq!(
        b.moteur.ranger_les_pochettes().await,
        2,
        "deux, puis la panne"
    );
    assert_eq!(b.hote.appels("image "), 3);
    let (_, e) = appel(&b.app, "GET", "/etat", None).await;
    assert_eq!(e["pochettes"]["en_cache"], 2, "{e}");
    assert_eq!(e["pochettes"]["restantes"], 3, "{e}");

    *b.hote.images_avant_panne.lock().unwrap() = None;
    let moteur = Arc::new(Moteur::new(
        b.hote.clone(),
        b._dossier.path().to_path_buf(),
        Cadence::immediate(),
    ));
    assert!(moteur.lancer_pochettes(), "la passe reprend au démarrage");
    attendre_que(
        || moteur.etat_pochettes().en_cache == 5 && !moteur.etat_pochettes().en_cours,
        "les cinq pochettes rangées",
    )
    .await;
    assert_eq!(
        b.hote.appels("image "),
        3 + 3,
        "seules les trois qui manquaient sont téléchargées"
    );
    let appels = b.hote.appels.lock().unwrap().clone();
    for r in [249504u64, 1050, 7024, 33597366, 38821545] {
        let n = appels
            .iter()
            .filter(|a| **a == format!("image {}", pochette_essai(r)))
            .count();
        assert!(n >= 1 && n <= 2, "{r} : {n}");
    }
    // Contre-épreuve : la même reprise sur un dossier VIDE retélécharge tout.
    let vide = tempfile::tempdir().unwrap();
    let neuf = Moteur::new(
        b.hote.clone(),
        vide.path().to_path_buf(),
        Cadence::immediate(),
    );
    assert_eq!(
        neuf.ranger_les_pochettes().await,
        0,
        "dossier vide : pas de liste, rien à ranger"
    );
}

/// Relecture partielle de la liste, puis complète dès qu'un disque est
/// sorti de la collection (le compte ne tombe plus juste).
#[tokio::test]
async fn un_disque_retire_fait_relire_toute_la_liste() {
    let b = banc_avec(HoteEssai::new());
    suivre(&b, "exemple").await;
    b.moteur.synchroniser_maintenant("exemple").await.unwrap();
    b.moteur.synchroniser_maintenant("exemple").await.unwrap();
    assert_eq!(b.hote.appels("discogs /users/"), 3, "2 pages, puis 1");

    *b.hote.disque_retire.lock().unwrap() = true;
    b.moteur.synchroniser_maintenant("exemple").await.unwrap();
    assert_eq!(
        b.hote.appels("discogs /users/"),
        5,
        "le compte annoncé (4) ne tombe plus juste : les deux pages"
    );
    let (_, c) = appel(&b.app, "GET", "/collection", None).await;
    assert_eq!(
        c["compteurs"]["total"], 4,
        "le disque vendu est sorti : {c}"
    );
}

/// « Effacer le cache » : les pochettes seules, puis tout.
#[tokio::test]
async fn effacer_le_cache_efface_les_pochettes_puis_la_memoire() {
    let b = banc_avec_pochettes();
    suivre(&b, "exemple").await;
    b.moteur.synchroniser_maintenant("exemple").await.unwrap();
    assert_eq!(b.moteur.ranger_les_pochettes().await, 5);
    let dossier = b._dossier.path().join("pochettes");
    assert!(dossier.join("1050.jpg").is_file());

    let (code, r) = appel(&b.app, "DELETE", "/cache?quoi=pochettes", None).await;
    assert_eq!(code, StatusCode::OK, "{r}");
    assert!(!dossier.join("1050.jpg").exists());
    let (_, c) = appel(&b.app, "GET", "/collection", None).await;
    assert_eq!(c["statut"], "ok", "la liste reste : {c}");
    let (_, e) = appel(&b.app, "GET", "/etat", None).await;
    assert_eq!(e["pochettes"]["en_cache"], 0, "{e}");
    assert_eq!(e["pochettes"]["restantes"], 5, "{e}");

    let (code, _) = appel(&b.app, "DELETE", "/cache", None).await;
    assert_eq!(code, StatusCode::OK);
    let (_, c) = appel(&b.app, "GET", "/collection", None).await;
    assert_eq!(c["statut"], "a_synchroniser", "{c}");
    let (_, r) = appel(&b.app, "GET", "/reglages", None).await;
    assert_eq!(r["nom_utilisateur"], "exemple", "le compte suivi reste");
    // Et la mémoire effacée l'est aussi sur le disque (redémarrage).
    let m = Moteur::new(
        b.hote.clone(),
        b._dossier.path().to_path_buf(),
        Cadence::immediate(),
    );
    assert!(m.memoire().disques.is_empty());
    let (code, _) = appel(&b.app, "DELETE", "/cache?quoi=autre", None).await;
    assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY);
}
