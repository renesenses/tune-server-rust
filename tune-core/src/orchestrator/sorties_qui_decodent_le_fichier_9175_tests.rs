//! #9175 — Sendspin et Devialet AIR décodaient le fichier d'origine
//! (`Media::depuis` : `file_path` préféré à `url`) et jetaient le flux que le
//! serveur venait de traiter pour eux : égaliseur, correction de pièce,
//! ReplayGain, crossfeed. AirPlay 2 a eu le même défaut (#1216).
//!
//! Le banc passe par la VRAIE porte de lecture (`PlaybackOrchestrator::play`)
//! sur une zone dont la sortie factice porte le type `sendspin` ou
//! `devialet_air`, et note le `PlayMedia` qu'elle reçoit :
//!
//! - traitement actif (une correction de pièce à la cadence du flux) : le
//!   serveur transcode, `file_path` vaut `None`, la sortie lit `url` ;
//! - aucun traitement : la session sert le fichier d'origine, `file_path` le
//!   désigne et la sortie le lit en passthrough.
//!
//! Contre-épreuve : rendre `local_file_path` tel quel dans
//! `envoyer_a_la_sortie` (l'ancienne ligne) fait tomber le premier cas.

use super::*;
use std::sync::Arc;

use crate::db::models::Track;
use crate::db::settings_repo::SettingsRepo;
use crate::db::track_repo::TrackRepo;
use crate::db::zone_repo::ZoneRepo;
use crate::outputs::{OutputCapabilities, OutputStatus, OutputTarget, PlayMedia};

const FLAC: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/test.flac");

/// Ce que la sortie a reçu : `(url, file_path)`.
type Recu = Arc<std::sync::Mutex<Vec<(String, Option<String>)>>>;

/// Les `seek` reçus par la sortie.
type Sauts = Arc<std::sync::Mutex<Vec<u64>>>;

struct SortieQuiDecode {
    type_de_sortie: &'static str,
    appareil: String,
    recu: Recu,
    sauts: Sauts,
}

#[async_trait::async_trait]
impl OutputTarget for SortieQuiDecode {
    fn name(&self) -> &str {
        "Sortie du banc 9175"
    }
    fn device_id(&self) -> &str {
        &self.appareil
    }
    fn output_type(&self) -> &str {
        self.type_de_sortie
    }
    fn capabilities(&self) -> OutputCapabilities {
        OutputCapabilities::v1(true, true, true, true, true, false)
    }
    async fn play_media(&self, media: &PlayMedia<'_>) -> Result<(), String> {
        self.recu
            .lock()
            .unwrap()
            .push((media.url.to_owned(), media.file_path.map(str::to_owned)));
        Ok(())
    }
    async fn pause(&self) -> Result<(), String> {
        Ok(())
    }
    async fn resume(&self) -> Result<(), String> {
        Ok(())
    }
    async fn stop(&self) -> Result<(), String> {
        Ok(())
    }
    async fn seek(&self, position_ms: u64) -> Result<(), String> {
        self.sauts.lock().unwrap().push(position_ms);
        Ok(())
    }
    async fn set_volume(&self, _: f64) -> Result<(), String> {
        Ok(())
    }
    async fn set_mute(&self, _: bool) -> Result<(), String> {
        Ok(())
    }
    async fn get_status(&self) -> Result<OutputStatus, String> {
        Ok(OutputStatus::default())
    }
    async fn is_available(&self) -> bool {
        true
    }
}

struct Banc {
    orch: PlaybackOrchestrator,
    zone_id: i64,
    track_id: i64,
    appareil: String,
    recu: Recu,
    sauts: Sauts,
}

