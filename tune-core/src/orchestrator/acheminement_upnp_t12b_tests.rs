//! TESS T12b — témoins de la famille UPnP, de bout en bout, sur un FAUX
//! lecteur DLNA (une vraie `DlnaOutput` contre un serveur SOAP local qui rend
//! le Sink donné à `GetProtocolInfo`). Aucun son : personne ne lit le flux,
//! on tire les octets que la session SERT, comme le ferait le lecteur.
//!
//! 1. **Miroir** : la voie publiée par `tess::reseau_upnp::acheminer` est,
//!    case par case, celle que les prédicats de la lecture choisissent
//!    (`cible_wav_pour_traitement`, `wav_progressif_consenti`,
//!    `use_file_transcode_for`, `service_en_wav_progressif`,
//!    `streaming_needs_pretranscode`).
//! 2. **Bit-perfect** : en Pure (égaliseur, crossfeed et opt-in armés), puis
//!    chaîne vide, le lecteur reçoit les octets du fichier source, sha256
//!    identique. Contre-épreuve : Pure coupé, le flux part en WAV progressif
//!    et ses échantillons diffèrent.
//! 3. **Réserve de profondeur** : un lecteur qui n'annonce que le L16 ne reçoit
//!    pas de WAV 24 bits ; la piste 24 bits repart par transcodage (FLAC), et
//!    l'état TESS le dit. La piste 16 bits, elle, part en relais progressif.
//! 4. **Étage casque sans LPCM** : crossfeed seul vers un lecteur qui n'annonce
//!    que le FLAC, la piste part TELLE QUELLE et l'état TESS éteint le
//!    crossfeed avec `renderer_without_lpcm` (badge « Bit-perfect »).
//!    Contre-épreuve : le même lecteur avec L24, crossfeed appliqué.
//! 5. **Gapless** : le relais progressif rend exactement les trames de la
//!    source, ni amorce ni bourrage.
//!
//! Chaque témoin confronte aussi la voie PUBLIÉE (`delivery`) à la voie
//! SERVIE : l'écran ne peut pas dire autre chose que ce qui part.

use super::acheminement_upnp_t12b::PisteEnCours;
use super::crossfeed_bibliotheque_reseau::{
    crossfeed_cuit_dans_le_fichier, wav_progressif_consenti,
};
use super::regles::{
    cible_wav_pour_traitement, service_en_wav_progressif, streaming_needs_pretranscode,
    use_file_transcode_for,
};
use super::{PlayRequest, PlaybackOrchestrator};
use crate::db::backend::DbBackend;
use crate::db::settings_repo::SettingsRepo;
use crate::db::zone_repo::ZoneRepo;
use crate::http::streamer::voie;
use crate::outputs::dlna::DlnaOutput;
use crate::tess::reseau_upnp::{
    FaitsUpnp, LpcmDuLecteur, MotifDeVoie, OrigineDuFlux, VoieUpnp, acheminer,
};
use crate::tess::{
    Badge, ContexteExecution, ControleVolume, MotifEtape, ReponseTessZone, SourceFlux,
    TransportZone, VolumeZone, catalogue_de_reference,
};
use axum::Router;
use axum::routing::post;
use sha2::{Digest, Sha256};
use std::sync::Arc;
use std::time::Duration;

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");
const LECTEUR: &str = "uuid:faux-lecteur-upnp-t12b";

/// Sinks publiés par le faux lecteur.
const SINK_COMPLET: &str = "http-get:*:audio/L16;rate=44100;channels=2:*,\
     http-get:*:audio/L24;rate=96000;channels=2:*,http-get:*:audio/flac:*";
const SINK_L16_SEUL: &str = "http-get:*:audio/L16;rate=44100;channels=2:*,\
     http-get:*:audio/flac:*";
const SINK_FLAC_SEUL: &str = "http-get:*:audio/flac:*";

fn sha(o: &[u8]) -> [u8; 32] {
    Sha256::digest(o).into()
}

// ─── Le faux lecteur ────────────────────────────────────────────────────────

fn enveloppe(corps: &str) -> String {
    format!(
        r#"<?xml version="1.0"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body>{corps}</s:Body></s:Envelope>"#
    )
}

