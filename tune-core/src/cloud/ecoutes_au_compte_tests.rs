//! #6510 — l'envoi des écoutes au compte, éprouvé contre un faux site.
//!
//! Chaque test passe par [`passe`], le seul chemin que la tâche de fond
//! appelle, et lit ce que le faux site a REÇU : le corps tel qu'il est parti,
//! pas une charge reconstruite à côté.

use std::sync::{Arc, Mutex};

use chrono::TimeZone;
use serde_json::{Value, json};

use super::*;
use crate::db::migrations;
use crate::db::sqlite::SqliteDb;

const SERVEUR: &str = "serveur-a";

fn base() -> Arc<dyn DbBackend> {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    migrations::run_migrations(&db).unwrap();
    Arc::new(db)
}

fn reglages(b: &Arc<dyn DbBackend>) -> SettingsRepo {
    SettingsRepo::with_backend(b.clone())
}

fn maintenant() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 19, 12, 0, 0).unwrap()
}

/// Une piste locale avec ISRC et MBID, sur un album, et un fichier.
fn bibliotheque(b: &Arc<dyn DbBackend>) {
    b.execute_batch(
        "INSERT INTO artists (id, name) VALUES (20, 'Miles Davis');\
         INSERT INTO albums (id, title, artist_id) VALUES (10, 'Kind of Blue', 20);\
         INSERT INTO tracks (id, title, album_id, artist_id, file_path, format, source, \
                             isrc, musicbrainz_recording_id, duration_ms) \
         VALUES (1, 'So What', 10, 20, '/srv/musique/secret/01.flac', 'flac', 'local', \
                 'USSM15900113', 'a1b2c3d4-0000-4000-8000-000000000001', 562000);",
    )
    .unwrap();
}

/// Une écoute dans `listen_history`, telle que `record_listen` l'écrit.
#[allow(clippy::too_many_arguments)]
fn ecoute(
    b: &Arc<dyn DbBackend>,
    id: i64,
    titre: &str,
    album_id: Option<i64>,
    source: &str,
    quand: &str,
    profil: Option<i64>,
) {
    b.execute(
        "INSERT INTO listen_history (id, title, artist_name, album_title, source, source_id, \
         album_id, duration_ms, listened_at, cover_url, profile_id) \
         VALUES (?, ?, 'Miles Davis', 'Kind of Blue', ?, 'qobuz-12345', ?, 562000, ?, \
                 '/srv/pochettes/secret.jpg', ?)",
        &[
            &id as &dyn ToSqlValue,
            &titre.to_string() as &dyn ToSqlValue,
            &source.to_string() as &dyn ToSqlValue,
            &album_id as &dyn ToSqlValue,
            &quand.to_string() as &dyn ToSqlValue,
            &profil as &dyn ToSqlValue,
        ],
    )
    .unwrap();
}

#[derive(Default)]
struct FauxSite {
    /// Statuts à rendre, dans l'ordre ; vide : 200.
    reponses: Vec<(u16, Value)>,
    /// Corps reçus sur `POST /api/v1/listens`.
    corps: Vec<Value>,
    /// En-têtes `X-Tune-Server-Token` reçus.
    jetons: Vec<Option<String>>,
    /// Appels de liaison reçus.
    liaisons: usize,
    /// Identifiants d'événement déjà vus (le dédoublonnage du site).
    vus: std::collections::HashSet<String>,
}

type Site = Arc<Mutex<FauxSite>>;