async fn banc(type_de_sortie: &'static str) -> Banc {
    let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    let db: Arc<dyn crate::db::backend::DbBackend> = Arc::new(db);
    let mut orch = PlaybackOrchestrator::new(
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
    // Le droit `RoomCorrection` : sans lui, la RI ne compte pas.
    orch.license = Some(crate::license::licence_premium_pour_test(db.clone()));
    let appareil = format!("{type_de_sortie}:banc-9175");
    let zone_id = ZoneRepo::with_backend(db.clone())
        .create("Zone du banc 9175", Some(type_de_sortie), Some(&appareil))
        .unwrap();
    let recu: Recu = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sauts: Sauts = Arc::new(std::sync::Mutex::new(Vec::new()));
    orch.outputs
        .lock()
        .await
        .register(Box::new(SortieQuiDecode {
            type_de_sortie,
            appareil: appareil.clone(),
            recu: recu.clone(),
            sauts: sauts.clone(),
        }));
    let mut t = Track::new("Piste du banc 9175".into());
    t.duration_ms = 1_000;
    t.file_path = Some(FLAC.into());
    t.format = Some("flac".into());
    t.sample_rate = Some(44_100);
    t.bit_depth = Some(16);
    t.channels = 2;
    t.file_size = std::fs::metadata(FLAC).ok().map(|m| m.len() as i64);
    t.source = "local".into();
    let track_id = TrackRepo::with_backend(db).create(&t).unwrap();
    Banc {
        orch,
        zone_id,
        track_id,
        appareil,
        recu,
        sauts,
    }
}

/// Une réponse impulsionnelle WAV float 32 mono à 44,1 kHz (la cadence du
/// flux) : un Dirac de gain 0,5, que le convolveur accepte.
fn ecrire_ri() -> crate::test_scratch::ScratchFile {
    let f = crate::test_scratch::scratch_file("ri-9175-44k", ".wav");
    let (cadence, canaux, taps) = (44_100u32, 1u16, 32usize);
    let donnees = taps * 4 * usize::from(canaux);
    let mut wav = Vec::with_capacity(44 + donnees);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&((36 + donnees) as u32).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&3u16.to_le_bytes());
    wav.extend_from_slice(&canaux.to_le_bytes());
    wav.extend_from_slice(&cadence.to_le_bytes());
    wav.extend_from_slice(&(cadence * 4 * u32::from(canaux)).to_le_bytes());
    wav.extend_from_slice(&(4 * canaux).to_le_bytes());
    wav.extend_from_slice(&32u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&(donnees as u32).to_le_bytes());
    for i in 0..taps {
        let v: f32 = if i == 0 { 0.5 } else { 0.0 };
        for _ in 0..canaux {
            wav.extend_from_slice(&v.to_le_bytes());
        }
    }
    std::fs::write(f.path(), wav).unwrap();
    f
}

impl Banc {
    fn armer_la_correction_de_piece(&self, ri: &crate::test_scratch::ScratchFile) {
        SettingsRepo::with_backend(self.orch.db.clone())
            .set(&format!("ir_path_{}", self.zone_id), ri.as_str().as_ref())
            .unwrap();
        assert!(
            self.orch.zone_has_active_ir(self.zone_id),
            "prémisse : la RI est réglée, sous licence, et le fichier existe"
        );
    }

    /// Joue la piste et rend `(url, file_path)` tels que la sortie les reçoit.
    async fn jouer(&self) -> (String, Option<String>) {
        let r = self
            .orch
            .play(PlayRequest {
                zone_id: self.zone_id,
                output_device_id: Some(self.appareil.clone()),
                track_id: Some(self.track_id),
                duration_ms: Some(1_000),
                media_format: Some("flac".into()),
                sample_rate: Some(44_100),
                bit_depth: Some(16),
                ..Default::default()
            })
            .await;
        assert!(r.is_ok(), "la lecture doit partir : {:?}", r.err());
        self.recu
            .lock()
            .unwrap()
            .last()
            .cloned()
            .expect("la sortie n'a reçu aucun play_media")
    }
}