async fn faux_lecteur(sink: &'static str) -> DlnaOutput {
    let app = Router::new()
        .route(
            "/cm",
            post(move |corps: String| async move {
                if corps.contains("GetProtocolInfo") {
                    enveloppe(&format!(
                        "<u:GetProtocolInfoResponse><Source></Source><Sink>{sink}</Sink>\
                         </u:GetProtocolInfoResponse>"
                    ))
                } else {
                    enveloppe("<u:Response/>")
                }
            }),
        )
        .route("/av", post(|| async { enveloppe("<u:R/>") }))
        .route("/rc", post(|| async { enveloppe("<u:R/>") }));
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = ecoute.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(ecoute, app).await.ok();
    });
    DlnaOutput::new(
        "Faux lecteur T12b".into(),
        LECTEUR.into(),
        "127.0.0.1".into(),
        format!("http://127.0.0.1:{port}/av"),
        format!("http://127.0.0.1:{port}/rc"),
        Some(format!("http://127.0.0.1:{port}/cm")),
    )
}

// ─── Le banc ────────────────────────────────────────────────────────────────

struct Banc {
    orch: PlaybackOrchestrator,
    zone_id: i64,
    /// La session de la dernière lecture (ce que `now_playing.stream_id`
    /// porte en production).
    derniere_session: std::sync::Mutex<Option<String>>,
}

fn orchestrateur() -> PlaybackOrchestrator {
    let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    let db: Arc<dyn DbBackend> = Arc::new(db);
    PlaybackOrchestrator::new(
        db,
        Arc::new(crate::playback::PlaybackManager::new()),
        Arc::new(crate::http::streamer::AudioStreamer::new(0)),
        Arc::new(tokio::sync::Mutex::new(
            crate::streaming::registry::ServiceRegistry::new(),
        )),
        Arc::new(tokio::sync::Mutex::new(
            crate::outputs::registry::OutputRegistry::new(),
        )),
        None,
    )
}

fn piste(orch: &PlaybackOrchestrator, nom: &str, cadence: i32, profondeur: i32) -> i64 {
    let chemin = format!("{FIXTURES}/{nom}");
    let mut t = crate::db::models::Track::new(format!("T12b {nom}"));
    t.duration_ms = 1_000;
    t.file_path = Some(chemin.clone());
    t.format = Some("flac".into());
    t.sample_rate = Some(cadence);
    t.bit_depth = Some(profondeur);
    t.channels = 2;
    t.file_size = std::fs::metadata(&chemin).ok().map(|m| m.len() as i64);
    t.source = "local".into();
    crate::db::track_repo::TrackRepo::with_backend(orch.db.clone())
        .create(&t)
        .unwrap()
}

const PISTE_24: (&str, i32, i32) = ("flac/ref_24_96000_stereo.flac", 96_000, 24);
const PISTE_16: (&str, i32, i32) = ("flac/ref_16_44100_stereo.flac", 44_100, 16);

async fn banc(sink: &'static str) -> Banc {
    let orch = orchestrateur();
    orch.outputs
        .lock()
        .await
        .register(Box::new(faux_lecteur(sink).await));
    let zone_id = ZoneRepo::with_backend(orch.db.clone())
        .create("Faux lecteur", Some("dlna"), Some(LECTEUR))
        .unwrap();
    Banc {
        orch,
        zone_id,
        derniere_session: Default::default(),
    }
}

impl Banc {
    fn reglage(&self, cle: &str, valeur: &str) {
        SettingsRepo::with_backend(self.orch.db.clone())
            .set(cle, valeur)
            .unwrap();
    }

