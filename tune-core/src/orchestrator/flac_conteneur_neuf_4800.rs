//! #4800 (cause 5) — un FLAC de l'enregistreur vers une zone réseau part TEL
//! QUEL sous un en-tête neuf, au lieu d'être décodé puis ré-encodé en entier
//! avant le premier octet servi.
//!
//! Mesure du 23/09/2026 sur le .18 : 7 lectures sur 17 mettaient 2 à 4,6 s
//! avant le premier son, toutes passées par
//! `flac_ffmpeg_transcode_au_lieu_du_passthrough` ; l'encodage FLAC seul
//! pesait 1,4 à 1,65 s. La règle de #4350 (vendeur `Lavf` ET MD5 nul ⇒ pas de
//! passthrough vers le réseau, l'Eversolo DMP-A8 cale sur ces en-têtes) est
//! GARDÉE : le conteneur est bien réécrit — mais par un en-tête neuf sur les
//! trames copiées (la forme de `remux_flac_dash_stream`, que le DMP-A8 lit
//! pour Tidal), sans décodage.
//!
//! Les fichiers de banc sont « façon enregistreur » : la forme exacte
//! mesurée sur le .18 (STREAMINFO à MD5 nul, PADDING, VORBIS_COMMENT
//! `Lavf60.16.100` avec tags, PICTURE en dernier), sur les trames d'un FLAC de
//! référence du dépôt — donc décodables, pour que les témoins qui transcodent
//! transcodent vraiment.

use std::sync::Arc;

use tokio::sync::Mutex;

use super::{PlaybackOrchestrator, ResolvedQueueItem};
use crate::audio::flac_vendeur::ENTETE_NEUF_OCTETS;
use crate::db::backend::DbBackend;
use crate::db::migrations::run_migrations;
use crate::db::models::Track;
use crate::db::play_queue_repo::{PlayQueueRepo, QueueInput};
use crate::db::sqlite::SqliteDb;
use crate::db::track_repo::TrackRepo;
use crate::db::zone_repo::ZoneRepo;
use crate::http::streamer::AudioStreamer;
use crate::outputs::registry::OutputRegistry;
use crate::playback::PlaybackManager;
use crate::streaming::registry::ServiceRegistry;

const FLAC_16_44: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/flac/ref_16_44100_stereo.flac"
);
const FLAC_24_96: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/flac/ref_24_96000_stereo.flac"
);

/// Un en-tête de bloc FLAC : dernier bloc ?, type, longueur sur 24 bits.
fn bloc(dernier: bool, type_bloc: u8, corps: &[u8]) -> Vec<u8> {
    let l = corps.len() as u32;
    let mut v = vec![
        (if dernier { 0x80 } else { 0 }) | type_bloc,
        (l >> 16) as u8,
        (l >> 8) as u8,
        l as u8,
    ];
    v.extend_from_slice(corps);
    v
}

/// Un bloc PICTURE bien formé (type, MIME, description, dimensions, données),
/// de la taille de la pochette mesurée sur le .18 : Symphonia le lit quand
/// les témoins qui transcodent décodent le fichier.
fn pochette(taille: usize) -> Vec<u8> {
    let mime = b"image/jpeg";
    let mut p = Vec::with_capacity(taille);
    p.extend_from_slice(&3u32.to_be_bytes()); // Cover (front)
    p.extend_from_slice(&(mime.len() as u32).to_be_bytes());
    p.extend_from_slice(mime);
    p.extend_from_slice(&0u32.to_be_bytes()); // description vide
    p.extend_from_slice(&600u32.to_be_bytes());
    p.extend_from_slice(&600u32.to_be_bytes());
    p.extend_from_slice(&24u32.to_be_bytes());
    p.extend_from_slice(&0u32.to_be_bytes());
    let donnees = taille - p.len() - 4;
    p.extend_from_slice(&(donnees as u32).to_be_bytes());
    p.extend(std::iter::repeat_n(0xABu8, donnees));
    assert_eq!(p.len(), taille);
    p
}

/// Le STREAMINFO et les trames d'un FLAC de référence : tout ce qui suit son
/// dernier bloc de métadonnées.
fn streaminfo_et_trames(fixture: &str) -> ([u8; 34], Vec<u8>) {
    let d = std::fs::read(fixture).expect("fixture");
    assert_eq!(&d[..4], b"fLaC");
    let mut si = [0u8; 34];
    si.copy_from_slice(&d[8..42]);
    let mut pos = 4;
    loop {
        let h = &d[pos..pos + 4];
        let dernier = h[0] & 0x80 != 0;
        let l = u32::from_be_bytes([0, h[1], h[2], h[3]]) as usize;
        pos += 4 + l;
        if dernier {
            break;
        }
    }
    (si, d[pos..].to_vec())
}