async fn traitement_actif_la_sortie_lit_le_flux_traite(type_de_sortie: &'static str) {
    let b = banc(type_de_sortie).await;
    let ri = ecrire_ri();
    b.armer_la_correction_de_piece(&ri);

    let (url, file_path) = b.jouer().await;

    assert_eq!(
        file_path, None,
        "#9175 ({type_de_sortie}) : un traitement de zone est actif, le serveur a \
         transcodé ; remettre le fichier d'origine à la sortie jette la \
         correction de pièce (url = {url})"
    );
    assert!(
        url.contains("/stream/"),
        "la sortie doit lire le flux servi par Tune : {url}"
    );
}

async fn aucun_traitement_la_sortie_lit_le_fichier_d_origine(type_de_sortie: &'static str) {
    let b = banc(type_de_sortie).await;

    let (_, file_path) = b.jouer().await;

    assert_eq!(
        file_path.as_deref(),
        Some(FLAC),
        "#9175 ({type_de_sortie}) : sans traitement, la session sert le fichier \
         d'origine ; la sortie le lit en passthrough"
    );
}

#[tokio::test]
async fn sendspin_traitement_actif_lit_le_flux_traite_9175() {
    traitement_actif_la_sortie_lit_le_flux_traite(crate::outputs::sendspin::TYPE_DE_SORTIE).await;
}

#[tokio::test]
async fn devialet_air_traitement_actif_lit_le_flux_traite_9175() {
    traitement_actif_la_sortie_lit_le_flux_traite(crate::outputs::devialet_air::TYPE_DE_SORTIE)
        .await;
}

#[tokio::test]
async fn sendspin_sans_traitement_lit_le_fichier_d_origine_9175() {
    aucun_traitement_la_sortie_lit_le_fichier_d_origine(crate::outputs::sendspin::TYPE_DE_SORTIE)
        .await;
}

#[tokio::test]
async fn devialet_air_sans_traitement_lit_le_fichier_d_origine_9175() {
    aucun_traitement_la_sortie_lit_le_fichier_d_origine(
        crate::outputs::devialet_air::TYPE_DE_SORTIE,
    )
    .await;
}

/// Les autres sorties gardent le fichier tel quel (OAAT le lit derrière ses
/// propres gardes, #9176), et seules ces deux-là sont concernées.
#[test]
fn seules_sendspin_et_devialet_air_voient_leur_fichier_filtre_9175() {
    use super::transport::sortie_qui_decode_file_path as filtre;
    assert!(filtre(Some("sendspin")));
    assert!(filtre(Some("devialet_air")));
    for autre in ["oaat", "local", "dlna", "airplay2", "hqplayer", "browser"] {
        assert!(!filtre(Some(autre)), "{autre}");
    }
    assert!(!filtre(None));
}