async fn faux_site() -> (String, Site) {
    use axum::extract::{Path, State};
    use axum::http::{HeaderMap, StatusCode};
    use axum::routing::post;
    use axum::{Json, Router};

    async fn ecoutes(
        State(s): State<Site>,
        entetes: HeaderMap,
        Json(corps): Json<Value>,
    ) -> (StatusCode, Json<Value>) {
        let mut s = s.lock().unwrap();
        s.corps.push(corps.clone());
        s.jetons.push(
            entetes
                .get("x-tune-server-token")
                .and_then(|v| v.to_str().ok())
                .map(str::to_string),
        );
        if !s.reponses.is_empty() {
            let (statut, corps) = s.reponses.remove(0);
            return (StatusCode::from_u16(statut).unwrap(), Json(corps));
        }
        let mut acceptees = 0;
        let mut doublons = 0;
        for e in corps["listens"].as_array().unwrap() {
            if s.vus.insert(e["event_id"].as_str().unwrap().to_string()) {
                acceptees += 1;
            } else {
                doublons += 1;
            }
        }
        (
            StatusCode::OK,
            Json(json!({"accepted": acceptees, "duplicates": doublons, "rejected": 0})),
        )
    }
    async fn lier(State(s): State<Site>, Path(id): Path<String>) -> Json<Value> {
        s.lock().unwrap().liaisons += 1;
        Json(json!({"server_id": id, "token": "liaison-neuve"}))
    }
    let etat: Site = Arc::new(Mutex::new(FauxSite::default()));
    let app = Router::new()
        .route("/api/v1/listens", post(ecoutes))
        .route("/api/v1/cloud-library/{id}/link", post(lier))
        .with_state(etat.clone());
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let adresse = ecoute.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(ecoute, app).await.ok();
    });
    (format!("http://{adresse}"), etat)
}

fn relier(b: &Arc<dyn DbBackend>, base_url: &str) {
    let s = reglages(b);
    s.set("mozaik_base_url", base_url).unwrap();
    s.set("mozaik_access_token", "jeton-de-test").unwrap();
    s.set("server_id", SERVEUR).unwrap();
    s.set(
        crate::cloud::library_sync::CLE_JETON_DE_LIAISON,
        "liaison-de-test",
    )
    .unwrap();
}

fn activer(b: &Arc<dyn DbBackend>) {
    reglages(b).set(CLE_ACTIVATION, "true").unwrap();
}

fn http() -> reqwest::Client {
    crate::http::client::shared().clone()
}

fn curseur(b: &Arc<dyn DbBackend>) -> Option<String> {
    reglages(b).get(CLE_CURSEUR).unwrap()
}

/// Le lot construit : les champs promis, et rien de ce qui ne doit jamais
/// partir.
#[tokio::test]
async fn le_lot_porte_le_morceau_et_rien_de_la_machine() {
    let (url, site) = faux_site().await;
    let b = base();
    bibliotheque(&b);
    ecoute(
        &b,
        1,
        "So What",
        Some(10),
        "local",
        "2026-10-18T20:00:00Z",
        Some(3),
    );
    ecoute(
        &b,
        2,
        "France Inter",
        None,
        "radio",
        "2026-10-18T21:00:00Z",
        Some(3),
    );
    ecoute(
        &b,
        3,
        "Blue in Green",
        None,
        "qobuz",
        "2026-10-18T22:00:00Z",
        None,
    );
    relier(&b, &url);
    activer(&b);

    let issue = passe(&b, &http(), maintenant()).await;
    assert_eq!(
        issue,
        Issue::Envoye {
            lots: 1,
            ecoutes: 2,
            doublons: 0
        }
    );

    let site = site.lock().unwrap();
    assert_eq!(site.corps.len(), 1);
    let corps = &site.corps[0];
    assert_eq!(corps["server_id"], json!(SERVEUR));
    assert_eq!(site.jetons[0].as_deref(), Some("liaison-de-test"));
    let ecoutes = corps["listens"].as_array().unwrap();
    assert_eq!(ecoutes.len(), 2, "la radio ne part pas : {corps}");

    let so_what = &ecoutes[0];
    assert_eq!(so_what["listened_at"], json!("2026-10-18T20:00:00Z"));
    assert_eq!(so_what["duration_ms"], json!(562000));
    assert_eq!(so_what["track"]["isrc"], json!("USSM15900113"));
    assert_eq!(
        so_what["track"]["mbid"],
        json!("a1b2c3d4-0000-4000-8000-000000000001")
    );
    assert_eq!(so_what["track"]["title"], json!("So What"));
    assert_eq!(so_what["track"]["artist"], json!("Miles Davis"));
    assert_eq!(so_what["track"]["album"], json!("Kind of Blue"));
    assert_eq!(so_what["track"]["duration_ms"], json!(562000));
    assert_eq!(
        so_what["event_id"],
        json!(identifiant_d_evenement(
            SERVEUR,
            1,
            "2026-10-18T20:00:00Z",
            "So What"
        ))
    );
    assert_eq!(so_what["profile"], json!(empreinte_de_profil(SERVEUR, 3)));
    assert_eq!(so_what["profile"].as_str().unwrap().len(), 16);

    // Une piste de service : pas d'ISRC connu, pas de profil.
    assert_eq!(ecoutes[1]["track"]["isrc"], Value::Null);
    assert_eq!(ecoutes[1]["profile"], Value::Null);

    // Rien de la machine : ni chemin, ni pochette, ni identifiant de service,
    // ni numéro de profil, ni zone.
    let texte = corps.to_string();
    for interdit in ["/srv/", "secret", "qobuz-12345", "zone", "\"profile\":3"] {
        assert!(
            !texte.contains(interdit),
            "« {interdit} » est parti : {texte}"
        );
    }
    let mut cles: Vec<&String> = so_what.as_object().unwrap().keys().collect();
    cles.sort();
    assert_eq!(
        cles,
        ["duration_ms", "event_id", "listened_at", "profile", "track"],
        "une clé inattendue est partie"
    );
}