/// Ce qu'un fichier de banc a d'utile aux témoins.
struct FichierDeBanc {
    chemin: String,
    taille: u64,
    debut_des_trames: u64,
    trames: u64,
}

/// Un fichier « façon enregistreur » sur les trames d'un FLAC de référence.
fn fichier_enregistreur(
    dir: &std::path::Path,
    nom: &str,
    fixture: &str,
    vendeur: &str,
    md5_nul: bool,
) -> FichierDeBanc {
    let (mut si, trames) = streaminfo_et_trames(fixture);
    if md5_nul {
        si[18..34].fill(0);
    }
    let mut f = b"fLaC".to_vec();
    f.extend(bloc(false, 0, &si));
    f.extend(bloc(false, 1, &vec![0u8; 42_509]));
    let mut tags = (vendeur.len() as u32).to_le_bytes().to_vec();
    tags.extend_from_slice(vendeur.as_bytes());
    tags.extend_from_slice(&2u32.to_le_bytes());
    for t in ["TITLE=Stickle Bricks", "ARTIST=Guess What"] {
        tags.extend_from_slice(&(t.len() as u32).to_le_bytes());
        tags.extend_from_slice(t.as_bytes());
    }
    f.extend(bloc(false, 4, &tags));
    f.extend(bloc(true, 6, &pochette(154_508)));
    let debut_des_trames = f.len() as u64;
    f.extend_from_slice(&trames);
    let chemin = dir.join(nom);
    std::fs::write(&chemin, &f).expect("fichier de banc");
    FichierDeBanc {
        chemin: chemin.to_string_lossy().into_owned(),
        taille: f.len() as u64,
        debut_des_trames,
        trames: trames.len() as u64,
    }
}

struct Banc {
    orch: PlaybackOrchestrator,
    db: Arc<dyn DbBackend>,
    zone_id: i64,
    file: PlayQueueRepo,
    position: std::cell::Cell<i64>,
}

fn banc() -> Banc {
    let sqlite = SqliteDb::open_in_memory().expect("base mémoire");
    sqlite.init_schema().expect("schéma");
    run_migrations(&sqlite).expect("migrations");
    let db: Arc<dyn DbBackend> = Arc::new(sqlite);
    let orch = PlaybackOrchestrator::new(
        db.clone(),
        Arc::new(PlaybackManager::new()),
        Arc::new(AudioStreamer::new(0)),
        Arc::new(Mutex::new(ServiceRegistry::new())),
        Arc::new(Mutex::new(OutputRegistry::new())),
        None,
    );
    let zone_id = ZoneRepo::with_backend(db.clone())
        .create("Salon (banc 4800)", Some("dlna"), Some("uuid:dmp-a8"))
        .expect("zone");
    Banc {
        orch,
        zone_id,
        file: PlayQueueRepo::with_backend(db.clone()),
        db,
        position: std::cell::Cell::new(0),
    }
}

impl Banc {
    fn piste(&self, f: &FichierDeBanc, sample_rate: i32, bit_depth: i32) -> i64 {
        let mut t = Track::new("Piste du banc 4800".into());
        t.duration_ms = 1_000;
        t.file_path = Some(f.chemin.clone());
        t.format = Some("flac".into());
        t.sample_rate = Some(sample_rate);
        t.bit_depth = Some(bit_depth);
        t.channels = 2;
        t.file_size = Some(f.taille as i64);
        t.source = "local".into();
        TrackRepo::with_backend(self.db.clone())
            .create(&t)
            .expect("piste")
    }

    async fn resoudre(&self, track_id: i64) -> ResolvedQueueItem {
        self.file
            .append(self.zone_id, &[QueueInput::Local { track_id }])
            .expect("file");
        let position = self.position.get();
        self.position.set(position + 1);
        self.orch
            .resolve_queue_item_url(self.zone_id, position)
            .await
            .expect("résolution")
    }

    /// Le fichier que la session sert, et la carte d'en-tête neuf si elle
    /// en porte une.
    async fn session(
        &self,
        r: &ResolvedQueueItem,
    ) -> (
        Option<String>,
        Option<crate::audio::faststart::FaststartMap>,
    ) {
        let id = r.stream_id.clone().expect("stream_id");
        let sessions = self.orch.streamer.sessions_state();
        let sessions = sessions.lock().await;
        let s = sessions.get(&id).expect("session");
        let chemin = s.file_path.lock().await.clone();
        let carte = s
            .faststart
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        (chemin, carte)
    }
}

