//! #5871 — la préparation d'une lecture DLNA pendant un scan (Tades, fil 2160).
//!
//! `load_streaming_dsp` lit une vingtaine de réglages (PURE, droits des
//! greffons, profil d'égaliseur, crossfeed, ReplayGain, compensation) et la
//! fiche de la zone. Toutes ces lectures passaient par la CONNEXION
//! D'ÉCRITURE (`SettingsRepo::get` → `query_one_strong`, `ZoneRepo::get` →
//! `query_many_strong`). Un scan qui écrit sans relâche la reprend aussitôt
//! rendue : le `Mutex` standard n'est pas équitable, et chaque lecture
//! attendait la fin d'une phase d'écriture entière. Une vingtaine d'attentes
//! de ce genre font les 9 à 13 s de la troisième tranche du rapport.
//!
//! ⚠️ Base de FICHIER obligatoire : sur `:memory:`, le pool de lecture EST la
//! connexion d'écriture, et l'épreuve ne mesurerait rien.
use super::PlaybackOrchestrator;
use crate::db::backend::{DbBackend, ToSqlValue};
use crate::db::settings_repo::SettingsRepo;
use crate::db::zone_repo::ZoneRepo;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

const DEVICE: &str = "uuid:diretta-ddc-0-c19-5871";

/// La borne de l'épreuve : une préparation DSP qui ne lit que des réglages
/// ne doit pas coûter plus d'une demi-seconde, scan ou pas. Avant le
/// correctif, sous le même écrivain, elle en coûtait plusieurs.
const BORNE: Duration = Duration::from_millis(500);

/// Ce que l'écrivain simulé garde la connexion à chaque prise.
const DETENTION: Duration = Duration::from_millis(150);

struct Banc {
    orch: PlaybackOrchestrator,
    db: Arc<dyn DbBackend>,
    sqlite: Arc<crate::db::sqlite::SqliteDb>,
    zone_id: i64,
    _dossier: crate::test_scratch::ScratchDir,
}

/// Base de fichier peuplée de `pistes` pistes (une album sur dix), une zone
/// DLNA avec égaliseur, crossfeed, ReplayGain d'album et compensation : tous
/// les étages que `load_streaming_dsp` lit.
fn banc(etiquette: &str, pistes: i64) -> Banc {
    let dossier = crate::test_scratch::scratch_dir(&format!("prepa-lecture-5871-{etiquette}"));
    let chemin = dossier.join("tune-banc.db");
    let sqlite = crate::db::sqlite::SqliteDb::open(&chemin.to_string_lossy()).unwrap();
    sqlite.init_schema().unwrap();
    crate::db::migrations::run_migrations(&sqlite).unwrap();
    {
        let mut conn = sqlite.connection().lock().unwrap();
        let tx = conn.transaction().unwrap();
        tx.execute("INSERT INTO artists (id, name) VALUES (1, 'A')", [])
            .unwrap();
        {
            let mut album = tx
                .prepare("INSERT INTO albums (id, title, artist_id) VALUES (?1, ?2, 1)")
                .unwrap();
            for a in 1..=(pistes / 10).max(1) {
                album
                    .execute(rusqlite::params![a, format!("Album {a}")])
                    .unwrap();
            }
            let mut piste = tx
                .prepare(
                    "INSERT INTO tracks (id, title, album_id, artist_id, file_path, format, \
                     duration_ms, sample_rate, bit_depth, channels) \
                     VALUES (?1, ?2, ?3, 1, ?4, 'flac', 240000, 44100, 16, 2)",
                )
                .unwrap();
            let mut meta = tx
                .prepare("INSERT INTO track_metadata (track_id, key, value) VALUES (?1, ?2, ?3)")
                .unwrap();
            for t in 1..=pistes {
                let album_id = (t - 1) / 10 + 1;
                piste
                    .execute(rusqlite::params![
                        t,
                        format!("Piste {t}"),
                        album_id,
                        format!("/media/music/music2/{album_id}/{t}.flac")
                    ])
                    .unwrap();
                meta.execute(rusqlite::params![t, "rg_track_gain", "-6.50 dB"])
                    .unwrap();
                meta.execute(rusqlite::params![t, "rg_album_gain", "-7.20 dB"])
                    .unwrap();
            }
        }
        tx.commit().unwrap();
    }
    let sqlite = Arc::new(sqlite);
    let db: Arc<dyn DbBackend> = sqlite.clone();
    let orch = PlaybackOrchestrator::new(
        db.clone(),
        Arc::new(crate::playback::PlaybackManager::new()),
        Arc::new(crate::http::streamer::AudioStreamer::new(0)),
        Arc::new(tokio::sync::Mutex::new(
            crate::streaming::registry::ServiceRegistry::new(),
        )),
        Arc::new(tokio::sync::Mutex::new(
            crate::outputs::registry::OutputRegistry::new(),
        )),
        None,
    );
    let zone_id = ZoneRepo::with_backend(db.clone())
        .create("DDC-0 C19", Some("dlna"), Some(DEVICE))
        .unwrap();
    let s = SettingsRepo::with_backend(db.clone());
    s.set("plugin_equalizer_installed", "true").unwrap();
    s.set(
        &format!("zone_{zone_id}_eq_profile"),
        &serde_json::to_string(&crate::audio::eq::EqProfile {
            enabled: true,
            bands: vec![crate::audio::eq::EqBandSpec {
                freq: 1000.0,
                gain: 4.0,
                q: 1.0,
                band_type: "peak".into(),
                ..Default::default()
            }],
            ..Default::default()
        })
        .unwrap(),
    )
    .unwrap();
    s.set(
        &format!("zone_{zone_id}_crossfeed"),
        r#"{"enabled":true,"amount":0.3,"delay_ms":0.3}"#,
    )
    .unwrap();
    s.set(crate::audio::replaygain::MODE_KEY, "album").unwrap();
    Banc {
        orch,
        db,
        sqlite,
        zone_id,
        _dossier: dossier,
    }
}