/// #6738 — les MBID de la release et de l'artiste partent avec l'écoute
/// quand ils sont connus (site-mozaiklabs#330) ; inconnus : `null`.
#[tokio::test]
async fn les_mbid_d_album_et_d_artiste_partent_avec_l_ecoute() {
    let (url, site) = faux_site().await;
    let b = base();
    bibliotheque(&b);
    b.execute_batch(
        "UPDATE artists SET musicbrainz_id = '561d854a-6a28-4aa7-8c99-323e6ce46c2a' WHERE id = 20;\
         UPDATE albums SET musicbrainz_release_id = 'b84ee12a-09ef-421b-82de-0441a926375b' \
         WHERE id = 10;",
    )
    .unwrap();
    ecoute(
        &b,
        1,
        "So What",
        Some(10),
        "local",
        "2026-10-18T20:00:00Z",
        None,
    );
    ecoute(
        &b,
        2,
        "Blue in Green",
        None,
        "qobuz",
        "2026-10-18T22:00:00Z",
        None,
    );
    relier(&b, &url);
    activer(&b);
    assert!(matches!(
        passe(&b, &http(), maintenant()).await,
        Issue::Envoye { ecoutes: 2, .. }
    ));

    let site = site.lock().unwrap();
    let ecoutes = site.corps[0]["listens"].as_array().unwrap();
    assert_eq!(
        ecoutes[0]["track"]["release_mbid"],
        json!("b84ee12a-09ef-421b-82de-0441a926375b")
    );
    assert_eq!(
        ecoutes[0]["track"]["artist_mbid"],
        json!("561d854a-6a28-4aa7-8c99-323e6ce46c2a")
    );
    assert_eq!(ecoutes[1]["track"]["release_mbid"], Value::Null);
    assert_eq!(ecoutes[1]["track"]["artist_mbid"], Value::Null);
}

