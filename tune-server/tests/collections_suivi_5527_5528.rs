//! Les albums manquants d'un dossier « Collections » se résolvent (#5527, #5528).
//!
//! Lulu (JLuc, fil 1891, v0.9.168) :
//!
//! - #5527 — « lorsque je transfère un album manquant de la bibliothèque vers
//!   un répertoire de "Collections", cet album figure encore dans la liste des
//!   albums manquants ». Ranger l'album vivant retire désormais du dossier les
//!   identifiants MORTS du même album (même artiste, même titre, à la casse,
//!   aux accents, à la ponctuation et au numéro de disque près).
//! - #5528 — « des albums qui ont été compilés dans un seul album, en
//!   particulier des opéras ». `GET /collections/{id}/missing` propose, pour
//!   chaque manquant, les albums vivants qui peuvent le remplacer — sans
//!   jamais substituer d'office — et `merged_into` dit l'album qui a reçu ses
//!   pistes quand le scan l'a noté.
//!
//! Le suivi par le scan lui-même (pistes retaguées, purge des albums vidés)
//! est gardé dans `tune-core` (`dossiers_des_collections_tests.rs`) et, par le
//! VRAI scan de fichiers, dans `rescan_reunit_deux_disques_5528` ci-dessous.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::artist_repo::ArtistRepo;
use tune_core::db::settings_repo::SettingsRepo;
use tune_server::state::AppState;

fn app_et_etat() -> (axum::Router, AppState) {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    (tune_server::routes::router(state.clone()), state)
}

async fn appel(
    app: &axum::Router,
    methode: &str,
    route: &str,
    corps: Option<Value>,
) -> (StatusCode, Value) {
    let mut r = Request::builder().method(methode).uri(route);
    let body = match corps {
        Some(c) => {
            r = r.header("Content-Type", "application/json");
            Body::from(c.to_string())
        }
        None => Body::empty(),
    };
    let rep = app.clone().oneshot(r.body(body).unwrap()).await.unwrap();
    let statut = rep.status();
    let o = axum::body::to_bytes(rep.into_body(), usize::MAX)
        .await
        .unwrap();
    (statut, serde_json::from_slice(&o).unwrap_or(Value::Null))
}

fn album(state: &AppState, artiste: &str, titre: &str) -> i64 {
    let a = ArtistRepo::with_backend(state.backend.clone())
        .get_or_create(artiste, None, None)
        .unwrap();
    AlbumRepo::with_backend(state.backend.clone())
        .get_or_create(titre, a.id.unwrap(), None)
        .unwrap()
        .id
        .unwrap()
}