/// Le cas du ticket : FLAC de l'enregistreur, zone DLNA, rien d'autre à
/// faire. La piste part TELLE QUELLE — session de fichier sur le fichier
/// d'origine, pas de temporaire — sous un en-tête neuf : `res@size` annonce
/// les octets qui partent réellement (en-tête + trames), et la carte pointe
/// sur la première trame du fichier.
#[tokio::test]
async fn un_flac_de_l_enregistreur_part_tel_quel_sous_un_en_tete_neuf_4800() {
    let dir = tempfile::tempdir().unwrap();
    let f = fichier_enregistreur(
        dir.path(),
        "10 - Stickle Bricks.flac",
        FLAC_16_44,
        "Lavf60.16.100",
        true,
    );
    let b = banc();
    let id = b.piste(&f, 44_100, 16);

    let r = b.resoudre(id).await;
    assert_eq!(r.mime_type, "audio/flac");
    assert_eq!(
        r.file_size,
        Some(ENTETE_NEUF_OCTETS as u64 + f.trames),
        "res@size = en-tête neuf + trames, pas le fichier ({}) : {:?}",
        f.taille,
        r.file_size
    );
    let (chemin, carte) = b.session(&r).await;
    assert_eq!(
        chemin.as_deref(),
        Some(f.chemin.as_str()),
        "servi depuis le fichier d'origine, sans temporaire de transcodage"
    );
    let carte = carte.expect("l'en-tête neuf doit être attaché à la session");
    assert_eq!(&carte.header[..4], b"fLaC");
    assert_eq!(carte.header.len(), ENTETE_NEUF_OCTETS);
    assert_eq!(carte.body_src_start, f.debut_des_trames);
    assert_eq!(carte.body_len, f.trames);
    assert_eq!(carte.total, r.file_size.unwrap());
}

/// Témoin #4350, l'autre moitié de la règle : un FLAC qui n'est PAS de
/// l'enregistreur (vendeur libFLAC, ou `Lavf` avec un MD5 réel) part en
/// passthrough ordinaire — le fichier entier, sans en-tête neuf.
#[tokio::test]
async fn un_flac_ordinaire_part_toujours_entier_sans_en_tete_neuf() {
    let dir = tempfile::tempdir().unwrap();
    let b = banc();
    for (nom, vendeur, md5_nul) in [
        ("libflac.flac", "reference libFLAC 1.4.3", true),
        ("lavf-md5.flac", "Lavf62.12.101", false),
    ] {
        let f = fichier_enregistreur(dir.path(), nom, FLAC_16_44, vendeur, md5_nul);
        let id = b.piste(&f, 44_100, 16);
        let r = b.resoudre(id).await;
        assert_eq!(r.mime_type, "audio/flac", "{nom}");
        assert_eq!(
            r.file_size,
            Some(f.taille),
            "{nom} : le fichier entier, à sa taille sur disque"
        );
        let (chemin, carte) = b.session(&r).await;
        assert_eq!(chemin.as_deref(), Some(f.chemin.as_str()), "{nom}");
        assert!(
            carte.is_none(),
            "{nom} : aucun en-tête neuf sur un FLAC ordinaire"
        );
    }
}

/// Témoin : un FLAC de l'enregistreur qui exige VRAIMENT un transcodage —
/// 96 kHz sur une zone plafonnée à 48 kHz — le subit comme avant : décodé,
/// rééchantillonné, ré-encodé dans un fichier temporaire. L'en-tête neuf
/// n'y a pas sa place.
#[tokio::test]
async fn un_flac_de_l_enregistreur_au_dessus_du_plafond_est_toujours_transcode() {
    let dir = tempfile::tempdir().unwrap();
    let f = fichier_enregistreur(
        dir.path(),
        "plafond.flac",
        FLAC_24_96,
        "Lavf60.16.100",
        true,
    );
    let b = banc();
    ZoneRepo::with_backend(b.db.clone())
        .update_max_sample_rate(b.zone_id, Some(48_000))
        .expect("max_sample_rate");
    let id = b.piste(&f, 96_000, 24);

    let r = b.resoudre(id).await;
    assert_eq!(
        r.sample_rate,
        Some(48_000),
        "le plafond s'applique au flux servi"
    );
    let (chemin, carte) = b.session(&r).await;
    assert_ne!(
        chemin.as_deref(),
        Some(f.chemin.as_str()),
        "transcodé : servi depuis un fichier produit par Tune, pas l'original"
    );
    assert!(carte.is_none(), "aucun en-tête neuf sur un flux ré-encodé");
    assert_ne!(
        r.file_size,
        Some(ENTETE_NEUF_OCTETS as u64 + f.trames),
        "la taille annoncée est celle du fichier ré-encodé"
    );
}
