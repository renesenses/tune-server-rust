//! rc14 — retour de Rémi sur la pré-release rc13.90 : un changement de chaîne
//! TESS fait à la volée doit s'entendre.
//!
//! 1. Désactiver un greffon à la volée : la sortie redevient l'entrée, au bit
//!    près (l'égaliseur coupé gardait son préampli, pour toujours si le volume
//!    avait monté entre-temps ; une relance de flux réseau tombée sous le
//!    plancher de 5 s était ABANDONNÉE).
//! 2. Une reprise après pause compte comme une « prochaine lecture » : le flux
//!    réseau fabriqué avec l'ancienne chaîne est refait (une radio reprend le
//!    direct), au lieu de rejouer l'ancien traitement.
//! 3. En mode immédiat, une convolution (ou un crossfeed, un greffon natif)
//!    changée relance le flux réseau qui joue, comme l'égaliseur.
//!
//! Sorties factices, aucun son.

use super::PlaybackOrchestrator;
use super::dsp::RaisonDeRelance;
use super::eq_en_vol::EqEnVol;
use crate::db::settings_repo::SettingsRepo;
use crate::db::zone_repo::ZoneRepo;
use std::sync::Arc;

const SORTIE_LOCALE: &str = "DAC USB TESS";
const RENDU: &str = "dlna:uuid:salon-tess";
const TAUX: u32 = 48_000;

fn orchestrateur() -> Arc<PlaybackOrchestrator> {
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
    // La convolution est un droit Premium (#7632).
    orch.license = Some(crate::license::licence_premium_pour_test(db));
    let s = SettingsRepo::with_backend(orch.db.clone());
    s.set(crate::audio::premium_plugins::MIGRATION, "complete")
        .unwrap();
    for id in ["equalizer", "crossfeed"] {
        s.set(&format!("plugin_{id}_installed"), "true").unwrap();
        s.set(&format!("plugin_{id}_enabled"), "true").unwrap();
    }
    Arc::new(orch)
}

/// +12 dB en crête à 8 kHz : réserve automatique d'environ −12 dB. À 40 Hz,
/// seul le préampli agit.
fn profil(actif: bool) -> String {
    serde_json::to_string(&crate::audio::eq::EqProfile {
        enabled: actif,
        bands: vec![crate::audio::eq::EqBandSpec {
            freq: 8000.0,
            gain: 12.0,
            q: 1.0,
            band_type: "peak".into(),
            ..Default::default()
        }],
        ..Default::default()
    })
    .unwrap()
}

fn regler_l_egaliseur(orch: &PlaybackOrchestrator, zone_id: i64, actif: bool) {
    SettingsRepo::with_backend(orch.db.clone())
        .set(&format!("zone_{zone_id}_eq_profile"), &profil(actif))
        .unwrap();
}

/// Une RI WAV mono flottante (Dirac de gain `gain`) posée sur la zone.
fn regler_la_convolution(
    orch: &PlaybackOrchestrator,
    zone_id: i64,
    etiquette: &str,
    gain: f32,
) -> crate::test_scratch::ScratchFile {
    let f = crate::test_scratch::scratch_file(etiquette, ".wav");
    let taps = 32usize;
    let donnees = taps * 4;
    let mut w = Vec::new();
    w.extend_from_slice(b"RIFF");
    w.extend_from_slice(&((36 + donnees) as u32).to_le_bytes());
    w.extend_from_slice(b"WAVEfmt ");
    w.extend_from_slice(&16u32.to_le_bytes());
    w.extend_from_slice(&3u16.to_le_bytes());
    w.extend_from_slice(&1u16.to_le_bytes());
    w.extend_from_slice(&TAUX.to_le_bytes());
    w.extend_from_slice(&(TAUX * 4).to_le_bytes());
    w.extend_from_slice(&4u16.to_le_bytes());
    w.extend_from_slice(&32u16.to_le_bytes());
    w.extend_from_slice(b"data");
    w.extend_from_slice(&(donnees as u32).to_le_bytes());
    for i in 0..taps {
        let v: f32 = if i == 0 { gain } else { 0.0 };
        w.extend_from_slice(&v.to_le_bytes());
    }
    std::fs::write(f.path(), w).unwrap();
    SettingsRepo::with_backend(orch.db.clone())
        .set(&format!("ir_path_{zone_id}"), &f.as_str())
        .unwrap();
    f
}