async fn creer_dossier(app: &axum::Router) -> i64 {
    let (st, col) = appel(
        app,
        "POST",
        "/api/v1/library/collections",
        Some(json!({"name": "Opéras"})),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{col}");
    col["id"].as_i64().unwrap()
}

async fn ranger(app: &axum::Router, cid: i64, aid: i64) -> Value {
    let (st, v) = appel(
        app,
        "POST",
        &format!("/api/v1/library/collections/{cid}/albums/{aid}"),
        Some(json!({})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "rangement de {aid}: {v}");
    v
}

async fn dossier_servi(app: &axum::Router, cid: i64) -> Value {
    let (_, liste) = appel(app, "GET", "/api/v1/library/collections", None).await;
    liste
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"].as_i64() == Some(cid))
        .unwrap()
        .clone()
}

fn ids_manquants(dossier: &Value) -> Vec<i64> {
    dossier["orphan_albums"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|a| a["id"].as_i64())
        .collect()
}

fn stocke(state: &AppState, cid: i64) -> Value {
    let brut = SettingsRepo::with_backend(state.backend.clone())
        .get("collections")
        .unwrap()
        .unwrap();
    serde_json::from_str::<Vec<Value>>(&brut)
        .unwrap()
        .into_iter()
        .find(|c| c["id"].as_i64() == Some(cid))
        .unwrap()
}

/// 🔴 #5527 — L'ÉPREUVE QUI TRANCHE : ranger l'album vivant fait sortir de la
/// liste des manquants ses anciens identifiants — et eux seuls.
#[tokio::test]
async fn ranger_l_album_vivant_retire_ses_identifiants_morts() {
    let (app, state) = app_et_etat();
    let cid = creer_dossier(&app).await;
    let cd1 = album(&state, "Maria Callas", "Tosca, CD1");
    let cd2 = album(&state, "Maria Callas", "Tosca (Disc 2)");
    let norma = album(&state, "Maria Callas", "Norma");
    let tebaldi = album(&state, "Renata Tebaldi", "Tosca");
    for id in [cd1, cd2, norma, tebaldi] {
        ranger(&app, cid, id).await;
    }
    let repo = AlbumRepo::with_backend(state.backend.clone());
    for id in [cd1, cd2, norma, tebaldi] {
        repo.delete(id).unwrap();
    }
    let avant = dossier_servi(&app, cid).await;
    assert_eq!(ids_manquants(&avant).len(), 4, "{avant}");

    // Le rescan a réuni les disques : l'album vivant, sous un nouvel id.
    let tosca = album(&state, "MARIA CALLAS", "Tosca");
    let rep = ranger(&app, cid, tosca).await;
    let mut remplaces: Vec<i64> = rep["replaced_album_ids"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_i64())
        .collect();
    remplaces.sort();
    assert_eq!(remplaces, vec![cd1, cd2], "{rep}");

    let apres = dossier_servi(&app, cid).await;
    let mut restent = ids_manquants(&apres);
    restent.sort();
    assert_eq!(
        restent,
        {
            let mut v = vec![norma, tebaldi];
            v.sort();
            v
        },
        "un autre titre, ou le même titre d'un AUTRE artiste, reste manquant : {apres}"
    );
    assert_eq!(apres["album_ids"], json!([tosca]));
    let etiquettes = stocke(&state, cid)["album_labels"].clone();
    assert!(etiquettes.get(cd1.to_string()).is_none(), "{etiquettes}");
    assert!(etiquettes.get(cd2.to_string()).is_none(), "{etiquettes}");
}

/// Un identifiant VIVANT du même titre n'est jamais retiré ; un album déjà
/// rangé, rangé une seconde fois, fait quand même le ménage de ses morts.
#[tokio::test]
async fn un_vivant_n_est_jamais_retire_et_le_second_rangement_fait_le_menage() {
    let (app, state) = app_et_etat();
    let cid = creer_dossier(&app).await;
    let tosca = album(&state, "Maria Callas", "Tosca");
    let jumeau = album(&state, "Maria Callas", "Tosca, CD2");
    let mort = album(&state, "Maria Callas", "Tosca, CD1");
    for id in [tosca, jumeau, mort] {
        ranger(&app, cid, id).await;
    }
    AlbumRepo::with_backend(state.backend.clone())
        .delete(mort)
        .unwrap();

    let rep = ranger(&app, cid, tosca).await;
    assert_eq!(rep["replaced_album_ids"], json!([mort]), "{rep}");
    let apres = dossier_servi(&app, cid).await;
    assert_eq!(
        apres["album_ids"],
        json!([tosca, jumeau]),
        "le vivant reste : {apres}"
    );
    assert_eq!(apres["orphan_album_count"], json!(0));
}

/// #5528 — les remplaçants PROPOSÉS d'un manquant : même artiste, même titre
/// au numéro de disque près. Rien n'est remplacé par la lecture.
#[tokio::test]
async fn les_manquants_proposent_leurs_remplacants_sans_rien_remplacer() {
    let (app, state) = app_et_etat();
    let cid = creer_dossier(&app).await;
    let cd1 = album(&state, "Maria Callas", "Tosca, CD1");
    let norma = album(&state, "Maria Callas", "Norma");
    let inconnu = album(&state, "Maria Callas", "Médée");
    for id in [cd1, norma, inconnu] {
        ranger(&app, cid, id).await;
    }
    let repo = AlbumRepo::with_backend(state.backend.clone());
    repo.delete(cd1).unwrap();
    repo.delete(inconnu).unwrap();
    let tosca = album(&state, "Maria Callas", "Tosca");
    album(&state, "Renata Tebaldi", "Tosca");

    let (st, manquants) = appel(
        &app,
        "GET",
        &format!("/api/v1/library/collections/{cid}/missing"),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{manquants}");
    let liste = manquants.as_array().unwrap();
    assert_eq!(liste.len(), 2, "{manquants}");
    let de = |id: i64| {
        liste
            .iter()
            .find(|m| m["id"].as_i64() == Some(id))
            .unwrap()
            .clone()
    };
    assert_eq!(
        de(cd1)["candidates"],
        json!([{ "id": tosca, "title": "Tosca", "artist": "Maria Callas", "in_collection": false }]),
        "un seul remplaçant : celui de Tebaldi n'est pas du même artiste"
    );
    assert_eq!(de(cd1)["title"], json!("Tosca, CD1"));
    assert_eq!(de(inconnu)["candidates"], json!([]));

    let avant = stocke(&state, cid);
    assert_eq!(
        avant["album_ids"],
        json!([cd1, norma, inconnu]),
        "la lecture ne remplace rien"
    );

    let (st, _) = appel(&app, "GET", "/api/v1/library/collections/999/missing", None).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}

/// #5528 — un manquant dont le scan a noté l'album d'arrivée le dit, dans la
/// liste des dossiers comme dans `/missing`. Plusieurs arrivées vivantes :
/// c'est un partage, `merged_into` reste nul.
#[tokio::test]
async fn un_manquant_dit_l_album_qui_a_recu_ses_pistes() {
    let (app, state) = app_et_etat();
    let cid = creer_dossier(&app).await;
    let reuni = album(&state, "Maria Callas", "Tosca — intégrale");
    let partage_a = album(&state, "X", "A");
    let partage_b = album(&state, "X", "B");
    SettingsRepo::with_backend(state.backend.clone())
        .set(
            "collections",
            &json!([{
                "id": cid, "name": "Opéras", "album_ids": [901, 902],
                "album_labels": {
                    "901": {"title": "Tosca, CD1", "artist": "Maria Callas", "merged_into": [reuni]},
                    "902": {"title": "Double", "artist": "X", "merged_into": [partage_a, partage_b]}
                }
            }])
            .to_string(),
        )
        .unwrap();
    let dossier = dossier_servi(&app, cid).await;
    let orphelins = dossier["orphan_albums"].as_array().unwrap();
    assert_eq!(
        orphelins[0]["merged_into"],
        json!({"id": reuni, "title": "Tosca — intégrale", "artist": "Maria Callas"}),
        "{dossier}"
    );
    assert_eq!(orphelins[1]["merged_into"], Value::Null, "{dossier}");
    let (_, manquants) = appel(
        &app,
        "GET",
        &format!("/api/v1/library/collections/{cid}/missing"),
        None,
    )
    .await;
    assert_eq!(
        manquants[0]["merged_into"]["id"],
        json!(reuni),
        "{manquants}"
    );
    assert!(
        dossier.get("album_labels").is_none(),
        "la réserve n'est pas servie"
    );
}

/// La note « réuni dans » d'un album ENCORE vivant survit au relevé des noms
/// que fait l'ouverture du dossier (#901) : elle attend la purge.
#[tokio::test]
async fn ouvrir_le_dossier_n_efface_pas_la_note_du_scan() {
    let (app, state) = app_et_etat();
    let cid = creer_dossier(&app).await;
    let vivant = album(&state, "Maria Callas", "Tosca, CD1");
    let reuni = album(&state, "Maria Callas", "Tosca");
    SettingsRepo::with_backend(state.backend.clone())
        .set(
            "collections",
            &json!([{
                "id": cid, "name": "Opéras", "album_ids": [vivant],
                "album_labels": {
                    vivant.to_string(): {"title": "Tosca CD1", "artist": "Maria Callas", "merged_into": [reuni]}
                }
            }])
            .to_string(),
        )
        .unwrap();
    let (st, _) = appel(
        &app,
        "GET",
        &format!("/api/v1/library/collections/{cid}/albums"),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let etiquette = stocke(&state, cid)["album_labels"][vivant.to_string()].clone();
    assert_eq!(
        etiquette["title"],
        json!("Tosca, CD1"),
        "le nom est relevé : {etiquette}"
    );
    assert_eq!(
        etiquette["merged_into"],
        json!([reuni]),
        "la note survit : {etiquette}"
    );
}

// ---------------------------------------------------------------------------
// Le VRAI scan : deux disques retagués en un seul album (#5528)
// ---------------------------------------------------------------------------

/// Un FLAC minimal valide pour `lofty` (même fabrique que les bancs #4602 et
/// #4907).
fn flac(balises: &[(&str, &str)], graine: u32) -> Vec<u8> {
    let sr: u64 = 44_100;
    let bits: u64 = 16;
    let mut out = b"fLaC".to_vec();
    out.push(0x00);
    out.extend_from_slice(&[0, 0, 34]);
    out.extend_from_slice(&4096u16.to_be_bytes());
    out.extend_from_slice(&4096u16.to_be_bytes());
    out.extend_from_slice(&[0, 0, 0, 0, 0, 0]);
    let canaux: u64 = 2 - 1;
    let total: u64 = sr * 180;
    let packed: u64 = (sr << 44) | (canaux << 41) | ((bits - 1) << 36) | total;
    out.extend_from_slice(&packed.to_be_bytes());
    out.extend_from_slice(&[0u8; 16]);
    let mut vc = Vec::new();
    let vendeur = b"banc-5528";
    vc.extend_from_slice(&(vendeur.len() as u32).to_le_bytes());
    vc.extend_from_slice(vendeur);
    vc.extend_from_slice(&(balises.len() as u32).to_le_bytes());
    for (k, v) in balises {
        let c = format!("{k}={v}");
        vc.extend_from_slice(&(c.len() as u32).to_le_bytes());
        vc.extend_from_slice(c.as_bytes());
    }
    out.push(0x80 | 0x04);
    let l = vc.len() as u32;
    out.extend_from_slice(&[(l >> 16) as u8, (l >> 8) as u8, l as u8]);
    out.extend_from_slice(&vc);
    let mut x = graine.wrapping_mul(2_654_435_761).wrapping_add(1);
    for _ in 0..(96 * 1024) {
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        out.push(x as u8);
    }
    out
}

async fn scanner(app: &axum::Router) {
    let (s, v) = appel(app, "POST", "/api/v1/system/scan", None).await;
    assert!(s.is_success(), "POST /system/scan → {s} : {v}");
    let mut dernier = Value::Null;
    for _ in 0..600 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        dernier = appel(app, "GET", "/api/v1/system/scan/status", None)
            .await
            .1;
        if dernier["status"] != "scanning" {
            break;
        }
    }
    assert_ne!(dernier["status"], "scanning", "scan jamais terminé");
}

/// Les albums en base : `(id, titre, pistes)`.
fn albums_en_base(state: &AppState) -> Vec<(i64, String, i64)> {
    state
        .backend
        .query_many(
            "SELECT a.id, a.title, (SELECT COUNT(*) FROM tracks t WHERE t.album_id = a.id) \
             FROM albums a ORDER BY a.id",
            &[],
        )
        .unwrap()
        .into_iter()
        .map(|r| {
            (
                r[0].as_i64().unwrap(),
                r[1].as_string().unwrap_or_default(),
                r[2].as_i64().unwrap_or(0),
            )
        })
        .collect()
}

/// Ce que le banc a vu : les albums avant et après le retag, le dossier
/// servi et le dossier stocké.
struct Constat {
    avant: Vec<(i64, String, i64)>,
    apres: Vec<(i64, String, i64)>,
    servi: Value,
    stocke: Value,
}

/// Deux disques d'un opéra, un album par disque, rangés dans un dossier, puis
/// RETAGUÉS pour ne former qu'un album (mêmes fichiers, balise ALBUM commune
/// et DISCNUMBER), puis rescannés par `POST /system/scan`.
///
/// `sans_dossiers_d_album` : la bibliothèque a été indexée avant que Tune ne
/// retienne le dossier de chaque album (`albums.folder_path` nul). Le scan ne
/// reconnaît alors plus l'album par son dossier : il résout le titre retagué
/// vers un AUTRE album et y déplace les pistes — c'est le chemin du scan
/// lui-même (`update_batch` puis la purge des albums vidés), et non celui de
/// la fusion des doublons.
/// L'état du scan est PARTAGÉ par tout le processus : deux bancs qui scannent
/// en même temps se voient répondre `409 already_scanning`. Un seul à la fois.
static UN_SCAN_A_LA_FOIS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn jouer_le_retag(etiquette: &str, sans_dossiers_d_album: bool) -> Constat {
    let _un_seul = UN_SCAN_A_LA_FOIS.lock().await;
    let (app, state) = app_et_etat();
    let base = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("banc-5528-{etiquette}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let racine = base.join("musique");
    let opera = racine.join("Maria Callas");
    // Un dossier et un album par acte : aucun nom de disque, ni dans les
    // dossiers ni dans les titres — la détection des coffrets ne les réunit
    // donc pas d'elle-même au premier scan.
    let disques = ["Tosca - Acte I", "Tosca - Acte II"];
    for d in disques {
        std::fs::create_dir_all(opera.join(d)).unwrap();
    }
    SettingsRepo::with_backend(state.backend.clone())
        .set(
            "music_dirs",
            &serde_json::to_string(&vec![racine.to_string_lossy().into_owned()]).unwrap(),
        )
        .unwrap();

    let titres = ["Vissi d'arte", "E lucevan le stelle"];
    let ecrire = |reuni: bool| {
        for (i, d) in disques.iter().enumerate() {
            let album = if reuni {
                "Tosca".to_string()
            } else {
                disques[i].to_string()
            };
            let mut b: Vec<(&str, String)> = vec![
                ("TITLE", titres[i].to_string()),
                ("ARTIST", "Maria Callas".to_string()),
                ("ALBUMARTIST", "Maria Callas".to_string()),
                ("ALBUM", album),
                ("TRACKNUMBER", "1".to_string()),
            ];
            if reuni {
                b.push(("DISCNUMBER", (i + 1).to_string()));
                b.push(("DISCTOTAL", "2".to_string()));
            }
            let b: Vec<(&str, &str)> = b.iter().map(|(k, v)| (*k, v.as_str())).collect();
            std::fs::write(opera.join(d).join("01.flac"), flac(&b, i as u32 + 1)).unwrap();
        }
    };
    ecrire(false);
    scanner(&app).await;
    let avant = albums_en_base(&state);

    let cid = creer_dossier(&app).await;
    for (id, _, _) in &avant {
        ranger(&app, cid, *id).await;
    }
    if sans_dossiers_d_album {
        state
            .backend
            .execute("UPDATE albums SET folder_path = NULL", &[])
            .unwrap();
    }

    // Retag : la taille des fichiers change, le scan les relit.
    std::thread::sleep(std::time::Duration::from_millis(1100));
    ecrire(true);
    scanner(&app).await;
    let apres = albums_en_base(&state);
    let servi = dossier_servi(&app, cid).await;
    let stocke = stocke(&state, cid);
    let _ = std::fs::remove_dir_all(&base);
    eprintln!(
        "[{etiquette}] avant : {avant:?}\n[{etiquette}] après : {apres:?}\n[{etiquette}] stocké : {stocke}"
    );
    Constat {
        avant,
        apres,
        servi,
        stocke,
    }
}

/// Le dossier ne garde aucun identifiant mort, désigne l'album qui porte
/// désormais les deux pistes, et lui donne son nom.
fn le_dossier_a_suivi(c: &Constat) {
    assert_eq!(
        c.avant.len(),
        2,
        "un album par disque avant le retag : {:?}",
        c.avant
    );
    let vivants: Vec<i64> = c
        .apres
        .iter()
        .filter(|(_, _, n)| *n > 0)
        .map(|(id, _, _)| *id)
        .collect();
    assert_eq!(vivants.len(), 1, "le retag forme UN album : {:?}", c.apres);
    let reuni = vivants[0];
    assert_eq!(
        c.servi["orphan_album_count"],
        json!(0),
        "aucun disque ne reste parmi les manquants : {}",
        c.servi
    );
    assert_eq!(
        c.stocke["album_ids"],
        json!([reuni]),
        "le dossier suit : {}",
        c.stocke
    );
    assert_eq!(
        c.stocke["album_labels"][reuni.to_string()]["title"],
        json!("Tosca"),
        "l'album réuni porte son nom dans le dossier : {}",
        c.stocke
    );
}

/// #5528 — le VRAI scan, bibliothèque récente : les deux dossiers de disques
/// frères se réunissent (coffret), le dossier suit.
#[tokio::test]
async fn rescan_reunit_deux_disques_freres_5528() {
    let c = jouer_le_retag("freres", false).await;
    le_dossier_a_suivi(&c);
}

/// #5528 — le VRAI scan, bibliothèque indexée avant `folder_path` : les
/// pistes retaguées CHANGENT d'album au scan, les albums vidés sont purgés.
/// Sans le suivi, le dossier gardait les deux identifiants morts.
#[tokio::test]
async fn rescan_d_une_bibliotheque_sans_dossiers_d_album_5528() {
    let c = jouer_le_retag("sans-dossiers", true).await;
    let ids_avant: Vec<i64> = c.avant.iter().map(|(id, _, _)| *id).collect();
    assert!(
        c.apres
            .iter()
            .filter(|(_, _, n)| *n > 0)
            .all(|(id, _, _)| !ids_avant.contains(id)),
        "le banc doit faire CHANGER les pistes d'album, sinon il ne garde pas le suivi : {:?}",
        c.apres
    );
    le_dossier_a_suivi(&c);
}
