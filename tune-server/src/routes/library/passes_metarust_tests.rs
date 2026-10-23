//! Témoins des passes de lot du lot 2 MetaRust (#4805).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;
use tune_core::db::backend::{DbBackend, ToSqlValue};
use tune_core::metadata::discogs_propositions::Empechement;

use super::*;
use crate::state::AppState;

fn etat() -> AppState {
    AppState::new(":memory:", 0, Default::default()).unwrap()
}

fn exec(b: &Arc<dyn DbBackend>, sql: &str) {
    b.execute(sql, &[]).unwrap();
}

fn ids(b: &Arc<dyn DbBackend>, sql: &str, apres: i64) -> Vec<i64> {
    b.query_many(sql, &[&apres as &dyn ToSqlValue])
        .unwrap()
        .iter()
        .filter_map(|r| r.first().and_then(|v| v.as_i64()))
        .collect()
}

async fn appeler(e: &AppState, methode: &str, chemin: &str) -> (StatusCode, Value) {
    let r = super::super::router()
        .with_state(e.clone())
        .oneshot(
            Request::builder()
                .method(methode)
                .uri(chemin)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let s = r.status();
    let o = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
    (s, serde_json::from_slice(&o).unwrap_or(Value::Null))
}

/// La sélection BPM ne prend que les albums LOCAUX qui ont au moins une piste
/// à estimer : un fichier (pas une URL), sans balise BPM, sans estimation
/// gardée. Elle compte ces pistes, et le curseur la fait repartir après lui.

/// #6862 : `BPM_EN_VOL` est un drapeau GLOBAL du binaire de tests. Deux tests
/// qui lancent `/bpm-all` en parallèle se voyaient l'un l'autre « en vol »
/// (409 `passe_deja_en_cours` alors que la passe était `done`). Les tests qui
/// lancent une passe BPM la prennent à tour de rôle.
static PASSE_BPM_SERIALISEE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[test]
fn la_selection_bpm_ne_prend_que_les_pistes_a_estimer() {
    let e = etat();
    let b = &e.backend;
    exec(
        b,
        "INSERT INTO albums (id, title, source) VALUES \
         (1, 'Balisé', 'local'), (2, 'Déjà estimé', 'local'), (3, 'Distant', 'upnp'), \
         (4, 'Flux', 'local'), (5, 'À faire', 'local'), (6, 'Mixte', 'local')",
    );
    exec(
        b,
        "INSERT INTO tracks (id, title, album_id, file_path, bpm, source) VALUES \
         (11, 'a', 1, '/m/11.flac', 120, 'local'), \
         (21, 'b', 2, '/m/21.flac', NULL, 'local'), \
         (31, 'c', 3, '/m/31.flac', NULL, 'upnp'), \
         (41, 'd', 4, 'http://radio/x', NULL, 'local'), \
         (51, 'e', 5, '/m/51.flac', NULL, 'local'), \
         (52, 'f', 5, '/m/52.flac', NULL, 'local'), \
         (61, 'g', 6, '/m/61.flac', 98, 'local'), \
         (62, 'h', 6, '/m/62.flac', NULL, 'local')",
    );
    exec(
        b,
        "INSERT INTO bpm_estimes (file_path, bpm, source, mesure_le) \
         VALUES ('/m/21.flac', 100, 'signal', '2026-10-13T00:00:00Z')",
    );
    let sql = sql_candidats_bpm();
    assert_eq!(
        ids(b, &sql, 0),
        vec![5, 6],
        "seuls 5 et 6 ont une piste à estimer"
    );
    let pistes: Vec<i64> = b
        .query_many(&sql, &[&0_i64 as &dyn ToSqlValue])
        .unwrap()
        .iter()
        .filter_map(|r| r.get(1).and_then(|v| v.as_i64()))
        .collect();
    assert_eq!(pistes, vec![2, 1], "pistes à estimer par album");
    assert_eq!(
        ids(b, &sql, 5),
        vec![6],
        "le curseur reprend après le dernier album"
    );
}

/// Discogs : albums locaux SANS MBID, avec un code, sans proposition (même
/// rejetée). Les albums sans code sont comptés à part, pour l'écran.
#[test]
fn la_selection_discogs_ne_prend_que_les_albums_servables() {
    let e = etat();
    let b = &e.backend;
    exec(
        b,
        "INSERT INTO albums (id, title, source, musicbrainz_release_id, barcode, catalog_number) VALUES \
         (1, 'Identifié', 'local', 'rel-1', '0077774600127', NULL), \
         (2, 'Code-barres', 'local', NULL, '0077774600127', NULL), \
         (3, 'Catalogue', 'local', '', NULL, 'CDP 7 46001 2'), \
         (4, 'Sans code', 'local', NULL, NULL, '  '), \
         (5, 'Déjà proposé', 'local', NULL, '5099902987620', NULL), \
         (6, 'Distant', 'upnp', NULL, '5099902987620', NULL), \
         (7, 'Sans rien', 'local', NULL, NULL, NULL)",
    );
    exec(
        b,
        "INSERT INTO propositions_discogs \
         (album_id, discogs_release_id, methode, proposition, statut, cree_le) \
         VALUES (5, 99, 'barcode', '{}', 'rejetee', '2026-10-13T00:00:00Z')",
    );
    let seuil = tune_core::metadata::discogs_propositions::seuil_de_fraicheur(chrono::Utc::now());
    assert_eq!(ids(b, &sql_candidats_discogs(&seuil), 0), vec![2, 3]);
    let sans_code = b
        .query_one(&sql_sans_code_discogs(), &[])
        .unwrap()
        .and_then(|r| r.first().and_then(|v| v.as_i64()));
    assert_eq!(sans_code, Some(2), "albums 4 et 7 : sans MBID et sans code");
}

/// Douze PANNES d'affilée arrêtent la passe et le disent ; des albums sans
/// résultat, si nombreux soient-ils, ne l'arrêtent pas.
#[tokio::test]
async fn douze_pannes_arretent_la_passe_mais_pas_douze_absences() {
    static EN_VOL: AtomicBool = AtomicBool::new(false);
    static PASSE: Passe = Passe {
        mode: "essai",
        cle_etat: "essai_lot_status",
        en_vol: &EN_VOL,
        libelle: "essai",
        raison_panne: "essai_injoignable",
    };
    let e = etat();
    // 20 absences, puis des pannes : la passe traverse les absences et
    // s'arrête à la douzième panne.
    let albums: Vec<i64> = (1..=40).collect();
    let a = derouler(
        e.backend.clone(),
        &PASSE,
        "t".into(),
        albums,
        Avancement {
            total: 40,
            ..Default::default()
        },
        |id| async move {
            if id <= 20 {
                issue_discogs(Ok(None), None)
            } else {
                issue_discogs(Err(Empechement::Panne), Some("status_503"))
            }
        },
    )
    .await;
    assert_eq!(a.traites, 32, "20 absences + 12 pannes, puis l'arrêt");
    assert_eq!(a.compte["sans_resultat"], 20);
    assert_eq!(a.compte["pannes"], 12);
    let s = etat_servi(&e.backend, &PASSE);
    assert_eq!(s["status"], "stopped");
    assert_eq!(s["raison"], "essai_injoignable");
    assert_eq!(s["dernier_album_id"], 32);
    // La reprise repart après le dernier album traité.
    assert_eq!(curseur_de_reprise(&s), 32);
}

/// Un jeton refusé arrête la passe AU PREMIER album : continuer ne ferait que
/// répéter le refus.
#[tokio::test]
async fn un_jeton_refuse_arrete_la_passe_aussitot() {
    static EN_VOL: AtomicBool = AtomicBool::new(false);
    static PASSE: Passe = Passe {
        mode: "essai2",
        cle_etat: "essai2_lot_status",
        en_vol: &EN_VOL,
        libelle: "essai",
        raison_panne: "essai_injoignable",
    };
    let e = etat();
    let vus = Arc::new(AtomicUsize::new(0));
    let v = vus.clone();
    derouler(
        e.backend.clone(),
        &PASSE,
        "t".into(),
        vec![1, 2, 3],
        Avancement::default(),
        move |_| {
            v.fetch_add(1, Ordering::SeqCst);
            async { issue_discogs(Err(Empechement::Panne), Some("status_401")) }
        },
    )
    .await;
    assert_eq!(vus.load(Ordering::SeqCst), 1);
    let s = etat_servi(&e.backend, &PASSE);
    assert_eq!(s["raison"], "discogs_jeton_refuse");
}

/// Un état `running` sans passe en vol (serveur redémarré) se lit
/// `interrupted`, et se reprend après le dernier album.
#[test]
fn une_passe_coupee_par_un_redemarrage_se_reprend() {
    static EN_VOL: AtomicBool = AtomicBool::new(false);
    static PASSE: Passe = Passe {
        mode: "essai3",
        cle_etat: "essai3_lot_status",
        en_vol: &EN_VOL,
        libelle: "essai",
        raison_panne: "x",
    };
    let e = etat();
    let a = Avancement {
        total: 9,
        traites: 4,
        dernier_album_id: 17,
        ..Default::default()
    };
    ecrire_etat(&e.backend, &PASSE, "t", "running", &a, None);
    let s = etat_servi(&e.backend, &PASSE);
    assert_eq!(s["status"], "interrupted");
    assert_eq!(curseur_de_reprise(&s), 17);
    // Une passe terminée repart de zéro.
    ecrire_etat(&e.backend, &PASSE, "t", "done", &a, None);
    assert_eq!(curseur_de_reprise(&etat_servi(&e.backend, &PASSE)), 0);
}

/// Sans jeton Discogs, la passe ne part pas : `412`, comme la fiche.
#[tokio::test]
async fn sans_jeton_la_passe_discogs_ne_part_pas() {
    let e = etat();
    let (s, v) = appeler(&e, "POST", "/discogs-all").await;
    assert_eq!(s, StatusCode::PRECONDITION_FAILED, "{v}");
    assert_eq!(v["code"], "discogs_token_absent");
    assert!(!DISCOGS_EN_VOL.load(Ordering::SeqCst));
}

/// Un fichier WAV mono 16 bits : un clic tous les `periode` échantillons.
fn ecrire_metronome(chemin: &std::path::Path, bpm: f64, secondes: u32) {
    let sr = 22_050u32;
    let n = (sr * secondes) as usize;
    let periode = (sr as f64 * 60.0 / bpm) as usize;
    let mut pcm = vec![0i16; n];
    for i in (0..n).step_by(periode) {
        for k in 0..80.min(n - i) {
            pcm[i + k] = 29_000;
        }
    }
    let data_len = (n * 2) as u32;
    let mut o = Vec::with_capacity(44 + n * 2);
    o.extend_from_slice(b"RIFF");
    o.extend_from_slice(&(36 + data_len).to_le_bytes());
    o.extend_from_slice(b"WAVEfmt ");
    o.extend_from_slice(&16u32.to_le_bytes());
    o.extend_from_slice(&1u16.to_le_bytes()); // PCM
    o.extend_from_slice(&1u16.to_le_bytes()); // mono
    o.extend_from_slice(&sr.to_le_bytes());
    o.extend_from_slice(&(sr * 2).to_le_bytes());
    o.extend_from_slice(&2u16.to_le_bytes());
    o.extend_from_slice(&16u16.to_le_bytes());
    o.extend_from_slice(b"data");
    o.extend_from_slice(&data_len.to_le_bytes());
    for s in pcm {
        o.extend_from_slice(&s.to_le_bytes());
    }
    std::fs::write(chemin, o).unwrap();
}

/// De bout en bout, par la ROUTE : `POST /library/bpm-all` choisit l'album,
/// décode le vrai fichier par le décodeur de Tune et garde 120 BPM « signal »
/// — sans toucher la balise. Relancée, la passe n'a plus rien à faire.
#[tokio::test]
async fn la_passe_bpm_mesure_un_vrai_fichier_par_la_route() {
    let _seul = PASSE_BPM_SERIALISEE.lock().await;
    let dossier = tempfile::tempdir().unwrap();
    let chemin = dossier.path().join("metronome.wav");
    ecrire_metronome(&chemin, 120.0, 20);
    let chemin = chemin.to_string_lossy().into_owned();
    let e = etat();
    let b = &e.backend;
    exec(b, "INSERT INTO albums (id, title) VALUES (7, 'Métronome')");
    b.execute(
        "INSERT INTO tracks (id, title, album_id, track_number, file_path) VALUES (71, 'Clic', 7, 1, ?)",
        &[&chemin as &dyn ToSqlValue],
    )
    .unwrap();

    let (s, v) = appeler(&e, "POST", "/bpm-all").await;
    assert_eq!(s, StatusCode::ACCEPTED, "{v}");
    assert_eq!(v["total"], 1);
    assert_eq!(v["annonce"]["pistes"], 1);

    let mut fin = Value::Null;
    for _ in 0..300 {
        let (_, etat) = appeler(&e, "GET", "/bpm-all/status").await;
        if etat["status"] == "done" {
            fin = etat;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert_eq!(fin["status"], "done", "la passe n'a pas fini : {fin}");
    assert_eq!(fin["compte"]["pistes_signal"], 1, "{fin}");
    let bpm = b
        .query_one(
            "SELECT bpm, source FROM bpm_estimes WHERE file_path = ?",
            &[&chemin as &dyn ToSqlValue],
        )
        .unwrap()
        .unwrap();
    let mesure = bpm[0].as_f64().unwrap();
    assert!(
        (mesure - 120.0).abs() <= 3.0,
        "attendu ~120, obtenu {mesure}"
    );
    assert_eq!(bpm[1].as_string().as_deref(), Some("signal"));
    let balise = b
        .query_one("SELECT bpm FROM tracks WHERE id = 71", &[])
        .unwrap()
        .unwrap();
    assert!(
        balise[0].as_f64().is_none(),
        "la balise n'est jamais écrite"
    );

    // Le drapeau « en vol » tombe juste après l'écriture de `done`.
    for _ in 0..100 {
        if !BPM_EN_VOL.load(Ordering::SeqCst) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let (s, v) = appeler(&e, "POST", "/bpm-all").await;
    assert_eq!(s, StatusCode::ACCEPTED, "{v}");
    assert_eq!(v["total"], 0, "relancée, la passe n'a plus rien à estimer");
}

/// Conditions de Discogs : une proposition EN ATTENTE de plus de six heures
/// est relue par la passe ; une proposition fraîche ou tranchée ne l'est pas.
#[test]
fn une_proposition_perimee_est_relue_par_la_passe() {
    let e = etat();
    let b = &e.backend;
    exec(
        b,
        "INSERT INTO albums (id, title, source, barcode) VALUES \
         (1, 'Périmée', 'local', '0077774600127'), (2, 'Fraîche', 'local', '0077774600127'), \
         (3, 'Rejetée hier', 'local', '0077774600127')",
    );
    exec(
        b,
        "INSERT INTO propositions_discogs \
         (album_id, discogs_release_id, methode, proposition, statut, cree_le) VALUES \
         (1, 11, 'barcode', '{}', 'proposee', '2026-10-13T01:00:00Z'), \
         (2, 22, 'barcode', '{}', 'proposee', '2026-10-13T09:00:00Z'), \
         (3, 33, 'barcode', '{}', 'rejetee', '2026-10-12T09:00:00Z')",
    );
    let midi = chrono::DateTime::parse_from_rfc3339("2026-10-13T12:00:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    let seuil = tune_core::metadata::discogs_propositions::seuil_de_fraicheur(midi);
    assert_eq!(seuil, "2026-10-13T06:00:00Z");
    assert_eq!(ids(b, &sql_candidats_discogs(&seuil), 0), vec![1]);
}

/// Décision de Bertrand du 13/10/2026 : les passes BPM et Discogs sont
/// GRATUITES. Dans un même état sans Premium, `identify-all` (Premium) rend
/// `402`, les deux passes jamais.
#[tokio::test]
async fn les_passes_sont_gratuites() {
    let _seul = PASSE_BPM_SERIALISEE.lock().await;
    let e = etat();
    assert!(
        !e.license
            .check_feature(tune_core::license::Feature::AutoEnrichment)
            .await,
        "l'état de test doit être sans Premium"
    );
    let (s, _) = appeler(&e, "POST", "/identify-all").await;
    assert_eq!(
        s,
        StatusCode::PAYMENT_REQUIRED,
        "identify-all reste Premium"
    );
    for chemin in ["/bpm-all", "/discogs-all"] {
        let (s, v) = appeler(&e, "POST", chemin).await;
        assert_ne!(s, StatusCode::PAYMENT_REQUIRED, "{chemin} : {v}");
        assert_ne!(v["code"], "premium_required", "{chemin} : {v}");
    }
    // La passe lancée ici tourne encore en tâche de fond : on attend qu'elle
    // rende le drapeau global avant de rendre le verrou (#6862).
    for _ in 0..500 {
        if !BPM_EN_VOL.load(Ordering::SeqCst) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}