fn retirer_la_convolution(orch: &PlaybackOrchestrator, zone_id: i64) {
    SettingsRepo::with_backend(orch.db.clone())
        .delete(&format!("ir_path_{zone_id}"))
        .unwrap();
}

async fn zone_reseau(orch: &PlaybackOrchestrator) -> i64 {
    ZoneRepo::with_backend(orch.db.clone())
        .create("Salon DLNA", Some("dlna"), Some(RENDU))
        .unwrap()
}

/// Une zone locale, sortie enregistrée, format déclaré (rien ne joue
/// vraiment : la chaîne est éprouvée bloc par bloc).
async fn zone_locale(orch: &PlaybackOrchestrator) -> i64 {
    let zone_id = ZoneRepo::with_backend(orch.db.clone())
        .create(
            "Bureau",
            Some("local"),
            Some(&format!("local:{SORTIE_LOCALE}")),
        )
        .unwrap();
    orch.outputs
        .lock()
        .await
        .register(Box::new(crate::outputs::local::LocalOutput::new(
            SORTIE_LOCALE.to_string(),
        )));
    avec_la_sortie_locale(orch, |l| {
        l.declare_current_format_for_test(TAUX, 2);
        l.set_pure_bypass(false);
        // Le fondu du filtre seul : la compensation (#5227) a ses témoins.
        l.set_compensation_de_niveau(false);
    })
    .await;
    zone_id
}

async fn avec_la_sortie_locale<R>(
    orch: &PlaybackOrchestrator,
    f: impl FnOnce(&crate::outputs::local::LocalOutput) -> R,
) -> R {
    let arc = orch
        .outputs
        .lock()
        .await
        .get(&format!("local:{SORTIE_LOCALE}"))
        .unwrap();
    let sortie = arc.lock().await;
    f(sortie
        .as_any()
        .downcast_ref::<crate::outputs::local::LocalOutput>()
        .unwrap())
}

async fn volume_local(orch: &PlaybackOrchestrator, v: f64) {
    let arc = orch
        .outputs
        .lock()
        .await
        .get(&format!("local:{SORTIE_LOCALE}"))
        .unwrap();
    arc.lock().await.set_volume(v).await.unwrap();
}

/// Un bloc de sinus 40 Hz stéréo, à partir de la trame `*trame`.
fn sinus(trame: &mut usize, trames: usize) -> Vec<f32> {
    let mut pcm = Vec::with_capacity(trames * 2);
    for _ in 0..trames {
        let t = *trame as f64 / f64::from(TAUX);
        let s = (0.5 * (2.0 * std::f64::consts::PI * 40.0 * t).sin()) as f32;
        pcm.push(s);
        pcm.push(s);
        *trame += 1;
    }
    pcm
}

/// Fait passer `secondes` de sinus par la chaîne locale ; rend le DERNIER
/// bloc, entrée et sortie.
async fn jouer(
    orch: &PlaybackOrchestrator,
    trame: &mut usize,
    secondes: f64,
) -> (Vec<f32>, Vec<f32>) {
    let blocs = (secondes * 10.0).ceil() as usize;
    let mut dernier = (Vec::new(), Vec::new());
    for _ in 0..blocs {
        let entree = sinus(trame, (TAUX / 10) as usize);
        let mut sortie = entree.clone();
        avec_la_sortie_locale(orch, |l| l.traiter_un_bloc_pour_test(&mut sortie)).await;
        dernier = (entree, sortie);
    }
    dernier
}

fn identiques_au_bit_pres(a: &[f32], b: &[f32]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.to_bits() == y.to_bits())
}

async fn jouer_np(orch: &PlaybackOrchestrator, zone_id: i64, source: &str, sid: &str) {
    orch.playback
        .play(
            zone_id,
            crate::playback::NowPlaying {
                title: "Écoute".into(),
                source: source.into(),
                source_id: Some("https://radio.exemple/flux".into()),
                stream_id: Some(sid.into()),
                duration_ms: if source == "radio" { 0 } else { 240_000 },
                ..Default::default()
            },
        )
        .await;
}