/// Désactivé, ou sans compte : rien ne part, et le site ne voit rien.
#[tokio::test]
async fn rien_ne_part_sans_le_reglage_ni_sans_compte() {
    let (url, site) = faux_site().await;
    let b = base();
    ecoute(
        &b,
        1,
        "So What",
        None,
        "local",
        "2026-10-18T20:00:00Z",
        None,
    );

    // Relié, mais réglage absent (le défaut).
    relier(&b, &url);
    assert_eq!(
        passe(&b, &http(), maintenant()).await,
        Issue::Rien("disabled")
    );
    // Réglage explicitement faux, ou illisible.
    for v in ["false", "0", "peut-être"] {
        reglages(&b).set(CLE_ACTIVATION, v).unwrap();
        assert_eq!(
            passe(&b, &http(), maintenant()).await,
            Issue::Rien("disabled")
        );
    }

    // Activé, mais aucune session de compte.
    activer(&b);
    reglages(&b).delete("mozaik_access_token").unwrap();
    assert_eq!(
        passe(&b, &http(), maintenant()).await,
        Issue::Rien("account_not_linked")
    );
    // Activé, session, mais pas de `server_id`.
    relier(&b, &url);
    reglages(&b).delete("server_id").unwrap();
    assert_eq!(
        passe(&b, &http(), maintenant()).await,
        Issue::Rien("account_not_linked")
    );

    let site = site.lock().unwrap();
    assert!(site.corps.is_empty(), "le site a reçu : {:?}", site.corps);
    assert_eq!(site.liaisons, 0);
    drop(site);
    assert_eq!(curseur(&b), None, "aucun curseur posé sans envoi autorisé");
}

/// Le défaut publié est bien « désactivé ».
#[test]
fn le_reglage_est_desactive_par_defaut() {
    const { assert!(!ACTIVATION_DEFAUT) };
    let b = base();
    assert!(!envoi_active(&reglages(&b)));
    reglages(&b).set(CLE_ACTIVATION, "true").unwrap();
    assert!(envoi_active(&reglages(&b)));
}

/// Dédoublonnage : un second passage n'envoie rien ; une écoute nouvelle part
/// seule ; l'identifiant d'événement est stable.
#[tokio::test]
async fn un_second_passage_n_envoie_que_le_nouveau() {
    let (url, site) = faux_site().await;
    let b = base();
    ecoute(
        &b,
        1,
        "So What",
        None,
        "local",
        "2026-10-18T20:00:00Z",
        None,
    );
    ecoute(
        &b,
        2,
        "Freddie Freeloader",
        None,
        "local",
        "2026-10-18T20:10:00Z",
        None,
    );
    relier(&b, &url);
    activer(&b);

    assert!(matches!(
        passe(&b, &http(), maintenant()).await,
        Issue::Envoye { ecoutes: 2, .. }
    ));
    assert_eq!(curseur(&b).as_deref(), Some("2"));
    assert_eq!(
        passe(&b, &http(), maintenant()).await,
        Issue::Rien("nothing_new")
    );

    ecoute(
        &b,
        3,
        "Blue in Green",
        None,
        "local",
        "2026-10-18T20:20:00Z",
        None,
    );
    assert!(matches!(
        passe(&b, &http(), maintenant()).await,
        Issue::Envoye { ecoutes: 1, .. }
    ));
    let site = site.lock().unwrap();
    assert_eq!(site.corps.len(), 2);
    let second = site.corps[1]["listens"].as_array().unwrap();
    assert_eq!(second.len(), 1);
    assert_eq!(second[0]["track"]["title"], json!("Blue in Green"));

    // Stable, et propre à la ligne ET à sa date.
    let a = identifiant_d_evenement(SERVEUR, 1, "2026-10-18T20:00:00Z", "So What");
    assert_eq!(
        a,
        identifiant_d_evenement(SERVEUR, 1, "2026-10-18T20:00:00Z", "So What")
    );
    assert_ne!(
        a,
        identifiant_d_evenement(SERVEUR, 1, "2026-10-19T20:00:00Z", "So What")
    );
    assert_ne!(
        a,
        identifiant_d_evenement("serveur-b", 1, "2026-10-18T20:00:00Z", "So What")
    );
    assert_eq!(a.len(), 32);
}