impl Banc {
    fn armer_l_egaliseur(&self) {
        let s = SettingsRepo::with_backend(self.orch.db.clone());
        s.set("plugin_equalizer_installed", "true").unwrap();
        let profil = crate::audio::eq::EqProfile {
            enabled: true,
            bands: vec![crate::audio::eq::EqBandSpec {
                freq: 1000.0,
                gain: 12.0,
                q: 1.0,
                band_type: "peak".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        s.set(
            &format!("zone_{}_eq_profile", self.zone_id),
            &serde_json::to_string(&profil).unwrap(),
        )
        .unwrap();
        assert!(
            self.orch.zone_has_active_eq(self.zone_id),
            "prémisse : l'égaliseur est actif sur la zone"
        );
    }

    async fn session_cherchable(&self) -> bool {
        let sid = self
            .orch
            .playback
            .get_state(self.zone_id)
            .await
            .now_playing
            .and_then(|np| np.stream_id)
            .expect("session installée");
        self.orch.streamer.is_seekable_session(&sid).await
    }
}

/// L'égaliseur (+12 dB) force le flux traité, comme la correction de pièce.
#[tokio::test]
async fn egaliseur_actif_la_sortie_lit_le_flux_traite_9175() {
    for type_de_sortie in [
        crate::outputs::sendspin::TYPE_DE_SORTIE,
        crate::outputs::devialet_air::TYPE_DE_SORTIE,
    ] {
        let b = banc(type_de_sortie).await;
        b.armer_l_egaliseur();
        let (url, file_path) = b.jouer().await;
        assert_eq!(
            file_path, None,
            "#9175 ({type_de_sortie}) : égaliseur actif, la sortie doit lire le flux \
             traité ({url})"
        );
    }
}

/// Un saut sur un flux TRAITÉ servi par un canal : la sortie ne peut pas le
/// rouvrir (la seconde connexion n'en reçoit que la suite). La zone recrée le
/// flux à la position (une seconde lecture), sans `seek` à la sortie.
#[tokio::test]
async fn saut_sur_un_flux_traite_recree_le_flux_a_la_position_9175() {
    for type_de_sortie in [
        crate::outputs::sendspin::TYPE_DE_SORTIE,
        crate::outputs::devialet_air::TYPE_DE_SORTIE,
    ] {
        let b = banc(type_de_sortie).await;
        b.armer_l_egaliseur();
        b.jouer().await;
        assert!(
            !b.session_cherchable().await,
            "prémisse ({type_de_sortie}) : le flux traité est un canal"
        );

        let r = b.orch.seek(b.zone_id, 500, Some(&b.appareil)).await;

        assert!(r.is_ok(), "{type_de_sortie} : {r:?}");
        assert_eq!(
            b.recu.lock().unwrap().len(),
            2,
            "#9175 ({type_de_sortie}) : le saut sur un canal doit recréer le flux à la \
             position (une seconde lecture)"
        );
        assert!(
            b.sauts.lock().unwrap().is_empty(),
            "#9175 ({type_de_sortie}) : aucun `seek` à la sortie, qui rouvrirait le \
             canal : {:?}",
            b.sauts.lock().unwrap()
        );
        assert_eq!(b.recu.lock().unwrap()[1].1, None, "toujours le flux traité");
    }
}

/// Contre-épreuve : sans traitement, la sortie lit le fichier d'origine et
/// s'y déplace elle-même, sans recréer de flux.
#[tokio::test]
async fn saut_sans_traitement_la_sortie_se_deplace_dans_le_fichier_9175() {
    for type_de_sortie in [
        crate::outputs::sendspin::TYPE_DE_SORTIE,
        crate::outputs::devialet_air::TYPE_DE_SORTIE,
    ] {
        let b = banc(type_de_sortie).await;
        b.jouer().await;
        assert!(b.session_cherchable().await, "prémisse : passthrough");

        let r = b.orch.seek(b.zone_id, 500, Some(&b.appareil)).await;

        assert!(r.is_ok(), "{type_de_sortie} : {r:?}");
        assert_eq!(b.recu.lock().unwrap().len(), 1, "aucun flux recréé");
        assert_eq!(*b.sauts.lock().unwrap(), vec![500]);
    }
}

/// La session sert le chemin RÉSOLU (repli NFC/NFD) ; la bibliothèque peut
/// garder l'autre graphie. C'est le même fichier.
#[test]
fn meme_fichier_sous_deux_graphies_unicode_9175() {
    let dossier = crate::test_scratch::scratch_dir("meme-fichier-9175");
    // « é » composé (NFC) sur le disque, décomposé (NFD) en base.
    let nfc = dossier.path().join("Caf\u{e9}.flac");
    std::fs::write(&nfc, b"x").unwrap();
    let nfd = dossier.path().join("Cafe\u{301}.flac");
    let (nfc, nfd) = (
        nfc.to_string_lossy().into_owned(),
        nfd.to_string_lossy().into_owned(),
    );
    assert!(super::transport::meme_fichier(&nfc, &nfd), "{nfc} / {nfd}");
    assert!(super::transport::meme_fichier(&nfc, &nfc));
    let autre = dossier.path().join("Autre.flac");
    std::fs::write(&autre, b"y").unwrap();
    assert!(!super::transport::meme_fichier(
        &nfc,
        &autre.to_string_lossy()
    ));
}