/// Note le traitement avec lequel le flux `sid` a été fabriqué : celui que la
/// zone demande À CET INSTANT (comme `resolve_stream_frais`).
fn flux_fabrique(orch: &PlaybackOrchestrator, zone_id: i64, sid: &str) {
    let e = orch.empreinte_du_traitement(zone_id, crate::audio::replaygain::PisteRg::AUCUNE);
    orch.noter_traitement_du_flux(zone_id, sid, e);
}

/// Ce que la route TESS fait d'un égaliseur réécrit (`rafraichir`, puis
/// `apres_le_rafraichissement` de tune-server).
async fn changement_d_egaliseur_tess(orch: &Arc<PlaybackOrchestrator>, zone_id: i64) {
    orch.apply_eq_change_portee(zone_id).await;
    orch.rendre_le_sec_si_egaliseur_coupe(zone_id).await;
}

fn relances_programmees(orch: &PlaybackOrchestrator, zone_id: i64) -> u64 {
    orch.eq_replay_gen
        .lock()
        .unwrap()
        .get(&zone_id)
        .copied()
        .unwrap_or(0)
}

// ── 1. Désactivation à la volée ────────────────────────────────────

/// LE témoin du bogue 2 sur la sortie locale : égaliseur activé à volume
/// bas, volume monté (pour compenser son préampli), puis égaliseur DÉCOCHÉ
/// dans TESS. La sortie doit redevenir l'entrée au bit près. Avant : le
/// préampli de −12 dB restait gardé indéfiniment (borne de volume de #6741),
/// la désactivation « ne faisait rien » à l'oreille.
#[tokio::test]
async fn desactiver_l_egaliseur_dans_tess_rend_l_entree_au_bit_pres() {
    let orch = orchestrateur();
    let zone_id = zone_locale(&orch).await;
    volume_local(&orch, 0.25).await;
    regler_l_egaliseur(&orch, zone_id, true);
    changement_d_egaliseur_tess(&orch, zone_id).await;
    let mut trame = 0;
    let (entree, sortie) = jouer(&orch, &mut trame, 1.0).await;
    assert!(
        !identiques_au_bit_pres(&entree, &sortie),
        "contre-épreuve : l'égaliseur actif traite le signal"
    );
    volume_local(&orch, 1.0).await;

    regler_l_egaliseur(&orch, zone_id, false);
    changement_d_egaliseur_tess(&orch, zone_id).await;
    // Pendant le fondu (300 ms), aucune marche : la plus grande variation
    // entre deux échantillons reste celle d'un sinus de 40 Hz.
    let mut tout = Vec::new();
    for _ in 0..5 {
        let (_, s) = jouer(&orch, &mut trame, 0.1).await;
        tout.extend(s.chunks(2).map(|t| t[0]));
    }
    let marche = tout
        .windows(2)
        .map(|w| (w[1] - w[0]).abs())
        .fold(0.0_f32, f32::max);
    assert!(marche <= 0.01, "marche de {marche} à la désactivation");
    // Fondu de la courbe, puis remontée de 12 dB à 2 dB/s : 7 s suffisent.
    let (entree, sortie) = jouer(&orch, &mut trame, 7.5).await;
    assert!(
        identiques_au_bit_pres(&entree, &sortie),
        "désactivé dans TESS, la sortie doit être l'entrée au bit près"
    );
    assert!(
        !avec_la_sortie_locale(&orch, |l| l.has_eq()).await,
        "plus aucun égaliseur monté"
    );
}

