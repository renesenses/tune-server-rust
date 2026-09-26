//! #2742 — témoins de bout en bout : une piste de la BIBLIOTHÈQUE jouée vers
//! une zone RÉSEAU sans l'opt-in `dsp_progressif_reseau`, crossfeed coché.
//!
//! La piste est un FLAC stéréo fabriqué pour l'occasion : sinus 1 kHz à
//! GAUCHE, silence numérique à DROITE. Tout ce qui ressort à droite dans les
//! octets SERVIS au renderer est donc de la diaphonie, et rien d'autre. La
//! mesure passe par `resolve_local_track` — la vraie résolution — puis relit
//! ce que la session sert : le fichier d'une session fichier, ou les octets
//! tirés du canal d'une session progressive.
use super::super::{PlayRequest, PlaybackOrchestrator};
use crate::db::settings_repo::SettingsRepo;
use crate::db::zone_repo::ZoneRepo;
use crate::outputs::mock::MockOutput;
use std::sync::Arc;
use std::time::{Duration, Instant};

const SR: u32 = 44_100;
const SECONDES: u32 = 20;
const AMOUNT: f32 = 0.3;
const DELAY_MS: f32 = 0.3;
const DEVICE: &str = "uuid:diretta-renderer-2742";

fn orchestrateur() -> PlaybackOrchestrator {
    let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    let db: Arc<dyn crate::db::backend::DbBackend> = Arc::new(db);
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

/// PCM 16 bits entrelacé : sinus 1 kHz à gauche, silence à droite.
fn pcm_source() -> Vec<u8> {
    let n = (SR * SECONDES) as usize;
    let mut pcm = Vec::with_capacity(n * 4);
    for i in 0..n {
        let t = i as f64 / SR as f64;
        let l = (2.0 * std::f64::consts::PI * 1000.0 * t).sin() * 0.5;
        pcm.extend_from_slice(&((l * 32767.0) as i16).to_le_bytes());
        pcm.extend_from_slice(&0i16.to_le_bytes());
    }
    pcm
}

/// Diaphonie D/G en dB d'un PCM entrelacé stéréo.
fn diaphonie_pcm(pcm: &[u8], bit_depth: u16) -> f64 {
    let o = bit_depth as usize / 8;
    let lire = |b: &[u8]| -> f64 {
        match o {
            2 => i16::from_le_bytes([b[0], b[1]]) as f64 / 32_768.0,
            3 => (i32::from_le_bytes([0, b[0], b[1], b[2]]) >> 8) as f64 / 8_388_608.0,
            _ => i32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f64 / 2_147_483_648.0,
        }
    };
    let (mut el, mut er) = (0.0f64, 0.0f64);
    for f in pcm.chunks_exact(o * 2) {
        let (l, r) = (lire(&f[..o]), lire(&f[o..]));
        el += l * l;
        er += r * r;
    }
    10.0 * (er.max(1e-30) / el.max(1e-30)).log10()
}

fn diaphonie_fichier(chemin: &str) -> f64 {
    let d = crate::audio::decode::decode_to_pcm(chemin, None, None, 0.0, 0.0).unwrap();
    assert_eq!(d.channels, 2);
    diaphonie_pcm(&d.pcm_bytes(), d.bit_depth)
}

/// La diaphonie d'UNE passe de crossfeed, et celle de DEUX, sur le même
/// signal : l'étalon qui dit si les octets servis portent le crossfeed une
/// fois, deux fois, ou pas du tout.
fn etalons() -> (f64, f64) {
    let mut une = pcm_source();
    crate::audio::crossfeed::CrossfeedProcessor::new(SR, AMOUNT, DELAY_MS)
        .process_pcm(&mut une, 16, 2);
    let mut deux = une.clone();
    crate::audio::crossfeed::CrossfeedProcessor::new(SR, AMOUNT, DELAY_MS)
        .process_pcm(&mut deux, 16, 2);
    (diaphonie_pcm(&une, 16), diaphonie_pcm(&deux, 16))
}

struct Montage {
    orch: PlaybackOrchestrator,
    zone_id: i64,
    source: String,
    _dir: tempfile::TempDir,
}

#[derive(Clone, Copy)]
enum Renderer {
    /// Absent du registre : présumé capable (règle de `dlna_accepte_lpcm`,
    /// comme `dlna_supports_mime`) — il ANNONCE le LPCM.
    AnnonceLeLpcm,
    /// Sortie enregistrée sans Sink `GetProtocolInfo` : aucun LPCM annoncé.
    SansLpcm,
}

async fn monter(renderer: Renderer, sortie: Option<(&str, &str)>) -> Montage {
    let orch = orchestrateur();
    let dir = tempfile::tempdir().unwrap();
    let piste = dir.path().join("piste.flac");
    let mut enc = crate::audio::encoder::AudioEncoder::new("flac", SR, 16, 2);
    enc.start().await.unwrap();
    enc.write(&pcm_source()).await.unwrap();
    std::fs::write(&piste, enc.finish().await.unwrap()).unwrap();
    let source = piste.to_string_lossy().into_owned();
    orch.db
        .execute("INSERT INTO artists (id, name) VALUES (1, 'A')", &[])
        .unwrap();
    orch.db
        .execute(
            "INSERT INTO albums (id, title, artist_id) VALUES (1, 'B', 1)",
            &[],
        )
        .unwrap();
    orch.db
        .execute(
            &format!(
                "INSERT INTO tracks (id, title, album_id, artist_id, file_path, format, \
                 duration_ms, sample_rate, bit_depth, channels) \
                 VALUES (1, 'T', 1, 1, ?, 'flac', {}, {SR}, 16, 2)",
                SECONDES as i64 * 1000
            ),
            &[&source as &dyn crate::db::backend::ToSqlValue],
        )
        .unwrap();
    let (type_sortie, device) = sortie.unwrap_or(("dlna", DEVICE));
    let zones = ZoneRepo::with_backend(orch.db.clone());
    let zone_id = zones
        .create("C19", Some(type_sortie), Some(device))
        .unwrap();
    // FLAC natif imposé, comme le renderer Diretta de Tades : la sortie
    // factice n'a pas de Sink, et la négociation basculerait sinon en WAV.
    zones.update_dlna_native_flac(zone_id, true).unwrap();
    if let Renderer::SansLpcm = renderer {
        orch.outputs
            .lock()
            .await
            .register(Box::new(MockOutput::new(DEVICE, "C19").with_type("dlna")));
    }
    SettingsRepo::with_backend(orch.db.clone())
        .set(
            &format!("zone_{zone_id}_crossfeed"),
            &format!(r#"{{"enabled":true,"amount":{AMOUNT},"delay_ms":{DELAY_MS}}}"#),
        )
        .unwrap();
    // L'opt-in n'est JAMAIS posé : c'est la fiche de Tades.
    assert_ne!(
        SettingsRepo::with_backend(orch.db.clone())
            .get("dsp_progressif_reseau")
            .ok()
            .flatten()
            .as_deref(),
        Some("true")
    );
    Montage {
        orch,
        zone_id,
        source,
        _dir: dir,
    }
}

fn armer_l_egaliseur(m: &Montage) {
    let s = SettingsRepo::with_backend(m.orch.db.clone());
    s.set("plugin_equalizer_installed", "true").unwrap();
    let p = crate::audio::eq::EqProfile {
        enabled: true,
        bands: vec![crate::audio::eq::EqBandSpec {
            freq: 80.0,
            gain: 3.0,
            q: 0.71,
            band_type: "low_shelf".into(),
            ..Default::default()
        }],
        ..Default::default()
    };
    s.set(
        &format!("zone_{}_eq_profile", m.zone_id),
        &serde_json::to_string(&p).unwrap(),
    )
    .unwrap();
    assert!(m.orch.zone_has_active_eq(m.zone_id));
}

/// Ce que le renderer reçoit.
struct Servi {
    mime: String,
    /// Les octets servis sont-ils EXACTEMENT ceux de la source ?
    tel_quel: bool,
    /// Session progressive (canal) plutôt que fichier.
    progressif: bool,
    diaphonie_db: f64,
    premier_octet: Duration,
    /// #5114 — ce que le flux ANNONCE porter (`StreamInfo::crossfeed`, lu par
    /// `stream_output_wire`, l'accesseur du chemin du signal). Confronté à la
    /// diaphonie MESURÉE dans les octets servis.
    annonce_crossfeed: bool,
}

async fn jouer(m: &Montage, output_device_id: Option<&str>) -> Servi {
    let req = PlayRequest {
        zone_id: m.zone_id,
        output_device_id: output_device_id.map(str::to_string),
        track_id: Some(1),
        source: Some("local".into()),
        ..Default::default()
    };
    let t0 = Instant::now();
    let r = m.orch.resolve_local_track(&req).await.unwrap();
    let sid = r.stream_id.clone().expect("une session");
    let session = m
        .orch
        .streamer
        .sessions_state()
        .lock()
        .await
        .get(&sid)
        .cloned()
        .expect("session inscrite");
    let annonce_crossfeed = m
        .orch
        .streamer
        .stream_output_wire(&sid)
        .await
        .expect("fil publié")
        .crossfeed;
    let fichier = session.file_path.lock().await.clone();
    if let Some(chemin) = fichier {
        let premier_octet = t0.elapsed();
        let tel_quel = std::fs::read(&chemin).unwrap() == std::fs::read(&m.source).unwrap();
        return Servi {
            mime: r.mime_type,
            tel_quel,
            progressif: false,
            diaphonie_db: diaphonie_fichier(&chemin),
            premier_octet,
            annonce_crossfeed,
        };
    }
    // Session progressive : on tire le canal comme le ferait le renderer.
    let mut octets = Vec::new();
    let mut premier_octet = None;
    while let Some(c) = tokio::time::timeout(Duration::from_secs(60), session.recv_chunk())
        .await
        .expect("le flux progressif s'est figé")
    {
        if premier_octet.is_none() && !c.is_empty() {
            premier_octet = Some(t0.elapsed());
        }
        octets.extend_from_slice(&c);
    }
    let dir = tempfile::tempdir().unwrap();
    let wav = dir.path().join("servi.wav");
    std::fs::write(&wav, &octets).unwrap();
    Servi {
        mime: r.mime_type,
        tel_quel: false,
        progressif: true,
        diaphonie_db: diaphonie_fichier(&wav.to_string_lossy()),
        premier_octet: premier_octet.expect("aucun octet servi"),
        annonce_crossfeed,
    }
}

fn journal(cas: &str, s: &Servi) {
    println!(
        "#2742 {cas} : mime={} tel_quel={} progressif={} diaphonie={:.1} dB premier_octet={} ms \
         annonce_crossfeed={}",
        s.mime,
        s.tel_quel,
        s.progressif,
        s.diaphonie_db,
        s.premier_octet.as_millis(),
        s.annonce_crossfeed
    );
}

/// Les octets servis portent le crossfeed UNE fois : ni zéro, ni deux.
fn porte_le_crossfeed_une_fois(cas: &str, s: &Servi) {
    let (une, deux) = etalons();
    assert!(
        deux - une > 2.0,
        "étalons trop proches pour distinguer une passe de deux : {une:.1} / {deux:.1} dB"
    );
    assert!(
        (s.diaphonie_db - une).abs() < 1.0,
        "{cas} : la diaphonie servie ({:.1} dB) doit être celle d'UNE passe de \
         crossfeed ({une:.1} dB) — deux passes donneraient {deux:.1} dB, aucune \
         donne un silence à droite",
        s.diaphonie_db
    );
    // #5114 — et le flux le DIT : c'est ce que le chemin du signal affiche.
    assert!(
        s.annonce_crossfeed,
        "{cas} : les octets portent le crossfeed, le flux doit l'annoncer"
    );
}

fn ne_porte_aucun_crossfeed(cas: &str, s: &Servi) {
    assert!(
        s.diaphonie_db < -60.0,
        "{cas} : aucune diaphonie attendue, mesuré {:.1} dB",
        s.diaphonie_db
    );
    assert!(
        !s.annonce_crossfeed,
        "{cas} : aucun crossfeed dans les octets, le flux ne doit pas en annoncer"
    );
}

/// Cas 1 — égaliseur ACTIF et crossfeed : la piste est déjà ré-encodée par le
/// fichier entier (le chemin d'avant, inchangé). Ce ré-encodage doit porter
/// le crossfeed, une seule fois, sans changer le format servi.
#[tokio::test]
async fn cas_1_le_reencodage_existant_porte_le_crossfeed_2742() {
    let m = monter(Renderer::SansLpcm, None).await;
    armer_l_egaliseur(&m);
    let s = jouer(&m, None).await;
    journal("cas 1 (égaliseur + crossfeed)", &s);
    assert_eq!(s.mime, "audio/flac", "le format servi ne change pas");
    assert!(!s.progressif, "le chemin reste le fichier, comme avant");
    porte_le_crossfeed_une_fois("cas 1", &s);
}

/// Cas 2 — crossfeed SEUL, renderer qui ANNONCE le LPCM : WAV progressif, le
/// crossfeed au fil de l'eau, premier octet sans attendre le fichier entier.
/// Le format servi change SANS l'opt-in : décision de Bertrand (24/09), le
/// crossfeed coché vaut consentement pour ce seul cas.
#[tokio::test]
async fn cas_2_crossfeed_seul_part_en_wav_progressif_si_le_renderer_lit_le_lpcm_2742() {
    let m = monter(Renderer::AnnonceLeLpcm, None).await;
    let s = jouer(&m, None).await;
    journal("cas 2 (crossfeed seul, LPCM annoncé)", &s);
    assert_eq!(s.mime, "audio/wav");
    assert!(
        s.progressif,
        "session progressive, jamais le fichier entier"
    );
    porte_le_crossfeed_une_fois("cas 2", &s);
}

/// Cas 3 — crossfeed SEUL, renderer SANS LPCM : rien ne change. La piste part
/// telle quelle, au même format, sans aucun ré-encodage — donc sans crossfeed,
/// et le statut le dit (`network_progressive_off`).
#[tokio::test]
async fn cas_3_crossfeed_seul_sans_lpcm_la_piste_part_telle_quelle_2742() {
    let m = monter(Renderer::SansLpcm, None).await;
    let s = jouer(&m, None).await;
    journal("cas 3 (crossfeed seul, sans LPCM)", &s);
    assert_eq!(s.mime, "audio/flac", "format servi inchangé");
    assert!(s.tel_quel, "les octets de la source, sans ré-encodage");
    ne_porte_aucun_crossfeed("cas 3", &s);
    let statut = crate::audio::crossfeed::crossfeed_status(true, false, true, false, false, false);
    assert_eq!(
        statut.reason,
        Some(crate::audio::crossfeed::CrossfeedConstraint::NetworkProgressiveOff),
        "la réserve reste nommée pour ce seul cas"
    );
}

/// PURE coupe tout, sur les deux renderers : la source part telle quelle.
#[tokio::test]
async fn en_mode_pure_rien_n_est_applique_2742() {
    for renderer in [Renderer::AnnonceLeLpcm, Renderer::SansLpcm] {
        let m = monter(renderer, None).await;
        armer_l_egaliseur(&m);
        SettingsRepo::with_backend(m.orch.db.clone())
            .set(
                &format!("zone_{}_audiophile", m.zone_id),
                r#"{"enabled":true}"#,
            )
            .unwrap();
        let s = jouer(&m, None).await;
        journal("PURE", &s);
        assert!(s.tel_quel, "PURE : la source telle quelle");
        ne_porte_aucun_crossfeed("PURE", &s);
    }
}

/// Sans les droits du greffon (installé à faux, migration passée) : pas de
/// crossfeed, et pas de changement de format pour un crossfeed inexistant.
#[tokio::test]
async fn sans_le_greffon_rien_ne_change_2742() {
    let m = monter(Renderer::AnnonceLeLpcm, None).await;
    let s = SettingsRepo::with_backend(m.orch.db.clone());
    s.set(crate::audio::premium_plugins::MIGRATION, "complete")
        .unwrap();
    s.set("plugin_crossfeed_installed", "false").unwrap();
    assert!(!m.orch.zone_has_active_crossfeed(m.zone_id));
    let servi = jouer(&m, None).await;
    journal("sans greffon", &servi);
    assert_eq!(servi.mime, "audio/flac");
    assert!(servi.tel_quel);
    ne_porte_aucun_crossfeed("sans greffon", &servi);
}

/// Sortie LOCALE : `LocalOutput` applique le crossfeed dans sa boucle de
/// lecture. Les octets que le serveur lui sert ne doivent donc PAS le porter
/// — sinon il s'entendrait deux fois (le cumul EQ/ReplayGain de v0.9.139).
/// Égaliseur armé en plus, pour couvrir aussi le chemin du ré-encodage.
#[tokio::test]
async fn une_sortie_locale_ne_recoit_jamais_le_crossfeed_du_serveur_2742() {
    let m = monter(Renderer::AnnonceLeLpcm, Some(("local", "local:Casque"))).await;
    armer_l_egaliseur(&m);
    let s = jouer(&m, Some("local:Casque")).await;
    journal("sortie locale", &s);
    ne_porte_aucun_crossfeed("sortie locale", &s);
}

/// Le chemin des SERVICES (Qobuz, Tidal, YouTube) n'est pas touché : sa
/// chaîne `load_streaming_dsp` applique toujours le crossfeed UNE fois — le
/// bras streaming ne traverse ni le ré-encodage de la bibliothèque ni son
/// relais.
#[tokio::test]
async fn le_chemin_des_services_garde_une_seule_passe_2742() {
    let m = monter(Renderer::AnnonceLeLpcm, None).await;
    let mut pcm = pcm_source();
    let mut chaine = m.orch.load_streaming_dsp(m.zone_id, None, SR, 2);
    chaine.process(&mut pcm, 16);
    let s = Servi {
        mime: "audio/flac".into(),
        tel_quel: false,
        progressif: false,
        diaphonie_db: diaphonie_pcm(&pcm, 16),
        premier_octet: Duration::ZERO,
        // Le fait que les bras des services posent sur leur session.
        annonce_crossfeed: chaine.is_active() && chaine.crossfeed_executable(),
    };
    journal("services", &s);
    porte_le_crossfeed_une_fois("services", &s);
}

/// #5114 — licence ÉCHUE : l'orchestrateur ne charge plus le crossfeed, le
/// flux n'en porte donc pas (mesuré), ne l'annonce pas, et le miroir de la
/// compensation ne le compte plus — il rend exactement la cible de
/// l'égaliseur seul, au lieu de la surestimer du gain moyen du crossfeed.
#[tokio::test]
async fn licence_echue_ni_crossfeed_servi_ni_compte_par_le_miroir_5114() {
    let mut m = monter(Renderer::AnnonceLeLpcm, None).await;
    armer_l_egaliseur(&m);
    let miroir = |m: &Montage| {
        PlaybackOrchestrator::compensation_reseau_prevue_with(
            &m.orch.db,
            m.orch.license.as_deref(),
            m.zone_id,
        )
    };
    let premium = miroir(&m).expect("égaliseur et crossfeed : compensés");
    m.orch.license = Some(Arc::new(crate::license::LicenseManager::new(
        m.orch.db.clone(),
    )));
    assert!(
        !m.orch.license.as_ref().unwrap().premium_snapshot(),
        "le témoin exige une licence échue"
    );
    assert!(m.orch.crossfeed_configure(m.zone_id).is_none());
    let echue = miroir(&m).expect("l'égaliseur, gratuit, reste compensé");
    // L'étalon : la même zone sans crossfeed du tout.
    SettingsRepo::with_backend(m.orch.db.clone())
        .set(
            &format!("zone_{}_crossfeed", m.zone_id),
            r#"{"enabled":false}"#,
        )
        .unwrap();
    let egaliseur_seul = miroir(&m).expect("égaliseur seul : compensé");
    println!(
        "#5114 miroir : premium {premium:.2} dB, licence échue {echue:.2} dB, \
         égaliseur seul {egaliseur_seul:.2} dB"
    );
    assert!(
        (echue - egaliseur_seul).abs() < 1e-9,
        "licence échue : le miroir doit rendre la cible de l'égaliseur seul \
         ({egaliseur_seul:.2} dB), il rend {echue:.2} dB"
    );
    assert!(
        premium - echue > 0.05,
        "le témoin exige un crossfeed qui déplace la cible : {premium:.2} / {echue:.2} dB"
    );
    // Et le flux, crossfeed recoché : toujours rien, la licence tranche.
    SettingsRepo::with_backend(m.orch.db.clone())
        .set(
            &format!("zone_{}_crossfeed", m.zone_id),
            &format!(r#"{{"enabled":true,"amount":{AMOUNT},"delay_ms":{DELAY_MS}}}"#),
        )
        .unwrap();
    let s = jouer(&m, None).await;
    journal("licence échue", &s);
    ne_porte_aucun_crossfeed("licence échue", &s);
}

/// Les deux règles pures du module, table complète.
#[test]
fn les_regles_du_module_2742() {
    use super::{crossfeed_cuit_dans_le_fichier, wav_progressif_consenti};
    // Opt-in coché : inchangé, quel que soit le traitement.
    assert!(wav_progressif_consenti(true, false, true));
    assert!(wav_progressif_consenti(true, true, false));
    // Crossfeed SEUL sur zone réseau : consentement (cas 2).
    assert!(wav_progressif_consenti(false, true, false));
    // Un autre traitement ré-encode déjà : pas de changement de format (cas 1).
    assert!(!wav_progressif_consenti(false, true, true));
    // Ni opt-in ni crossfeed : rien.
    assert!(!wav_progressif_consenti(false, false, true));
    assert!(!wav_progressif_consenti(false, false, false));

    assert!(crossfeed_cuit_dans_le_fichier(true, false));
    assert!(
        !crossfeed_cuit_dans_le_fichier(true, true),
        "une sortie locale l'applique elle-même"
    );
    assert!(
        !crossfeed_cuit_dans_le_fichier(false, false),
        "navigateur, PULL : hors du périmètre mesuré"
    );
}

/// Le crossfeed entre dans la clé du cache : sans lui, la rendition du
/// pré-chauffage (`queue.rs`, sans aucun traitement) serait servie à une zone
/// qui l'a coché.
#[test]
fn le_crossfeed_entre_dans_la_cle_du_cache_2742() {
    use super::empreinte_avec_crossfeed;
    assert_eq!(empreinte_avec_crossfeed(None, None), None, "clé d'avant");
    let eq = Some([7u8; 32]);
    assert_eq!(empreinte_avec_crossfeed(eq, None), eq, "clé d'avant");
    let a = empreinte_avec_crossfeed(None, Some((0.3, 0.3)));
    assert!(a.is_some(), "crossfeed seul : une clé propre");
    assert_ne!(a, empreinte_avec_crossfeed(None, Some((0.4, 0.3))));
    assert_ne!(a, empreinte_avec_crossfeed(None, Some((0.3, 0.5))));
    assert_ne!(empreinte_avec_crossfeed(eq, Some((0.3, 0.3))), eq);
    assert_ne!(empreinte_avec_crossfeed(eq, Some((0.3, 0.3))), a);
}