/// Réessai : un refus transitoire (500), un 429 ou un site injoignable
/// laissent le curseur en place ; le passage suivant renvoie LE MÊME lot,
/// avec les mêmes identifiants, et le site le dédoublonne.
#[tokio::test]
async fn un_echec_ne_fait_pas_avancer_le_curseur_et_le_lot_repart() {
    let (url, site) = faux_site().await;
    let b = base();
    ecoute(
        &b,
        1,
        "So What",
        None,
        "local",
        "2026-10-18T20:00:00Z",
        None,
    );
    relier(&b, &url);
    activer(&b);

    site.lock()
        .unwrap()
        .reponses
        .push((500, json!({"error": "boom"})));
    assert!(matches!(
        passe(&b, &http(), maintenant()).await,
        Issue::Echec(_)
    ));
    assert_eq!(
        curseur(&b).as_deref(),
        Some("0"),
        "le curseur a avancé sur un 500"
    );

    // Hors ligne : le site ne répond plus du tout.
    reglages(&b)
        .set("mozaik_base_url", "http://127.0.0.1:9")
        .unwrap();
    assert_eq!(
        passe(&b, &http(), maintenant()).await,
        Issue::Echec("network".into())
    );
    assert_eq!(curseur(&b).as_deref(), Some("0"));

    // De retour en ligne : le lot repart et passe.
    reglages(&b).set("mozaik_base_url", &url).unwrap();
    assert!(matches!(
        passe(&b, &http(), maintenant()).await,
        Issue::Envoye { ecoutes: 1, .. }
    ));
    assert_eq!(curseur(&b).as_deref(), Some("1"));

    let site = site.lock().unwrap();
    assert_eq!(site.corps.len(), 2, "un envoi refusé, un envoi accepté");
    assert_eq!(
        site.corps[0]["listens"][0]["event_id"], site.corps[1]["listens"][0]["event_id"],
        "le lot renvoyé doit porter les mêmes identifiants"
    );
}

/// Une réponse perdue après acceptation : le lot repart, le site compte des
/// doublons et n'ajoute rien.
#[tokio::test]
async fn un_lot_renvoye_est_dedoublonne_par_le_site() {
    let (url, site) = faux_site().await;
    let b = base();
    ecoute(
        &b,
        1,
        "So What",
        None,
        "local",
        "2026-10-18T20:00:00Z",
        None,
    );
    relier(&b, &url);
    activer(&b);
    assert!(matches!(
        passe(&b, &http(), maintenant()).await,
        Issue::Envoye { doublons: 0, .. }
    ));
    // Le serveur « oublie » qu'il a envoyé (curseur remis comme après une
    // réponse perdue).
    reglages(&b).set(CLE_CURSEUR, "0").unwrap();
    assert_eq!(
        passe(&b, &http(), maintenant()).await,
        Issue::Envoye {
            lots: 1,
            ecoutes: 1,
            doublons: 1
        }
    );
    assert_eq!(site.lock().unwrap().vus.len(), 1);
}

/// Jeton de liaison refusé : le serveur se relie UNE fois et renvoie le lot
/// avec le jeton neuf.
#[tokio::test]
async fn un_jeton_de_liaison_refuse_fait_relier_une_fois() {
    let (url, site) = faux_site().await;
    let b = base();
    ecoute(
        &b,
        1,
        "So What",
        None,
        "local",
        "2026-10-18T20:00:00Z",
        None,
    );
    relier(&b, &url);
    activer(&b);
    site.lock()
        .unwrap()
        .reponses
        .push((401, json!({"error": "server_token_invalid"})));

    assert!(matches!(
        passe(&b, &http(), maintenant()).await,
        Issue::Envoye { ecoutes: 1, .. }
    ));
    let site = site.lock().unwrap();
    assert_eq!(site.liaisons, 1);
    assert_eq!(
        site.jetons,
        vec![
            Some("liaison-de-test".to_string()),
            Some("liaison-neuve".to_string())
        ]
    );
}