/// Contre-épreuve : le chemin des anciennes routes (`apply_eq_change_portee`)
/// garde le préampli quand le volume a monté — décision du 10/10 et #6741,
/// inchangée hors de TESS. C'est exactement ce que TESS faisait avant.
#[tokio::test]
async fn hors_tess_le_preampli_reste_garde_quand_le_volume_a_monte() {
    let orch = orchestrateur();
    let zone_id = zone_locale(&orch).await;
    volume_local(&orch, 0.25).await;
    regler_l_egaliseur(&orch, zone_id, true);
    orch.apply_eq_change_portee(zone_id).await;
    let mut trame = 0;
    jouer(&orch, &mut trame, 1.0).await;
    volume_local(&orch, 1.0).await;
    regler_l_egaliseur(&orch, zone_id, false);
    orch.apply_eq_change_portee(zone_id).await;
    let (entree, sortie) = jouer(&orch, &mut trame, 7.5).await;
    assert!(!identiques_au_bit_pres(&entree, &sortie));
    let rapport = sortie[0] / entree[0];
    assert!(
        (20.0 * f64::from(rapport).log10() + 12.0).abs() < 0.5,
        "≈ −12 dB gardés, vu {rapport}"
    );
}

/// Bogue 2 sur un flux réseau relevé en vol (radio décodée par Tune) :
/// décoché dans TESS, l'égaliseur est déposé « vers le sec » (préampli libéré
/// sans borne de volume), comme l'entrée en PURE.
#[tokio::test]
async fn desactiver_dans_tess_un_egaliseur_en_vol_le_rend_au_sec() {
    for tess in [true, false] {
        let orch = orchestrateur();
        let zone_id = zone_reseau(&orch).await;
        regler_l_egaliseur(&orch, zone_id, true);
        jouer_np(&orch, zone_id, "radio", "sid-en-vol").await;
        flux_fabrique(&orch, zone_id, "sid-en-vol");
        let poste = EqEnVol::pour_la_radio();
        orch.noter_eq_en_vol(zone_id, "sid-en-vol", poste.clone());

        regler_l_egaliseur(&orch, zone_id, false);
        let portee = orch.apply_eq_change_portee(zone_id).await;
        if tess {
            assert!(orch.rendre_le_sec_si_egaliseur_coupe(zone_id).await);
        }
        assert_eq!(portee, super::dsp::PorteeDuReglage::Immediate, "{tess}");
        // La relève en vol, puis (TESS) le dépôt « vers le sec » qui la
        // remplace ou la prolonge.
        assert_eq!(poste.poses(), if tess { 2 } else { 1 }, "{tess}");
        assert_eq!(
            poste.vers_le_sec_en_attente(),
            tess,
            "TESS rend le sec ; les anciennes routes gardent le préampli"
        );
    }
}

/// Bogue 2 sur un flux réseau refabriqué (fichier pré-transcodé) : la
/// désactivation qui suit l'activation de moins de 5 s tombait sous le
/// plancher de relance et était ABANDONNÉE. Elle est désormais différée à la
/// fin du plancher : la dernière demande l'emporte toujours.
#[tokio::test(start_paused = true)]
async fn une_desactivation_sous_le_plancher_est_differee_pas_abandonnee() {
    let orch = orchestrateur();
    let zone_id = zone_reseau(&orch).await;
    jouer_np(&orch, zone_id, "local", "sid-fichier").await;
    // L'activation vient de relancer le flux.
    let avant = std::time::Instant::now();
    orch.eq_replay_last.lock().unwrap().insert(zone_id, avant);

    assert!(orch.schedule_replay_pour(zone_id, RaisonDeRelance::Reglage));
    let mut relancee = false;
    for _ in 0..100 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let derniere = orch.eq_replay_last.lock().unwrap().get(&zone_id).copied();
        if derniere != Some(avant) {
            relancee = true;
            break;
        }
    }
    assert!(
        relancee,
        "la désactivation doit relancer le flux à la fin du plancher, pas se perdre"
    );
    assert!(RaisonDeRelance::Reglage.differe_sous_le_plancher());
    assert!(!RaisonDeRelance::BasculePure.differe_sous_le_plancher());
}

// ── 2. Reprise = prochaine lecture ─────────────────────────────────

