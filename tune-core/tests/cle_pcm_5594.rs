//! #5594 (lot 1) — la clé du signal PCM des FLAC, `tracks.audio_pcm_key`.
//!
//! Deux propriétés, sur de vrais fichiers :
//!
//! 1. la clé désigne bien le SIGNAL : son MD5 est celui des échantillons que
//!    rend le décodeur de Tune, recalculé ici indépendamment de l'en-tête ;
//! 2. le rattrapage (`taches_de_fond::cle_pcm::rattraper`) pose la clé sur
//!    les FLAC à MD5 réel, aucune clé sur un MD5 nul, une piste CUE, un MP3 ;
//!    il est reprenable, ne relit pas ce qu'il a déjà lu, relit un fichier
//!    changé, et n'écrit jamais dans les fichiers audio.
//!
//! `[[test]]` à lui seul dans `tune-core/Cargo.toml` (`autotests = false`).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use md5::{Digest, Md5};
use tune_core::db::backend::{DbBackend, ToSqlValue};
use tune_core::db::sqlite::SqliteDb;
use tune_core::taches_de_fond::cle_pcm::{Bilan, LOT, rattraper};

fn fixture(nom: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(nom)
}

/// Le MD5 au sens de FLAC : échantillons entrelacés, signés, petit-boutistes,
/// sur `ceil(bits / 8)` octets chacun.
fn md5_du_pcm(samples: &[i32], bits: u16) -> String {
    let largeur = usize::from(bits).div_ceil(8);
    let mut h = Md5::new();
    for s in samples {
        h.update(&s.to_le_bytes()[..largeur]);
    }
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// Propriété 1 : la clé lue dans l'en-tête porte le MD5 du PCM décodé, la
/// durée en échantillons par canal, la cadence, les canaux et la profondeur
/// que le décodeur rend.
#[test]
fn la_cle_est_celle_du_pcm_decode() {
    for nom in [
        "test.flac",
        "flac/ref_16_44100_stereo.flac",
        "flac/ref_24_96000_stereo.flac",
        "flac/ref_16_44100_mono.flac",
    ] {
        let chemin = fixture(nom);
        let cle = tune_core::audio::flac_vendeur::cle_pcm_du_fichier(&chemin)
            .expect("fichier lisible")
            .unwrap_or_else(|| panic!("{nom} : un FLAC à MD5 réel doit avoir une clé"));
        let audio =
            tune_core::audio::decode::decode_to_pcm(chemin.to_str().unwrap(), None, None, 0.0, 0.0)
                .expect("décodage");
        let attendue = format!(
            "flac-md5-v1:{}:{}:{}:{}:{}",
            md5_du_pcm(&audio.samples_i32, audio.bit_depth),
            audio.samples_i32.len() / audio.channels as usize,
            audio.sample_rate,
            audio.channels,
            audio.bit_depth
        );
        assert_eq!(
            cle, attendue,
            "{nom} : la clé ne désigne pas le signal décodé"
        );
    }
}

fn base() -> Arc<dyn DbBackend> {
    let db = SqliteDb::open_in_memory().expect("base");
    db.init_schema().expect("schéma");
    tune_core::db::migrations::run_migrations(&db).expect("migrations");
    Arc::new(db)
}

/// Une piste locale déjà en base.
fn piste(
    b: &Arc<dyn DbBackend>,
    id: i64,
    file_path: Option<&str>,
    cue_media_path: Option<&str>,
    format: &str,
    audio_hash: Option<&str>,
) {
    b.execute(
        "INSERT INTO tracks (id, title, file_path, cue_media_path, cue_start_ms, format, \
         audio_hash, source) VALUES (?, ?, ?, ?, ?, ?, ?, 'local')",
        &[
            &id as &dyn ToSqlValue,
            &format!("piste {id}"),
            &file_path,
            &cue_media_path,
            // Début de tranche distinct par piste : l'identité d'une tranche
            // est `(cue_media_path, cue_start_ms)`, unique.
            &cue_media_path.map(|_| id * 1000),
            &format,
            &audio_hash,
        ],
    )
    .expect("insertion de piste");
}

/// `(audio_pcm_key, audio_pcm_key_seen)` d'une piste.
fn cle(b: &Arc<dyn DbBackend>, id: i64) -> (Option<String>, Option<String>) {
    let r = b
        .query_one(
            "SELECT audio_pcm_key, audio_pcm_key_seen FROM tracks WHERE id = ?",
            &[&id as &dyn ToSqlValue],
        )
        .unwrap()
        .expect("piste");
    (
        r.first().and_then(|v| v.as_string()),
        r.get(1).and_then(|v| v.as_string()),
    )
}

fn copie(dossier: &Path, nom: &str, source: &str) -> String {
    let p = dossier.join(nom);
    std::fs::copy(fixture(source), &p).unwrap();
    p.to_string_lossy().into_owned()
}

const CLE_TEST_FLAC: &str = "flac-md5-v1:e3d5a52400c85f978eebd475ec8a10bf:44100:44100:2:16";

/// Propriété 2, le cas complet : vrai FLAC, FLAC au MD5 nul, piste CUE, MP3,
/// FLAC introuvable.
#[test]
fn le_rattrapage_pose_la_cle_et_rien_qu_elle() {
    let tmp = tempfile::TempDir::new().unwrap();
    let d = tmp.path();
    let b = base();

    let vrai = copie(d, "vrai.flac", "test.flac");
    let mut octets = std::fs::read(fixture("test.flac")).unwrap();
    octets[26..42].fill(0); // MD5 de STREAMINFO mis à zéro, comme ffmpeg.
    let nul = d.join("nul.flac");
    std::fs::write(&nul, &octets).unwrap();
    let nul = nul.to_string_lossy().into_owned();
    let image = copie(d, "image.flac", "test.flac");
    let mp3 = copie(d, "piste.mp3", "test.mp3");
    let absent = d.join("absent.flac").to_string_lossy().into_owned();

    let avant: Vec<(String, Vec<u8>)> = [&vrai, &nul, &image, &mp3]
        .iter()
        .map(|p| (p.to_string(), std::fs::read(p).unwrap()))
        .collect();

    piste(&b, 1, Some(&vrai), None, "flac", Some("h-vrai"));
    piste(&b, 2, Some(&nul), None, "flac", Some("h-nul"));
    piste(&b, 3, None, Some(&image), "flac", None);
    // Une tranche CUE qui porterait AUSSI un `file_path` (ligne incohérente,
    // ou écrite par un outil tiers) : c'est `cue_media_path` qui l'écarte.
    let tranche = copie(d, "tranche.flac", "test.flac");
    piste(
        &b,
        7,
        Some(&tranche),
        Some(&image),
        "flac",
        Some("h-tranche"),
    );
    piste(&b, 4, Some(&mp3), None, "mp3", Some("h-mp3"));
    piste(&b, 5, Some(&absent), None, "flac", Some("h-absent"));
    // `format` en majuscules et `audio_hash` absent : la piste est quand même
    // lue, et son témoin vaut ''.
    let sans_hash = copie(d, "sans_hash.flac", "flac/ref_16_44100_mono.flac");
    piste(&b, 6, Some(&sans_hash), None, "FLAC", None);

    let bilan = rattraper(&b);
    assert_eq!(
        bilan,
        Bilan {
            candidates: 4,
            read: 3,
            keys_written: 2,
            without_key: 1,
            missing: 1,
            interrupted: false,
            duration_ms: bilan.duration_ms,
        }
    );
    assert_eq!(
        cle(&b, 1),
        (Some(CLE_TEST_FLAC.into()), Some("h-vrai".into()))
    );
    assert_eq!(
        cle(&b, 2),
        (None, Some("h-nul".into())),
        "MD5 nul : lu, sans clé"
    );
    assert_eq!(cle(&b, 3), (None, None), "piste CUE : jamais lue");
    assert_eq!(cle(&b, 7), (None, None), "tranche CUE : jamais lue");
    assert_eq!(cle(&b, 4), (None, None), "MP3 : jamais lu");
    assert_eq!(cle(&b, 5), (None, None), "introuvable : ni lu, ni marqué");
    assert_eq!(
        cle(&b, 6),
        (
            Some("flac-md5-v1:5270a4cef1fd390e14b27a6bc7dc66c8:8820:44100:1:16".into()),
            Some(String::new())
        )
    );

    // Aucune écriture dans les fichiers audio.
    for (p, o) in &avant {
        assert_eq!(&std::fs::read(p).unwrap(), o, "{p} a été modifié");
    }

    // Deuxième passe : seule l'introuvable reste candidate, rien n'est relu.
    let bis = rattraper(&b);
    assert_eq!(
        (bis.candidates, bis.read, bis.missing),
        (1, 0, 1),
        "{bis:?}"
    );

    // Le partage revient : la piste est lue à la passe suivante.
    std::fs::copy(fixture("flac/ref_24_96000_stereo.flac"), &absent).unwrap();
    let ter = rattraper(&b);
    assert_eq!((ter.candidates, ter.keys_written), (1, 1), "{ter:?}");
    assert_eq!(
        cle(&b, 5).0.as_deref(),
        Some("flac-md5-v1:f69407545b775f1a6291b510790a2559:9600:96000:2:24")
    );
}

/// Un fichier REMPLACÉ (le scan a recalculé son `audio_hash`) est relu, et
/// sa clé suit le nouveau signal — y compris vers « pas de clé ».
#[test]
fn un_fichier_change_est_relu() {
    let tmp = tempfile::TempDir::new().unwrap();
    let b = base();
    let p = copie(tmp.path(), "a.flac", "test.flac");
    piste(&b, 1, Some(&p), None, "flac", Some("h1"));
    rattraper(&b);
    assert_eq!(cle(&b, 1).0.as_deref(), Some(CLE_TEST_FLAC));

    // Remplacé par un FLAC au MD5 nul ; le scan a posé un nouvel audio_hash.
    let mut octets = std::fs::read(fixture("test.flac")).unwrap();
    octets[26..42].fill(0);
    std::fs::write(&p, &octets).unwrap();
    // Tant que l'audio_hash n'a pas bougé, rien n'est relu.
    assert_eq!(rattraper(&b).candidates, 0);
    b.execute("UPDATE tracks SET audio_hash = 'h2' WHERE id = 1", &[])
        .unwrap();
    let bilan = rattraper(&b);
    assert_eq!((bilan.read, bilan.without_key), (1, 1), "{bilan:?}");
    assert_eq!(
        cle(&b, 1),
        (None, Some("h2".into())),
        "l'ancienne clé ne survit pas"
    );
}

/// Reprenable : la passe va au-delà d'un lot (pagination par identifiant), et
/// une passe arrêtée en route ne perd rien — chaque piste lue est marquée,
/// la suivante reprend exactement les restantes.
#[test]
fn le_rattrapage_traverse_plusieurs_lots_et_reprend_ou_il_s_est_arrete() {
    let tmp = tempfile::TempDir::new().unwrap();
    let b = base();
    let p = copie(tmp.path(), "a.flac", "test.flac");
    let n = (2 * LOT + 7) as i64;
    for id in 1..=n {
        // Une ligne par piste (file_path est UNIQUE) : des liens vers le même
        // fichier suffisent, seul l'en-tête est lu.
        let lien = tmp.path().join(format!("{id}.flac"));
        std::fs::hard_link(&p, &lien).unwrap();
        piste(
            &b,
            id,
            Some(lien.to_str().unwrap()),
            None,
            "flac",
            Some(&format!("h{id}")),
        );
    }
    // Un arrêt en route, simulé : les 300 premières pistes ont déjà été lues
    // par une passe précédente (témoin posé), sans clé encore écrite.
    b.execute(
        "UPDATE tracks SET audio_pcm_key_seen = audio_hash WHERE id <= 300",
        &[],
    )
    .unwrap();
    let bilan = rattraper(&b);
    assert_eq!(bilan.candidates as i64, n - 300, "{bilan:?}");
    assert_eq!(bilan.keys_written as i64, n - 300);
    let restantes = b
        .query_one(
            "SELECT COUNT(*) FROM tracks WHERE audio_pcm_key IS NULL",
            &[],
        )
        .unwrap()
        .and_then(|r| r.first().and_then(|v| v.as_i64()));
    assert_eq!(
        restantes,
        Some(300),
        "seules les pistes déjà lues restent sans clé"
    );
    assert_eq!(rattraper(&b).candidates, 0, "rien à relire");
}

/// La pause de « ReplayGain » arrête la passe à la frontière d'un lot, sans
/// rien marquer de ce qui n'a pas été lu.
#[test]
fn la_pause_arrete_la_passe() {
    use tune_core::taches_de_fond::{Tache, mettre_en_pause, reprendre};
    let tmp = tempfile::TempDir::new().unwrap();
    let b = base();
    let p = copie(tmp.path(), "a.flac", "test.flac");
    piste(&b, 1, Some(&p), None, "flac", Some("h1"));
    mettre_en_pause(&b, Tache::ReplayGain).unwrap();
    let bilan = rattraper(&b);
    reprendre(&b, Tache::ReplayGain).unwrap();
    assert!(bilan.interrupted);
    assert_eq!(bilan.read, 0);
    assert_eq!(cle(&b, 1), (None, None));
    assert_eq!(rattraper(&b).keys_written, 1);
}

/// L'index de la clé existe : la recherche d'une clé reçue ne balaiera pas la
/// table.
#[test]
fn la_cle_est_indexee() {
    let b = base();
    let n = b
        .query_one(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' \
             AND name = 'idx_tracks_audio_pcm_key' AND tbl_name = 'tracks'",
            &[],
        )
        .unwrap()
        .and_then(|r| r.first().and_then(|v| v.as_i64()));
    assert_eq!(n, Some(1));
}
