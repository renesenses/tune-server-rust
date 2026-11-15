//! Épreuves des routes `/sources-reseau` et du parcours de bout en bout
//! (#8189, étape 2), sur le faux système en mémoire.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

use tune_core::sources_reseau::memoire::SystemeMemoire;
use tune_core::sources_reseau::{self as sr, identifiants};

use crate::state::AppState;

/// Les faux NAS des épreuves, par « hôte ». La fabrique `memoire` y prend le
/// système de la source qu'on lui demande d'ouvrir.
fn nas() -> &'static Mutex<HashMap<String, Arc<SystemeMemoire>>> {
    static N: OnceLock<Mutex<HashMap<String, Arc<SystemeMemoire>>>> = OnceLock::new();
    N.get_or_init(|| Mutex::new(HashMap::new()))
}

fn faux_nas(hote: &str) -> Arc<SystemeMemoire> {
    sr::installer_la_fabrique(
        "memoire",
        Arc::new(
            |s: &tune_core::db::sources_reseau::SourceReseau,
             _ids: Option<&identifiants::Identifiants>| {
                nas()
                    .lock()
                    .unwrap()
                    .get(&s.hote)
                    .cloned()
                    .map(|m| m as Arc<dyn sr::SystemeDistant>)
                    .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotConnected, "absent"))
            },
        ),
    );
    let m = Arc::new(SystemeMemoire::nouveau());
    nas().lock().unwrap().insert(hote.to_string(), m.clone());
    m
}

/// Les identifiants et la clé de l'hôte sont des états du processus : les
/// épreuves qui les touchent passent une par une.
async fn verrou() -> tokio::sync::MutexGuard<'static, ()> {
    static V: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    V.lock().await
}

fn flac(remplissage: usize) -> Vec<u8> {
    let mut v = std::fs::read(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../tune-core/tests/fixtures/test.flac"),
    )
    .unwrap();
    v.extend(std::iter::repeat_n(0u8, remplissage));
    v
}

fn etat(mobile: bool) -> (AppState, tempfile::TempDir) {
    let dossier = tempfile::tempdir().unwrap();
    let base = dossier.path().join("tune.db");
    let config = crate::config::TuneConfig {
        mobile,
        ..Default::default()
    };
    let state = AppState::new(base.to_str().unwrap(), 0, config).unwrap();
    (state, dossier)
}

async fn appel(
    state: &AppState,
    methode: &str,
    uri: &str,
    corps: Option<Value>,
) -> (StatusCode, Vec<u8>, axum::http::HeaderMap) {
    appel_entetes(state, methode, uri, corps, &[]).await
}

async fn appel_entetes(
    state: &AppState,
    methode: &str,
    uri: &str,
    corps: Option<Value>,
    entetes: &[(&str, &str)],
) -> (StatusCode, Vec<u8>, axum::http::HeaderMap) {
    let app = crate::routes::router(state.clone());
    let mut r = Request::builder().method(methode).uri(uri);
    for (k, v) in entetes {
        r = r.header(*k, *v);
    }
    let req = match corps {
        Some(c) => r
            .header("content-type", "application/json")
            .body(Body::from(c.to_string()))
            .unwrap(),
        None => r.body(Body::empty()).unwrap(),
    };
    let rep = app.oneshot(req).await.unwrap();
    let statut = rep.status();
    let h = rep.headers().clone();
    let octets = axum::body::to_bytes(rep.into_body(), 64 << 20)
        .await
        .unwrap()
        .to_vec();
    (statut, octets, h)
}

fn json_de(o: &[u8]) -> Value {
    serde_json::from_slice(o).unwrap_or(Value::Null)
}

#[test]
fn la_passerelle_est_le_seul_chemin_exempte() {
    assert!(super::est_route_passerelle(
        "/api/v1/sources-reseau/abc123/fichier"
    ));
    assert!(super::est_route_passerelle(
        "/sources-reseau/abc123/fichier"
    ));
    assert!(!super::est_route_passerelle(
        "/api/v1/sources-reseau/abc123/parcourir"
    ));
    assert!(!super::est_route_passerelle(
        "/api/v1/sources-reseau/abc123/fichier/x"
    ));
    assert!(!super::est_route_passerelle(
        "/api/v1/sources-reseau/../fichier"
    ));
    assert!(!super::est_route_passerelle("/api/v1/sources-reseau"));
}