/// Rattrapage borné : rien d'avant la fenêtre de 365 jours, et au plus
/// `RATTRAPAGE_MAX` écoutes — les plus récentes.
#[tokio::test]
async fn le_rattrapage_est_borne_en_date_et_en_nombre() {
    let (url, site) = faux_site().await;
    let b = base();
    // Une écoute d'il y a deux ans, puis RATTRAPAGE_MAX + 5 écoutes récentes.
    ecoute(
        &b,
        1,
        "Trop vieille",
        None,
        "local",
        "2024-10-01T10:00:00Z",
        None,
    );
    let total = RATTRAPAGE_MAX + 5;
    b.execute(
        "WITH RECURSIVE n(i) AS (SELECT 2 UNION ALL SELECT i + 1 FROM n WHERE i < ?) \
         INSERT INTO listen_history (id, title, source, duration_ms, listened_at) \
         SELECT i, 'Piste ' || i, 'local', 1000, '2026-10-01T10:00:00Z' FROM n",
        &[&(total + 1) as &dyn ToSqlValue],
    )
    .unwrap();
    relier(&b, &url);
    activer(&b);

    let s = reglages(&b);
    let (c, plancher) = curseur_et_plancher(&b, &s, maintenant());
    assert_eq!(plancher, "2025-10-19T12:00:00Z");
    // Les 5 plus anciennes récentes (2..=6) restent locales, la vieille aussi.
    assert_eq!(c, 6);

    // Tout le rattrapage part, sur plusieurs passages.
    let mut passages = 0;
    while let Issue::Envoye { .. } = passe(&b, &http(), maintenant()).await {
        passages += 1;
        assert!(passages < 20, "le rattrapage ne finit pas");
    }
    let site = site.lock().unwrap();
    let titres: Vec<String> = site
        .corps
        .iter()
        .flat_map(|c| c["listens"].as_array().unwrap().clone())
        .map(|e| e["track"]["title"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(titres.len() as i64, RATTRAPAGE_MAX);
    assert!(!titres.contains(&"Trop vieille".to_string()));
    assert!(!titres.contains(&"Piste 6".to_string()));
    assert!(titres.contains(&"Piste 7".to_string()));
    assert!(
        site.corps
            .iter()
            .all(|c| { c["listens"].as_array().unwrap().len() as i64 <= TAILLE_DU_LOT })
    );
}

// ---------------------------------------------------------------------------
// #6840 — le consentement à trois états.
// ---------------------------------------------------------------------------

/// LE témoin de #6840 : sur un serveur relié, avec des écoutes à envoyer,
/// RIEN ne part tant que le consentement n'est pas « accepté » — ni quand il
/// est à demander, ni après « Non », ni après deux fermetures sans réponse.
/// Seul « Oui » fait partir le lot.
#[tokio::test]
async fn rien_ne_part_avant_un_oui_6840() {
    let (url, site) = faux_site().await;
    let b = base();
    bibliotheque(&b);
    ecoute(
        &b,
        1,
        "So What",
        Some(10),
        "local",
        "2026-10-18T20:00:00Z",
        Some(1),
    );
    relier(&b, &url);
    let s = reglages(&b);

    // Jamais demandé : à demander, rien ne part.
    assert_eq!(consentement(&s), Consentement::ADemander);
    assert_eq!(
        passe(&b, &http(), maintenant()).await,
        Issue::Rien("disabled"),
        "à demander : l'envoi est parti sans réponse"
    );
    // Une fermeture sans réponse : toujours à demander, toujours rien.
    enregistrer_reponse(&s, Reponse::Fermee, maintenant()).unwrap();
    assert_eq!(consentement(&s), Consentement::ADemander);
    assert_eq!(
        passe(&b, &http(), maintenant()).await,
        Issue::Rien("disabled")
    );
    // Deuxième fermeture : refusé, rien.
    enregistrer_reponse(&s, Reponse::Fermee, maintenant()).unwrap();
    assert_eq!(consentement(&s), Consentement::Refuse);
    assert_eq!(
        passe(&b, &http(), maintenant()).await,
        Issue::Rien("disabled")
    );
    // « Non » explicite : rien.
    enregistrer_reponse(&s, Reponse::Non, maintenant()).unwrap();
    assert_eq!(
        passe(&b, &http(), maintenant()).await,
        Issue::Rien("disabled")
    );
    {
        let site = site.lock().unwrap();
        assert!(
            site.corps.is_empty(),
            "le site a reçu des écoutes sans consentement : {:?}",
            site.corps
        );
        assert_eq!(site.liaisons, 0);
    }
    assert_eq!(curseur(&b), None, "un curseur posé sans consentement");

    // « Oui » : le lot part.
    assert_eq!(
        enregistrer_reponse(&s, Reponse::Oui, maintenant()).unwrap(),
        Consentement::Accepte
    );
    assert!(matches!(
        passe(&b, &http(), maintenant()).await,
        Issue::Envoye { ecoutes: 1, .. }
    ));
    assert_eq!(site.lock().unwrap().corps.len(), 1);
}

/// Fermée sans réponse : redemandée UNE fois, puis plus jamais. La deuxième
/// fermeture date le refus ; une troisième ne change rien.
#[test]
fn une_seule_relance_apres_une_fermeture_6840() {
    let b = base();
    let s = reglages(&b);
    assert_eq!(fermetures_sans_reponse(&s), 0);
    assert_eq!(
        enregistrer_reponse(&s, Reponse::Fermee, maintenant()).unwrap(),
        Consentement::ADemander,
        "la première fermeture doit laisser la question à poser une fois encore"
    );
    assert_eq!(fermetures_sans_reponse(&s), 1);
    assert_eq!(date_du_consentement(&s), None);
    assert_eq!(
        enregistrer_reponse(&s, Reponse::Fermee, maintenant()).unwrap(),
        Consentement::Refuse,
        "la deuxième fermeture doit clore la question"
    );
    assert_eq!(fermetures_sans_reponse(&s), FERMETURES_MAX);
    assert_eq!(
        date_du_consentement(&s).as_deref(),
        Some(maintenant().to_rfc3339().as_str())
    );
    enregistrer_reponse(&s, Reponse::Fermee, maintenant()).unwrap();
    assert_eq!(fermetures_sans_reponse(&s), FERMETURES_MAX);
    assert_eq!(consentement(&s), Consentement::Refuse);
    // Le réglage reste accessible : un « Oui » plus tard vaut.
    enregistrer_reponse(&s, Reponse::Oui, maintenant()).unwrap();
    assert!(envoi_active(&s));
}

/// Un serveur qui avait DÉJÀ coché la bascule avant la rc8 reste accepté,
/// sans re-question ; une fermeture ne lui retire rien. Une bascule décochée
/// avant la rc8 était un geste : refusé, pas « à demander ».
#[test]
fn deja_active_reste_accepte_6840() {
    let b = base();
    let s = reglages(&b);
    s.set(CLE_ACTIVATION, "true").unwrap();
    assert_eq!(consentement(&s), Consentement::Accepte);
    assert_eq!(
        enregistrer_reponse(&s, Reponse::Fermee, maintenant()).unwrap(),
        Consentement::Accepte
    );
    assert_eq!(fermetures_sans_reponse(&s), 0);
    assert!(envoi_active(&s));

    let b = base();
    let s = reglages(&b);
    s.set(CLE_ACTIVATION, "false").unwrap();
    assert_eq!(consentement(&s), Consentement::Refuse);
}

/// La réponse date le choix et garde la bascule en phase ; un « accepté »
/// contredit par une bascule écrite fausse depuis vaut refus.
#[test]
fn la_reponse_est_datee_et_la_bascule_suit_6840() {
    let b = base();
    let s = reglages(&b);
    enregistrer_reponse(&s, Reponse::Oui, maintenant()).unwrap();
    assert_eq!(s.get(CLE_ACTIVATION).unwrap().as_deref(), Some("true"));
    assert_eq!(s.get(CLE_CONSENTEMENT).unwrap().as_deref(), Some("accepte"));
    assert!(date_du_consentement(&s).is_some());
    s.set(CLE_ACTIVATION, "false").unwrap();
    assert_eq!(consentement(&s), Consentement::Refuse);
    enregistrer_reponse(&s, Reponse::Non, maintenant()).unwrap();
    assert_eq!(s.get(CLE_ACTIVATION).unwrap().as_deref(), Some("false"));
    assert_eq!(Reponse::depuis("accepte"), Some(Reponse::Oui));
    assert_eq!(Reponse::depuis("refuse"), Some(Reponse::Non));
    assert_eq!(Reponse::depuis("fermee"), Some(Reponse::Fermee));
    assert_eq!(Reponse::depuis("oui"), None);
}