    fn egaliseur(&self) {
        self.reglage("plugin_equalizer_installed", "true");
        let p = crate::audio::eq::EqProfile {
            enabled: true,
            bands: vec![crate::audio::eq::EqBandSpec {
                freq: 1000.0,
                gain: -4.0,
                q: 0.71,
                band_type: "peak".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        self.reglage(
            &format!("zone_{}_eq_profile", self.zone_id),
            &serde_json::to_string(&p).unwrap(),
        );
        assert!(self.orch.zone_has_active_eq(self.zone_id));
    }

    fn crossfeed(&self) {
        self.reglage(
            &format!("zone_{}_crossfeed", self.zone_id),
            r#"{"enabled":true,"amount":0.3,"delay_ms":0.3}"#,
        );
        assert!(self.orch.zone_has_active_crossfeed(self.zone_id));
    }

    fn pure(&self, oui: bool) {
        self.reglage(
            &format!("zone_{}_audiophile", self.zone_id),
            &format!(r#"{{"enabled":{oui}}}"#),
        );
    }

    fn opt_in(&self) {
        self.reglage("dsp_progressif_reseau", "true");
    }

    async fn faits(&self, profondeur: u32) -> FaitsUpnp {
        self.orch
            .faits_upnp_de_zone(
                self.zone_id,
                &PisteEnCours {
                    track_id: None,
                    source: Some("local".into()),
                    source_id: None,
                    format: Some("flac".into()),
                    bit_depth: Some(profondeur),
                    stream_id: self.derniere_session.lock().unwrap().clone(),
                },
            )
            .await
            .expect("zone de la famille UPnP")
    }

    /// L'état TESS publié (`GET /zones/{id}/tess`) pour une piste PCM.
    async fn etat_tess(&self, cadence: u32, profondeur: u32) -> ReponseTessZone {
        let settings = SettingsRepo::with_backend(self.orch.db.clone());
        let lecture = crate::tess::migration::lire_zone(&settings, self.zone_id, true).unwrap();
        let faits = self.faits(profondeur).await;
        let tout = |_: &str| true;
        let ctx = ContexteExecution {
            transport: TransportZone::Dlna,
            source: Some(SourceFlux::Pcm {
                sample_rate_hz: cadence,
                bits: profondeur as u8,
                channels: 2,
            }),
            premium: true,
            greffon_disponible: &tout,
            pure: lecture.pure,
            volume: VolumeZone {
                control: ControleVolume::Device,
                db: 0.0,
            },
        };
        ReponseTessZone::depuis_avec_le_lecteur(
            self.zone_id,
            lecture.chaine,
            None,
            catalogue_de_reference(),
            &ctx,
            Some(&faits),
        )
    }
}

/// Ce que le lecteur reçoit.
struct Servi {
    mime: String,
    voie: &'static str,
    progressif: bool,
    /// Les octets, tels que le serveur de flux les sert.
    octets: Vec<u8>,
}

impl Servi {
    /// Les échantillons décodés et leur nombre de trames.
    fn pcm(&self) -> (Vec<i32>, u64) {
        let dir = tempfile::tempdir().unwrap();
        let ext = if self.mime.contains("wav") {
            "wav"
        } else {
            "flac"
        };
        let f = dir.path().join(format!("servi.{ext}"));
        std::fs::write(&f, &self.octets).unwrap();
        let d = crate::audio::decode::decode_to_pcm(&f.to_string_lossy(), None, None, 0.0, 0.0)
            .unwrap();
        let n = d.samples_i32.len() as u64 / u64::from(d.channels);
        (d.samples_i32, n)
    }
}

async fn jouer(b: &Banc, track_id: i64) -> Servi {
    let req = PlayRequest {
        zone_id: b.zone_id,
        output_device_id: Some(LECTEUR.into()),
        track_id: Some(track_id),
        source: Some("local".into()),
        ..Default::default()
    };
    let r = b.orch.resolve_local_track(&req).await.unwrap();
    let sid = r.stream_id.clone().expect("une session");
    *b.derniere_session.lock().unwrap() = Some(sid.clone());
    let voie = b.orch.streamer.voie_servie(&sid).await;
    let session = b
        .orch
        .streamer
        .sessions_state()
        .lock()
        .await
        .get(&sid)
        .cloned()
        .expect("session inscrite");
    if let Some(chemin) = session.file_path.lock().await.clone() {
        let fichier = std::fs::read(&chemin).unwrap();
        // Carte d'en-tête (#4800) : en-tête neuf, puis le corps du fichier.
        let carte = session.faststart.lock().unwrap().clone();
        let octets = match carte {
            Some(c) => {
                let mut o = c.header.clone();
                o.extend_from_slice(&fichier[usize::try_from(c.body_src_start).unwrap()..]);
                o
            }
            None => fichier,
        };
        return Servi {
            mime: r.mime_type,
            voie,
            progressif: false,
            octets,
        };
    }
    let mut octets = Vec::new();
    while let Some(c) = tokio::time::timeout(Duration::from_secs(60), session.recv_chunk())
        .await
        .expect("le flux progressif s'est figé")
    {
        octets.extend_from_slice(&c);
    }
    Servi {
        mime: r.mime_type,
        voie,
        progressif: true,
        octets,
    }
}

fn source(nom: &str) -> (Vec<u8>, Vec<i32>, u64) {
    let chemin = format!("{FIXTURES}/{nom}");
    let d = crate::audio::decode::decode_to_pcm(&chemin, None, None, 0.0, 0.0).unwrap();
    let n = d.samples_i32.len() as u64 / u64::from(d.channels);
    (std::fs::read(&chemin).unwrap(), d.samples_i32, n)
}

fn greffon<'a>(r: &'a ReponseTessZone, id: &str) -> &'a crate::tess::EtapeResolue {
    r.signal_path
        .iter()
        .find(|e| e.plugin_id.as_deref() == Some(id))
        .unwrap_or_else(|| panic!("{id} absent de la chaîne"))
}

// ─── 1. Miroir de la lecture ────────────────────────────────────────────────

/// La voie que la LECTURE choisit, recomposée avec ses propres prédicats.
/// `reencodage` est `TraitementsDeZone::un_au_moins` (égaliseur, correction de
/// pièce, ReplayGain : `eq_forces_transcode`), `casque` est
/// `zone_has_active_crossfeed` (le `crossfeed_reseau` de `dsp_progressif_wav`),
/// `conversion` dit que le format impose de convertir la piste, `dsd` que la
/// source est un DSD converti en PCM (`src_est_dsd` de la lecture).
#[allow(clippy::too_many_arguments)]
fn voie_de_la_lecture(
    reencodage: bool,
    casque: bool,
    conversion: bool,
    dsd: bool,
    lpcm: bool,
    opt_in: bool,
    service: bool,
    decodable: bool,
) -> VoieUpnp {
    let consenti = wav_progressif_consenti(opt_in, casque, reencodage);
    if service {
        let actif = reencodage || casque;
        if service_en_wav_progressif(actif, true, consenti, decodable, lpcm) {
            VoieUpnp::ProgressiveLpcm
        } else if streaming_needs_pretranscode(!conversion, actif, false, false) {
            VoieUpnp::FileTranscode
        } else {
            VoieUpnp::Untouched
        }
    } else {
        let traitement = reencodage || casque;
        if cible_wav_pour_traitement(traitement, true, dsd, consenti, lpcm) {
            VoieUpnp::ProgressiveLpcm
        } else if (reencodage || conversion || dsd)
            // La cible FLAC part par le fichier ; l'étage casque y est cuit
            // (`crossfeed_cuit_dans_le_fichier`).
            && use_file_transcode_for(true, false, false, false, false, reencodage)
            && crossfeed_cuit_dans_le_fichier(true, false)
        {
            VoieUpnp::FileTranscode
        } else {
            VoieUpnp::Untouched
        }
    }
}

/// Le miroir, réglages compris : chaque combinaison d'égaliseur et de
/// crossfeed est ÉCRITE dans les réglages de la zone ; la lecture la classe
/// par ses propres prédicats (`zone_has_active_eq`, `zone_ir_s_appliquerait`,
/// `zone_replaygain_changes_audio` : les champs de `TraitementsDeZone` ;
/// `zone_has_active_crossfeed`), TESS par la chaîne que T02 en tire
/// (`lire_zone`) puis `acheminer`. Les deux voies doivent coïncider, case
/// par case, conversion imposée par le format comprise.
#[tokio::test]
async fn la_voie_publiee_est_celle_de_la_lecture_case_par_case() {
    let mut cases = 0;
    for (eq, cf) in [(false, false), (true, false), (false, true), (true, true)] {
        let b = banc(SINK_COMPLET).await;
        if eq {
            b.egaliseur();
        }
        if cf {
            b.crossfeed();
        }
        let z = b.zone_id;
        let piste = crate::audio::replaygain::PisteRg::AUCUNE;
        let reencodage = b.orch.zone_has_active_eq(z)
            || b.orch.zone_ir_s_appliquerait(z, None, 2)
            || b.orch.zone_replaygain_changes_audio(z, piste);
        let casque = b.orch.zone_has_active_crossfeed(z);
        assert_eq!((reencodage, casque), (eq, cf), "banc");
        let settings = SettingsRepo::with_backend(b.orch.db.clone());
        let lecture = crate::tess::migration::lire_zone(&settings, z, true).unwrap();
        let tout = |_: &str| true;
        let pcm = SourceFlux::Pcm {
            sample_rate_hz: 44_100,
            bits: 16,
            channels: 2,
        };
        let dsd = SourceFlux::Dsd {
            rate: crate::tess::TauxDsd::Dsd64,
            channels: 2,
            played_as: crate::tess::LectureDsd::ConvertedToPcm,
        };
        for (source, est_dsd) in [(pcm, false), (dsd, true)] {
            let ctx = ContexteExecution {
                transport: TransportZone::Dlna,
                source: Some(source),
                premium: true,
                greffon_disponible: &tout,
                pure: lecture.pure,
                volume: VolumeZone {
                    control: ControleVolume::Device,
                    db: 0.0,
                },
            };
            let r = lecture.chaine.resoudre(&catalogue_de_reference(), &ctx);
            for rg in [false, true] {
                for conversion in [false, true] {
                    for lpcm in [
                        LpcmDuLecteur::Accepted,
                        LpcmDuLecteur::Refused,
                        LpcmDuLecteur::Unknown,
                    ] {
                        for opt_in in [false, true] {
                            for (origine, decodable) in [
                                (OrigineDuFlux::Library, true),
                                (OrigineDuFlux::Unknown, true),
                                (OrigineDuFlux::Service, true),
                                (OrigineDuFlux::Service, false),
                            ] {
                                // Un DSD converti vient de la bibliothèque.
                                if est_dsd && origine == OrigineDuFlux::Service {
                                    continue;
                                }
                                let f = FaitsUpnp {
                                    lpcm,
                                    lpcm_bits: 16,
                                    progressif_arme: opt_in,
                                    origine,
                                    replaygain_actif: rg,
                                    service_decodable_au_fil: decodable,
                                    format_impose_le_transcodage: conversion,
                                };
                                let attendu = voie_de_la_lecture(
                                    reencodage || rg,
                                    casque,
                                    conversion,
                                    est_dsd,
                                    lpcm == LpcmDuLecteur::Accepted,
                                    opt_in,
                                    origine == OrigineDuFlux::Service,
                                    decodable,
                                );
                                assert_eq!(
                                    acheminer(&r, &f).route,
                                    attendu,
                                    "eq={eq} cf={cf} dsd={est_dsd} rg={rg} \
                                     conversion={conversion} {lpcm:?} opt_in={opt_in} \
                                     {origine:?} decodable={decodable}"
                                );
                                cases += 1;
                            }
                        }
                    }
                }
            }
        }
    }
    // PCM : 2 (RG) × 2 × 3 × 2 × 4 ; DSD converti : 2 × 2 × 3 × 2 × 2.
    assert_eq!(cases, 4 * (96 + 48));
}

// ─── 2. Bit-perfect à l'octet ───────────────────────────────────────────────

/// En Pure, tout armé (égaliseur, crossfeed, opt-in) vers un lecteur qui
/// annonce L16, L24 et FLAC : le fichier part tel quel. Contre-épreuve : Pure
/// coupé, WAV progressif, échantillons différents, et l'état le dit.
#[tokio::test]
async fn en_pure_le_faux_lecteur_recoit_la_source_a_l_octet() {
    let b = banc(SINK_COMPLET).await;
    let id = piste(&b.orch, PISTE_24.0, PISTE_24.1, PISTE_24.2);
    b.egaliseur();
    b.crossfeed();
    b.opt_in();
    b.pure(true);
    let (fichier, pcm_source, trames) = source(PISTE_24.0);

    let s = jouer(&b, id).await;
    assert_eq!(
        s.voie,
        voie::FICHIER_TEL_QUEL,
        "Pure : le fichier, octet pour octet"
    );
    assert!(!s.progressif);
    assert_eq!(
        sha(&s.octets),
        sha(&fichier),
        "sha256 servi = sha256 source"
    );
    let (pcm, n) = s.pcm();
    assert_eq!(n, trames);
    assert!(pcm == pcm_source);
    let etat = b.etat_tess(96_000, 24).await;
    assert_eq!(etat.badge, Badge::BitPerfect);
    let d = etat.delivery.expect("famille UPnP");
    assert_eq!(d.route, VoieUpnp::Untouched);
    assert_eq!(d.reason, Some(MotifDeVoie::NothingToProcess));

    // Contre-épreuve : Pure coupé.
    b.pure(false);
    let s = jouer(&b, id).await;
    assert!(s.progressif, "relais progressif LPCM");
    assert_eq!(s.mime, "audio/wav");
    let (pcm, n) = s.pcm();
    assert_eq!(n, trames, "ni amorce ni bourrage");
    assert!(pcm != pcm_source, "l'égaliseur et le crossfeed ont agi");
    let etat = b.etat_tess(96_000, 24).await;
    assert_eq!(etat.badge, Badge::ProcessedByTess);
    assert_eq!(etat.delivery.unwrap().route, VoieUpnp::ProgressiveLpcm);
}

/// Chaîne vide, Pure coupé : rien n'est traité, la source part à l'octet.
#[tokio::test]
async fn chaine_vide_la_source_part_a_l_octet() {
    let b = banc(SINK_COMPLET).await;
    let id = piste(&b.orch, PISTE_16.0, PISTE_16.1, PISTE_16.2);
    b.opt_in();
    let (fichier, _, _) = source(PISTE_16.0);
    let s = jouer(&b, id).await;
    assert_eq!(s.voie, voie::FICHIER_TEL_QUEL);
    assert_eq!(sha(&s.octets), sha(&fichier));
    let etat = b.etat_tess(44_100, 16).await;
    assert_eq!(etat.badge, Badge::BitPerfect);
    assert_eq!(etat.delivery.unwrap().route, VoieUpnp::Untouched);
}

// ─── 3. Réserve de profondeur ───────────────────────────────────────────────

/// Un lecteur qui n'annonce que le L16 : la piste 24 bits avec égaliseur ne
/// part PAS en WAV 24 bits ; elle repart par transcodage (FLAC 24 bits, que
/// le lecteur lit), et l'état le dit. Contre-épreuve : la piste 16 bits part
/// en relais progressif.
#[tokio::test]
async fn un_lecteur_l16_seul_recoit_un_repli_propre_en_24_bits() {
    let b = banc(SINK_L16_SEUL).await;
    let id24 = piste(&b.orch, PISTE_24.0, PISTE_24.1, PISTE_24.2);
    let id16 = piste(&b.orch, PISTE_16.0, PISTE_16.1, PISTE_16.2);
    b.egaliseur();
    b.opt_in();

    // Rien n'a encore joué : aucune réponse mémorisée, et l'état ne sonde pas.
    assert_eq!(b.faits(24).await.lpcm, LpcmDuLecteur::Unknown);
    let s = jouer(&b, id24).await;
    let f24 = b.faits(24).await;
    assert_eq!(
        f24.lpcm,
        LpcmDuLecteur::Refused,
        "sonde de la lecture, pas de L24"
    );
    assert_eq!(f24.lpcm_bits, 24);
    assert!(!s.progressif, "aucun WAV 24 bits vers un lecteur L16");
    assert_eq!(s.mime, "audio/flac");
    assert_eq!(s.voie, voie::FICHIER_CONVERTI);
    let (_, n) = s.pcm();
    assert_eq!(n, source(PISTE_24.0).2, "transcodage : trames exactes");
    let d = b.etat_tess(96_000, 24).await.delivery.unwrap();
    assert_eq!(d.route, VoieUpnp::FileTranscode);
    assert_eq!(d.reason, Some(MotifDeVoie::RendererWithoutLpcm));
    assert!(d.detail.unwrap().contains("24 bits"));

    // Contre-épreuve : 16 bits, le L16 annoncé ouvre le relais.
    let s = jouer(&b, id16).await;
    assert_eq!(b.faits(16).await.lpcm, LpcmDuLecteur::Accepted);
    assert!(s.progressif);
    assert_eq!(s.mime, "audio/wav");
    let d = b.etat_tess(44_100, 16).await.delivery.unwrap();
    assert_eq!(d.route, VoieUpnp::ProgressiveLpcm);
    assert_eq!(d.reason, None);
}

// ─── 4. Étage casque sans LPCM ──────────────────────────────────────────────

/// Crossfeed seul vers un lecteur qui n'annonce que le FLAC : la piste part
/// telle quelle, et l'état TESS ne prétend pas le contraire.
#[tokio::test]
async fn crossfeed_seul_sans_lpcm_part_tel_quel_et_l_etat_le_dit() {
    let b = banc(SINK_FLAC_SEUL).await;
    let id = piste(&b.orch, PISTE_24.0, PISTE_24.1, PISTE_24.2);
    b.crossfeed();
    let (fichier, _, _) = source(PISTE_24.0);
    let s = jouer(&b, id).await;
    assert_eq!(s.voie, voie::FICHIER_TEL_QUEL);
    assert_eq!(sha(&s.octets), sha(&fichier), "rien n'a touché les octets");
    let etat = b.etat_tess(96_000, 24).await;
    let cf = greffon(&etat, "crossfeed");
    assert!(cf.requested && !cf.effective);
    assert_eq!(cf.reason, Some(MotifEtape::RendererWithoutLpcm));
    assert_eq!(etat.badge, Badge::BitPerfect);
    let d = etat.delivery.unwrap();
    assert_eq!(d.route, VoieUpnp::Untouched);
    assert_eq!(d.reason, Some(MotifDeVoie::RendererWithoutLpcm));
}

/// Contre-épreuve du précédent : le même réglage vers un lecteur qui annonce
/// le L24 part en relais progressif, crossfeed appliqué, et l'état le dit.
#[tokio::test]
async fn crossfeed_seul_avec_lpcm_part_en_relais_progressif() {
    let b = banc(SINK_COMPLET).await;
    let id = piste(&b.orch, PISTE_24.0, PISTE_24.1, PISTE_24.2);
    b.crossfeed();
    let (_, pcm_source, trames) = source(PISTE_24.0);
    let s = jouer(&b, id).await;
    assert!(s.progressif);
    let (pcm, n) = s.pcm();
    assert_eq!(n, trames);
    assert!(pcm != pcm_source, "crossfeed appliqué");
    let etat = b.etat_tess(96_000, 24).await;
    assert!(greffon(&etat, "crossfeed").effective);
    assert_eq!(etat.badge, Badge::ProcessedByTess);
    assert_eq!(etat.delivery.unwrap().route, VoieUpnp::ProgressiveLpcm);
}

// ─── 5. Gapless ─────────────────────────────────────────────────────────────

/// Deux pistes de suite en relais progressif (égaliseur) : chacune rend
/// exactement ses trames, sans amorce de latence ni queue ajoutée ; la
/// seconde n'hérite rien de la première (même rendu qu'une première lecture).
#[tokio::test]
async fn le_relais_progressif_rend_les_trames_exactes_de_piste_en_piste() {
    let b = banc(SINK_COMPLET).await;
    let a = piste(&b.orch, PISTE_16.0, PISTE_16.1, PISTE_16.2);
    let c = piste(&b.orch, PISTE_24.0, PISTE_24.1, PISTE_24.2);
    b.egaliseur();
    b.opt_in();
    let premiere_a = jouer(&b, a).await;
    let premiere_c = jouer(&b, c).await;
    let encore_a = jouer(&b, a).await;
    for (s, nom) in [(&premiere_a, PISTE_16.0), (&premiere_c, PISTE_24.0)] {
        assert!(s.progressif, "{nom}");
        assert_eq!(s.pcm().1, source(nom).2, "{nom} : trames exactes");
    }
    assert_eq!(
        sha(&premiere_a.octets),
        sha(&encore_a.octets),
        "même piste, même porteur neuf : mêmes octets"
    );
}

// ─── 6. Relecture du 23/11 ──────────────────────────────────────────────────

/// Un lecteur INJOIGNABLE (il accepte la connexion et ne répond jamais) :
/// l'état ne le sonde pas, ne prend pas le verrou de sa sortie, et répond en
/// moins d'une seconde, capacité `unknown`.
#[tokio::test]
async fn un_lecteur_injoignable_ne_retient_pas_l_etat() {
    let muet = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = muet.local_addr().unwrap().port();
    tokio::spawn(async move {
        let mut tenues = Vec::new();
        while let Ok((sock, _)) = muet.accept().await {
            tenues.push(sock);
        }
    });
    let orch = orchestrateur();
    orch.outputs.lock().await.register(Box::new(DlnaOutput::new(
        "Lecteur endormi".into(),
        LECTEUR.into(),
        "127.0.0.1".into(),
        format!("http://127.0.0.1:{port}/av"),
        format!("http://127.0.0.1:{port}/rc"),
        Some(format!("http://127.0.0.1:{port}/cm")),
    )));
    let zone_id = ZoneRepo::with_backend(orch.db.clone())
        .create("Endormi", Some("dlna"), Some(LECTEUR))
        .unwrap();
    let b = Banc {
        orch,
        zone_id,
        derniere_session: Default::default(),
    };
    b.crossfeed();
    let t0 = std::time::Instant::now();
    let etat = tokio::time::timeout(Duration::from_secs(1), b.etat_tess(44_100, 16))
        .await
        .expect("l'état répond en moins d'une seconde");
    assert!(t0.elapsed() < Duration::from_secs(1));
    let d = etat.delivery.unwrap();
    assert_eq!(d.renderer_lpcm, LpcmDuLecteur::Unknown);
    assert_eq!(d.route, VoieUpnp::Untouched);
}

/// Crossfeed seul vers un lecteur qui n'annonce que le FLAC, mais zone
/// plafonnée à 48 kHz : la piste 96 kHz est CONVERTIE quoi qu'il arrive, et la
/// lecture y cuit le crossfeed. L'état le dit : `file_transcode`, crossfeed
/// effectif, pas de « Bit-perfect ». La contre-épreuve sans plafond est
/// `crossfeed_seul_sans_lpcm_part_tel_quel_et_l_etat_le_dit`.
#[tokio::test]
async fn le_format_qui_impose_la_conversion_y_cuit_le_crossfeed_et_l_etat_le_dit() {
    let b = banc(SINK_FLAC_SEUL).await;
    let id = piste(&b.orch, PISTE_24.0, PISTE_24.1, PISTE_24.2);
    ZoneRepo::with_backend(b.orch.db.clone())
        .update_max_sample_rate(b.zone_id, Some(48_000))
        .unwrap();
    b.crossfeed();
    let (fichier, _, _) = source(PISTE_24.0);
    let s = jouer(&b, id).await;
    assert_ne!(
        s.voie,
        voie::FICHIER_TEL_QUEL,
        "plafond : la piste est convertie"
    );
    assert_ne!(sha(&s.octets), sha(&fichier));
    let sid = b.derniere_session.lock().unwrap().clone().unwrap();
    let fil = b.orch.streamer.stream_output_wire(&sid).await.unwrap();
    assert!(fil.crossfeed, "le flux servi porte le crossfeed");
    let etat = b.etat_tess(96_000, 24).await;
    let d = etat.delivery.clone().unwrap();
    assert_eq!(d.route, VoieUpnp::FileTranscode, "{d:?}");
    assert!(greffon(&etat, "crossfeed").effective);
    assert_eq!(etat.badge, Badge::ProcessedByTess);
}

/// Relecture de suivi : Pure, mais zone plafonnée à 48 kHz. La piste 96 kHz
/// est convertie sans aucun étage TESS : `delivery: file_transcode`, et le
/// badge n'est PAS « Bit-perfect » (`altered_outside_tess`). Contre-épreuve :
/// sans plafond, la source part à l'octet et le badge redit « Bit-perfect ».
#[tokio::test]
async fn en_pure_une_conversion_imposee_n_est_pas_dite_bit_perfect() {
    let b = banc(SINK_FLAC_SEUL).await;
    let id = piste(&b.orch, PISTE_24.0, PISTE_24.1, PISTE_24.2);
    b.egaliseur();
    b.pure(true);
    ZoneRepo::with_backend(b.orch.db.clone())
        .update_max_sample_rate(b.zone_id, Some(48_000))
        .unwrap();
    let s = jouer(&b, id).await;
    assert_ne!(s.voie, voie::FICHIER_TEL_QUEL, "plafond : convertie");
    let etat = b.etat_tess(96_000, 24).await;
    assert_eq!(
        etat.delivery.as_ref().unwrap().route,
        VoieUpnp::FileTranscode
    );
    assert_eq!(etat.badge, Badge::AlteredOutsideTess);

    ZoneRepo::with_backend(b.orch.db.clone())
        .update_max_sample_rate(b.zone_id, None)
        .unwrap();
    let s = jouer(&b, id).await;
    assert_eq!(s.voie, voie::FICHIER_TEL_QUEL);
    assert_eq!(sha(&s.octets), sha(&source(PISTE_24.0).0));
    let etat = b.etat_tess(96_000, 24).await;
    assert_eq!(etat.delivery.as_ref().unwrap().route, VoieUpnp::Untouched);
    assert_eq!(etat.badge, Badge::BitPerfect);
}