#[tokio::test]
async fn un_type_inconnu_est_refuse_sans_rien_creer() {
    let (state, _d) = etat(false);
    let (s, o, _) = appel(
        &state,
        "POST",
        "/api/v1/sources-reseau",
        Some(json!({"type": "smb", "hote": "nas"})),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(json_de(&o)["code"], "type_non_pris_en_charge");
    let (_, o, _) = appel(&state, "GET", "/api/v1/sources-reseau", None).await;
    assert_eq!(json_de(&o)["sources"].as_array().unwrap().len(), 0);
}

/// Sur mobile, sans clé de l'hôte, un mot de passe n'est PAS enregistré ;
/// avec la clé, il entre scellé : ni le mot de passe ni l'utilisateur ne sont
/// lisibles dans la base, et aucune route ne le rend.
#[tokio::test(flavor = "multi_thread")]
async fn identifiants_scelles_jamais_en_clair_dans_la_base() {
    let _v = verrou().await;
    let (state, _d) = etat(true);
    faux_nas("nas-ids");
    let corps = json!({"type": "memoire", "hote": "nas-ids", "nom": "NAS",
        "identifiants": {"utilisateur": "lea", "mot_de_passe": "MotDePasse-Tres-Secret"}});

    identifiants::oublier_la_cle_de_l_hote();
    let (s, o, _) = appel(
        &state,
        "POST",
        "/api/v1/sources-reseau",
        Some(corps.clone()),
    )
    .await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(json_de(&o)["code"], "cle_de_l_hote_absente");

    identifiants::installer_la_cle_de_l_hote([42u8; 32]);
    let (s, o, _) = appel(&state, "POST", "/api/v1/sources-reseau", Some(corps)).await;
    assert_eq!(s, StatusCode::CREATED, "{}", String::from_utf8_lossy(&o));
    let v = json_de(&o);
    assert_eq!(v["identifiants"], true);
    assert_eq!(v["disponible"], true);
    assert!(
        !String::from_utf8_lossy(&o).contains("MotDePasse"),
        "rendu par la route"
    );

    let rows = state
        .backend
        .query_many("SELECT * FROM sources_reseau", &[])
        .unwrap();
    let tout = format!("{rows:?}");
    assert!(
        !tout.contains("MotDePasse-Tres-Secret"),
        "mot de passe en clair dans la base"
    );
    assert!(!tout.contains("lea"), "utilisateur en clair dans la base");
    let (_, o, _) = appel(&state, "GET", "/api/v1/sources-reseau", None).await;
    assert!(!String::from_utf8_lossy(&o).contains("MotDePasse"));
    // Aucun fichier de clé n'a été écrit à côté de la base.
    assert!(!_d.path().join(tune_core::coffre::FICHIER_CLE).exists());
    identifiants::oublier_la_cle_de_l_hote();
}

/// La passerelle sert un fichier distant par plages, au seul porteur du jeton
/// de CE fichier, même quand l'authentification est activée.
#[tokio::test(flavor = "multi_thread")]
async fn la_passerelle_sert_des_plages_au_porteur_du_jeton() {
    let (state, _d) = etat(false);
    let m = faux_nas("nas-passerelle");
    let octets: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
    m.ajouter("Musique/A/01.flac", octets.clone(), 1);
    m.ajouter("Musique/A/02.flac", vec![9; 10], 1);
    let (s, o, _) = appel(
        &state,
        "POST",
        "/api/v1/sources-reseau",
        Some(json!({"type": "memoire", "hote": "nas-passerelle", "racine": "Musique"})),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED);
    let id = json_de(&o)["id"].as_str().unwrap().to_string();

    let (s, o, _) = appel(
        &state,
        "GET",
        &format!("/api/v1/sources-reseau/{id}/parcourir?chemin=A"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(json_de(&o)["entrees"].as_array().unwrap().len(), 2);

    let (s, o, _) = appel(
        &state,
        "POST",
        &format!("/api/v1/sources-reseau/{id}/lien?chemin=A/01.flac"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let lien = json_de(&o)["chemin"].as_str().unwrap().to_string();

    // L'authentification activée ne ferme pas la passerelle…
    tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone())
        .set("auth_enabled", "true")
        .unwrap();
    let (s, o, h) =
        appel_entetes(&state, "GET", &lien, None, &[("range", "bytes=1000-1009")]).await;
    assert_eq!(s, StatusCode::PARTIAL_CONTENT);
    assert_eq!(o, octets[1000..1010]);
    assert_eq!(h["content-range"], "bytes 1000-1009/300000");
    assert_eq!(h["content-type"], "audio/flac");
    let lus = m.octets_lus();
    assert!(
        lus < octets.len() as u64,
        "{lus} : le fichier a été lu en entier pour 10 octets"
    );

    let (s, o, _) = appel(&state, "GET", &lien, None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(o, octets);

    // … mais le reste de l'API, si.
    let (s, _, _) = appel(
        &state,
        "GET",
        &format!("/api/v1/sources-reseau/{id}/parcourir"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    // Le jeton d'un fichier n'ouvre pas son voisin, ni un jeton forgé.
    let voisin = lien.replace("A/01.flac", "A/02.flac");
    let (s, _, _) = appel(&state, "GET", &voisin, None).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, _, _) = appel(
        &state,
        "GET",
        &format!("/api/v1/sources-reseau/{id}/fichier?chemin=Musique/A/01.flac&jeton=1.x"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
}

/// Lance un scan, en attendant au besoin qu'un autre scan du processus (une
/// autre épreuve qui tourne en parallèle) rende le droit.
async fn lancer_un_scan(state: &AppState, cible: Option<String>) {
    for _ in 0..1200 {
        if crate::routes::system::scan::spawn_library_scan(state.clone(), false, cible.clone())
            .await
        {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    panic!("aucun scan n'a pu être lancé en 120 s");
}

async fn attendre_pistes(state: &AppState, prefixe: &str, n: usize) -> Vec<(i64, String)> {
    let fin = std::time::Instant::now() + std::time::Duration::from_secs(120);
    loop {
        let rows = state
            .backend
            .query_many("SELECT id, file_path FROM tracks ORDER BY file_path", &[])
            .unwrap();
        let p: Vec<(i64, String)> = rows
            .iter()
            .filter_map(|r| Some((r.first()?.as_i64()?, r.get(1)?.as_string()?)))
            .filter(|(_, f)| f.starts_with(prefixe))
            .collect();
        let fini = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone())
            .get("scan_status")
            .ok()
            .flatten()
            .as_deref()
            != Some("scanning");
        if (p.len() >= n && fini) || std::time::Instant::now() > fin {
            return p;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
}

/// De bout en bout : une source en mémoire est indexée par le scan ordinaire,
/// la piste se lit par sa route de lecture, et une source coupée garde ses
/// pistes (indisponibles, pas supprimées).
#[tokio::test(flavor = "multi_thread")]
async fn une_source_distante_s_indexe_se_lit_et_survit_a_une_coupure() {
    let (state, _d) = etat(false);
    let m = faux_nas("nas-scan");
    m.ajouter(
        "Musique/Album/01.flac",
        flac(2 * 1024 * 1024),
        1_700_000_000,
    );
    m.ajouter("Musique/Album/02.flac", flac(0), 1_700_000_000);
    m.ajouter("Musique/Album/notes.txt", vec![1, 2, 3], 1);
    let (s, o, _) = appel(
        &state,
        "POST",
        "/api/v1/sources-reseau",
        Some(json!({"type": "memoire", "hote": "nas-scan", "racine": "Musique"})),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED);
    let id = json_de(&o)["id"].as_str().unwrap().to_string();
    let prefixe = format!("tune-src://{id}/");

    let (s, _, _) = appel(
        &state,
        "POST",
        &format!("/api/v1/sources-reseau/{id}/tester"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let (s, o, _) = appel(
        &state,
        "POST",
        &format!("/api/v1/sources-reseau/{id}/rescanner"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::ACCEPTED);
    if json_de(&o)["lance"] != true {
        // Un autre scan du processus (autre épreuve) tenait le droit.
        lancer_un_scan(&state, json_de(&o)["racine"].as_str().map(str::to_string)).await;
    }
    let pistes = attendre_pistes(&state, &prefixe, 2).await;
    assert_eq!(
        pistes.iter().map(|(_, f)| f.as_str()).collect::<Vec<_>>(),
        vec![
            format!("{prefixe}Musique/Album/01.flac").as_str(),
            format!("{prefixe}Musique/Album/02.flac").as_str(),
        ],
        "les deux FLAC distants doivent être indexés"
    );
    // Le scan ouvre chaque fichier plusieurs fois (étiquettes, étiquettes
    // étendues, pochette, empreinte) : le cache de blocs partagé fait qu'un
    // bloc ne passe qu'une fois sur le réseau, et le fichier n'est jamais lu
    // en entier.
    let total = 2 * 1024 * 1024 + 2 * flac(0).len() as u64;
    let plafond = 4 * tune_core::sources_reseau::lecteur::BLOC + flac(0).len() as u64;
    assert!(
        m.octets_lus() <= plafond && m.octets_lus() < total,
        "{} octets lus pour indexer {total} (plafond {plafond}) : copie complète ou blocs relus ?",
        m.octets_lus()
    );
    let (_, o, _) = appel(&state, "GET", &format!("/api/v1/sources-reseau/{id}"), None).await;
    assert_eq!(json_de(&o)["etat"], "a_jour");

    // La piste se lit par la route ordinaire (celle de la DIDL des renderers).
    let (piste, _) = &pistes[1];
    let (s, o, _) = appel_entetes(
        &state,
        "GET",
        &format!("/api/v1/library/tracks/{piste}/audio"),
        None,
        &[("range", "bytes=0-3")],
    )
    .await;
    assert!(
        s == StatusCode::PARTIAL_CONTENT || s == StatusCode::OK,
        "{s}"
    );
    assert_eq!(&o[..4], b"fLaC");

    // Coupure : un scan complet ne supprime rien, la source est dite injoignable.
    m.couper(true);
    lancer_un_scan(&state, None).await;
    let apres = attendre_pistes(&state, &prefixe, 2).await;
    assert_eq!(
        apres.len(),
        2,
        "une source coupée ne doit pas perdre ses pistes"
    );
    let (_, o, _) = appel(&state, "GET", &format!("/api/v1/sources-reseau/{id}"), None).await;
    assert_eq!(json_de(&o)["etat"], "injoignable");
    m.couper(false);
}

/// Créer, supprimer, tester, rescanner, parcourir et signer un lien sont des
/// gestes d'administration, comme monter un partage (`/network/mounts`) :
/// authentification active, un profil non-admin reçoit 403 et rien ne bouge ;
/// l'admin garde le comportement d'avant.
#[tokio::test(flavor = "multi_thread")]
async fn les_routes_de_gestion_exigent_l_admin() {
    const SECRET: &str = "secret-epreuve-8202";
    let (state, _d) = etat(false);
    let m = faux_nas("nas-admin");
    m.ajouter("Musique/A/01.flac", vec![1; 10], 1);
    let reglages = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    reglages.set("auth_enabled", "true").unwrap();
    reglages.set("jwt_secret", SECRET).unwrap();
    let porteur = |role: &str, id: i64| {
        format!(
            "Bearer {}",
            crate::auth::sign_jwt(id, role, SECRET).unwrap()
        )
    };
    let admin = porteur("admin", 1);
    let usager = porteur("user", 2);
    let corps = json!({"type": "memoire", "hote": "nas-admin", "racine": "Musique"});

    // Création : refusée au profil non-admin, sans rien écrire.
    let (s, _, _) = appel_entetes(
        &state,
        "POST",
        "/api/v1/sources-reseau",
        Some(corps.clone()),
        &[("authorization", &usager)],
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    assert_eq!(repo_compte(&state), 0);

    let (s, o, _) = appel_entetes(
        &state,
        "POST",
        "/api/v1/sources-reseau",
        Some(corps),
        &[("authorization", &admin)],
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{}", String::from_utf8_lossy(&o));
    let id = json_de(&o)["id"].as_str().unwrap().to_string();

    // Les cinq autres gestes : 403 pour le profil non-admin.
    let gestes = [
        ("POST", format!("/api/v1/sources-reseau/{id}/tester")),
        ("POST", format!("/api/v1/sources-reseau/{id}/rescanner")),
        (
            "GET",
            format!("/api/v1/sources-reseau/{id}/parcourir?chemin=A"),
        ),
        (
            "POST",
            format!("/api/v1/sources-reseau/{id}/lien?chemin=A/01.flac"),
        ),
        ("DELETE", format!("/api/v1/sources-reseau/{id}")),
    ];
    for (methode, uri) in &gestes {
        let (s, _, _) =
            appel_entetes(&state, methode, uri, None, &[("authorization", &usager)]).await;
        assert_eq!(s, StatusCode::FORBIDDEN, "{methode} {uri}");
    }
    assert_eq!(repo_compte(&state), 1, "le refus a supprimé la source");

    // L'admin : comportement inchangé.
    let a = [("authorization", admin.as_str())];
    let (s, o, _) = appel_entetes(&state, "POST", &gestes[0].1, None, &a).await;
    assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&o));
    assert_eq!(json_de(&o)["ok"], true);
    let (s, o, _) = appel_entetes(&state, "GET", &gestes[2].1, None, &a).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(json_de(&o)["entrees"].as_array().unwrap().len(), 1);
    let (s, o, _) = appel_entetes(&state, "POST", &gestes[3].1, None, &a).await;
    assert_eq!(s, StatusCode::OK);
    assert!(json_de(&o)["chemin"].as_str().is_some());
    // Rescanner une source inconnue (pas de scan lancé pendant les autres
    // épreuves) : la porte laisse passer, la route répond 404 comme avant.
    let (s, _, _) = appel_entetes(
        &state,
        "POST",
        "/api/v1/sources-reseau/inconnue/rescanner",
        None,
        &a,
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _, _) = appel_entetes(&state, "DELETE", &gestes[4].1, None, &a).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    assert_eq!(repo_compte(&state), 0);
}

fn repo_compte(state: &AppState) -> usize {
    state
        .backend
        .query_many("SELECT id FROM sources_reseau", &[])
        .unwrap()
        .len()
}