/// LE témoin des bogues 1 et 3 : la convolution change pendant la pause (ou
/// en mode « à la prochaine lecture ») ; à la reprise, le flux réseau
/// fabriqué avec l'ancienne RI est reconnu périmé, sur une radio comme sur
/// un fichier — la reprise le refait au lieu de reprendre sur place.
#[tokio::test]
async fn une_convolution_changee_rend_le_flux_reseau_perime_radio_et_fichier() {
    for source in ["radio", "local"] {
        let orch = orchestrateur();
        let zone_id = zone_reseau(&orch).await;
        let _ri = regler_la_convolution(&orch, zone_id, &format!("ri-tv-a-{source}"), 0.5);
        jouer_np(&orch, zone_id, source, "sid-1").await;
        flux_fabrique(&orch, zone_id, "sid-1");
        orch.playback.pause(zone_id).await;
        let np = orch.playback.get_state(zone_id).await.now_playing.unwrap();
        assert!(
            !orch.flux_reseau_perime(zone_id, &np),
            "{source} : rien n'a changé, la reprise reste sur place"
        );

        // Autre filtre…
        let _autre = regler_la_convolution(&orch, zone_id, &format!("ri-tv-b-{source}"), 0.25);
        assert!(
            orch.flux_reseau_perime(zone_id, &np),
            "{source} : autre filtre"
        );
        // … ou convolution désactivée : périmé aussi.
        retirer_la_convolution(&orch, zone_id);
        assert!(
            orch.flux_reseau_perime(zone_id, &np),
            "{source} : désactivée"
        );
        // Et la reprise décide bien de refaire le flux.
        let decision =
            super::session::reprise_de_session(source == "radio", true, false, false, true);
        assert_ne!(
            decision,
            super::session::RepriseDeSession::SurPlace,
            "{source}"
        );
    }
}

/// Ce qu'on ne sait pas ne refait rien : sortie locale (elle traite à
/// chaud), flux sans identifiant, flux dont le traitement n'a pas été noté.
#[tokio::test]
async fn sans_certitude_la_reprise_reste_sur_place() {
    let orch = orchestrateur();
    let locale = zone_locale(&orch).await;
    let _ri = regler_la_convolution(&orch, locale, "ri-tv-locale", 0.5);
    jouer_np(&orch, locale, "radio", "sid-l").await;
    flux_fabrique(&orch, locale, "sid-l");
    retirer_la_convolution(&orch, locale);
    let np = orch.playback.get_state(locale).await.now_playing.unwrap();
    assert!(!orch.flux_reseau_perime(locale, &np), "sortie locale");

    let reseau = zone_reseau(&orch).await;
    jouer_np(&orch, reseau, "radio", "sid-inconnu").await;
    let _ri = regler_la_convolution(&orch, reseau, "ri-tv-inconnu", 0.5);
    let np = orch.playback.get_state(reseau).await.now_playing.unwrap();
    assert!(!orch.flux_reseau_perime(reseau, &np), "traitement non noté");
    let sans_id = crate::playback::NowPlaying {
        stream_id: None,
        ..np
    };
    assert!(
        !orch.flux_reseau_perime(reseau, &sans_id),
        "flux sans identifiant"
    );
}

// ── 3. Mode immédiat : la convolution relance le flux réseau ───────

#[tokio::test]
async fn en_mode_immediat_une_convolution_changee_relance_le_flux_reseau_qui_joue() {
    let orch = orchestrateur();
    let zone_id = zone_reseau(&orch).await;
    let _ri = regler_la_convolution(&orch, zone_id, "ri-tv-imm", 0.5);
    jouer_np(&orch, zone_id, "radio", "sid-imm").await;
    flux_fabrique(&orch, zone_id, "sid-imm");
    assert!(
        !orch.relancer_si_flux_reseau_perime(zone_id).await,
        "rien n'a changé : aucune relance"
    );
    assert_eq!(relances_programmees(&orch, zone_id), 0);

    retirer_la_convolution(&orch, zone_id);
    assert!(orch.relancer_si_flux_reseau_perime(zone_id).await);
    assert_eq!(relances_programmees(&orch, zone_id), 1);

    // En pause : jamais de relance (#7556) — la reprise s'en charge.
    let _ri = regler_la_convolution(&orch, zone_id, "ri-tv-imm-2", 0.25);
    orch.playback.pause(zone_id).await;
    assert!(!orch.relancer_si_flux_reseau_perime(zone_id).await);
    assert_eq!(relances_programmees(&orch, zone_id), 1);
}