/// Le témoin que le banc est monté : tous les étages sont armés. Sans lui,
/// une préparation rapide pourrait n'être qu'une préparation VIDE.
fn tous_les_etages_sont_armes(b: &Banc) {
    let dsp = b.orch.load_streaming_dsp(b.zone_id, Some(1), 44_100, 2);
    assert!(dsp.replaygain.is_some(), "ReplayGain d'album non armé");
    assert!(dsp.eq.is_some(), "égaliseur non armé");
    assert!(dsp.crossfeed.is_some(), "crossfeed non armé");
    assert!(dsp.compensation.is_some(), "compensation non armée");
}

/// Un écrivain qui ne lâche la connexion d'écriture que le temps d'une
/// respiration : il la tient [`DETENTION`], la rend, la reprend aussitôt —
/// ce que fait un lot de scan dans sa phase d'écriture. Rend un drapeau
/// d'arrêt et le fil ; `tient` est prévenu à la première prise.
fn ecrivain_qui_ne_lache_pas(
    sqlite: Arc<crate::db::sqlite::SqliteDb>,
) -> (Arc<AtomicBool>, std::thread::JoinHandle<()>) {
    let arret = Arc::new(AtomicBool::new(false));
    let (tient, premiere_prise) = mpsc::channel::<()>();
    let a = arret.clone();
    let fil = std::thread::spawn(move || {
        let mut prevenu = false;
        while !a.load(Ordering::Relaxed) {
            let conn = sqlite.connection().lock().unwrap();
            conn.execute(
                "UPDATE tracks SET duration_ms = duration_ms WHERE id = 2",
                [],
            )
            .unwrap();
            if !prevenu {
                tient.send(()).unwrap();
                prevenu = true;
            }
            std::thread::sleep(DETENTION);
            drop(conn);
        }
    });
    premiere_prise.recv().unwrap();
    (arret, fil)
}

/// 🔴 LA PRÉPARATION DSP NE FAIT PLUS LA QUEUE DERRIÈRE L'ÉCRIVAIN.
#[test]
fn load_streaming_dsp_reste_sous_la_borne_pendant_des_ecritures() {
    let b = banc("borne", 2_000);
    tous_les_etages_sont_armes(&b);
    let (arret, fil) = ecrivain_qui_ne_lache_pas(b.sqlite.clone());
    let mut pire = Duration::ZERO;
    for _ in 0..5 {
        let debut = Instant::now();
        let dsp = b.orch.load_streaming_dsp(b.zone_id, Some(1), 44_100, 2);
        pire = pire.max(debut.elapsed());
        assert!(dsp.eq.is_some() && dsp.replaygain.is_some());
    }
    arret.store(true, Ordering::Relaxed);
    fil.join().unwrap();
    assert!(
        pire < BORNE,
        "load_streaming_dsp a pris {} ms pendant que l'écrivain tenait la connexion \
         (borne {} ms) : ses lectures attendent encore la connexion d'écriture",
        pire.as_millis(),
        BORNE.as_millis()
    );
}

/// LA CONTRE-ÉPREUVE — sans elle, un écrivain qui ne gênerait personne
/// rendrait l'épreuve verte quoi que fasse le code. Sous le MÊME écrivain, une
/// lecture par la connexion d'écriture attend bel et bien : c'est le chemin
/// qu'empruntaient les réglages avant #5871.
#[test]
fn contre_epreuve_une_lecture_par_l_ecrivain_attend_bien_le_meme_ecrivain() {
    let b = banc("contre-epreuve", 200);
    let (arret, fil) = ecrivain_qui_ne_lache_pas(b.sqlite.clone());
    let debut = Instant::now();
    let cle: &dyn ToSqlValue = &"replaygain_mode";
    let valeur =
        b.db.query_one_strong("SELECT value FROM settings WHERE key = ?1", &[cle])
            .unwrap();
    let attendu = debut.elapsed();
    arret.store(true, Ordering::Relaxed);
    fil.join().unwrap();
    assert!(valeur.is_some());
    assert!(
        attendu >= DETENTION / 3,
        "la lecture forte n'a attendu que {} ms : l'écrivain du banc ne tient pas \
         la connexion, et l'épreuve de la borne ne prouverait rien",
        attendu.as_millis()
    );
}

/// Un réglage posé à l'instant se relit aussitôt, écrivain libre : la
/// lecture fraîche n'a rien perdu de ce que garantissait la lecture forte.
#[test]
fn un_reglage_pose_se_relit_aussitot() {
    let b = banc("relecture", 10);
    let s = SettingsRepo::with_backend(b.db.clone());
    for i in 0..50 {
        let v = format!("valeur-{i}");
        s.set("cle_5871", &v).unwrap();
        assert_eq!(s.get("cle_5871").unwrap().as_deref(), Some(v.as_str()));
    }
}

/// Le banc du terrain : 600 000 pistes (`TUNE_BANC_5871_PISTES`), un scan
/// simulé qui écrit par lots de 500 dans `BEGIN IMMEDIATE`, une respiration de
/// 20 ms entre deux lots. Imprime la durée de `load_streaming_dsp` ; n'affirme
/// rien. `cargo test -p tune-core --lib banc_5871 -- --ignored --nocapture`.
#[test]
#[ignore = "banc de mesure, 600 000 pistes"]
fn banc_5871_load_streaming_dsp_pendant_un_scan() {
    let pistes: i64 = std::env::var("TUNE_BANC_5871_PISTES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(600_000);
    let t = Instant::now();
    let b = banc("terrain", pistes);
    eprintln!(
        "banc_5871 base de {pistes} pistes en {} s",
        t.elapsed().as_secs()
    );
    tous_les_etages_sont_armes(&b);
    let mesurer = |etiquette: &str| {
        let mut d: Vec<u128> = (0..10)
            .map(|_| {
                let t = Instant::now();
                let _ = b.orch.load_streaming_dsp(b.zone_id, Some(4242), 44_100, 2);
                t.elapsed().as_millis()
            })
            .collect();
        d.sort();
        eprintln!(
            "banc_5871 {etiquette}: médiane {} ms, pire {} ms, toutes {:?}",
            d[d.len() / 2],
            d[d.len() - 1],
            d
        );
    };
    mesurer("sans scan");
    let arret = Arc::new(AtomicBool::new(false));
    let a = arret.clone();
    let db = b.db.clone();
    let scan = std::thread::spawn(move || {
        let mut id: i64 = 1;
        let mut lots = 0u64;
        while !a.load(Ordering::Relaxed) {
            db.execute_batch("BEGIN IMMEDIATE").unwrap();
            for _ in 0..500 {
                let titre = format!("Piste {id} rescannée");
                db.execute(
                    "UPDATE tracks SET title = ?1 WHERE id = ?2",
                    &[&titre as &dyn ToSqlValue, &id],
                )
                .unwrap();
                id = id % pistes + 1;
            }
            db.execute_batch("COMMIT").unwrap();
            lots += 1;
            std::thread::sleep(Duration::from_millis(20));
        }
        eprintln!("banc_5871 scan simulé : {lots} lots de 500");
    });
    std::thread::sleep(Duration::from_millis(500));
    mesurer("pendant le scan");
    arret.store(true, Ordering::Relaxed);
    scan.join().unwrap();
}
