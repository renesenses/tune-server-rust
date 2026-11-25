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

/// LE témoin du bogue 2 sur la sortie locale : égaliseur activé, puis
/// DÉCOCHÉ dans TESS. Décision du 25/11 (A/B) : retour en 0,6 s au plus,
/// sans marche (blocs de 10 ms ; un sinus de 40 Hz nu varie de ≈ 0,0026 entre
/// deux échantillons). Sécurité auditive (#6741, fil 1974) : jamais
/// au-dessus du niveau d'avant l'égaliseur.
///
/// Rend `(instant en ms où la sortie se stabilise, rapport sortie/entrée
/// final)` ; échoue sur une marche.
async fn decocher_l_egaliseur_dans_tess(volume_avant: f64, volume_apres: f64) -> (u32, f32, bool) {
    let orch = orchestrateur();
    let zone_id = zone_locale(&orch).await;
    volume_local(&orch, volume_avant).await;
    regler_l_egaliseur(&orch, zone_id, true);
    changement_d_egaliseur_tess(&orch, zone_id).await;
    let mut trame = 0;
    let (entree, sortie) = jouer(&orch, &mut trame, 1.0).await;
    assert!(
        !identiques_au_bit_pres(&entree, &sortie),
        "contre-épreuve : l'égaliseur actif traite le signal"
    );
    volume_local(&orch, volume_apres).await;

    regler_l_egaliseur(&orch, zone_id, false);
    changement_d_egaliseur_tess(&orch, zone_id).await;
    let mut tout = Vec::new();
    let mut rapports = Vec::new();
    for _ in 1..=100u32 {
        let entree = sinus(&mut trame, (TAUX / 100) as usize);
        let mut sortie = entree.clone();
        avec_la_sortie_locale(&orch, |l| l.traiter_un_bloc_pour_test(&mut sortie)).await;
        tout.extend(sortie.chunks(2).map(|t| t[0]));
        let crete = |v: &[f32]| v.iter().fold(0.0_f32, |m, s| m.max(s.abs()));
        rapports.push(crete(&sortie) / crete(&entree));
    }
    let marche = tout
        .windows(2)
        .map(|w| (w[1] - w[0]).abs())
        .fold(0.0_f32, f32::max);
    assert!(marche <= 0.01, "marche de {marche} à la désactivation");
    let final_ = *rapports.last().unwrap();
    // Le premier bloc à partir duquel le niveau ne bouge plus.
    let stable = rapports
        .iter()
        .rposition(|r| (r - final_).abs() > 1e-4)
        .map_or(0, |i| i + 1);
    let (entree, sortie) = jouer(&orch, &mut trame, 0.5).await;
    let au_bit_pres = identiques_au_bit_pres(&entree, &sortie);
    ((stable as u32 + 1) * 10, final_, au_bit_pres)
}

/// Volume inchangé (le cas d'un A/B) : le signal sec, au bit près, en
/// 0,6 s au plus. Avant (rc13.90) : remontée à 2 dB/s, ≈ 6 s pour 12 dB.
#[tokio::test]
async fn decocher_a_volume_inchange_rend_l_entree_au_bit_pres_en_0_6_s() {
    let (stable_ms, rapport, au_bit_pres) = decocher_l_egaliseur_dans_tess(0.5, 0.5).await;
    assert!(
        stable_ms <= 600,
        "stabilisé en {stable_ms} ms, attendu ≤ 600 ms"
    );
    assert!((rapport - 1.0).abs() < 1e-6, "rapport final {rapport}");
    assert!(au_bit_pres, "la sortie doit être l'entrée au bit près");
}

/// Volume monté pendant que l'égaliseur jouait (0,25 → 0,5) : le niveau
/// revient à celui d'avant l'égaliseur (−6 dB ici), vite, JAMAIS au signal
/// sec (+6 dB au-dessus) ; le reste demeure gardé (#6741, fil 1974).
#[tokio::test]
async fn decocher_apres_un_volume_monte_s_arrete_au_niveau_d_avant() {
    let (stable_ms, rapport, au_bit_pres) = decocher_l_egaliseur_dans_tess(0.25, 0.5).await;
    assert!(stable_ms <= 600, "stabilisé en {stable_ms} ms");
    let db = 20.0 * f64::from(rapport).log10();
    assert!(
        (db + 6.02).abs() < 0.3,
        "{db} dB, attendu ≈ −6 dB (niveau d'avant)"
    );
    assert!(!au_bit_pres, "le reste du préampli reste gardé");
}

/// Même règle sur la voie réseau relevée en vol (radio, renderer DLNA) :
/// `poser_vers_le_sec_rapide` remonte vite, mais jamais au-dessus du niveau
/// d'avant l'égaliseur. Volume inchangé : sec en 0,6 s. Volume monté : le
/// volume d'un appareil réseau suit SA courbe (souvent en dB), la borne y est
/// tout ou rien (#6741, `EqEnVol::borne_de_remontee`) : tout le préampli
/// reste gardé, aucune remontée.
#[test]
fn en_vol_le_retour_rapide_suit_la_borne_de_volume() {
    for apres in [0.25, 0.5] {
        let poste = EqEnVol::pour_la_radio();
        poste.noter_le_volume(0.25);
        let profil: crate::audio::eq::EqProfile = serde_json::from_str(&profil(true)).unwrap();
        let mut courant = None;
        let mut trame = 0usize;
        let mut bloc = |courant: &mut Option<crate::audio::eq::EqProcessor>| -> f64 {
            poste.relever(courant, TAUX, 2);
            let n = (TAUX / 100) as usize;
            let mut pcm = Vec::with_capacity(n * 4);
            let mut crete_in = 0i32;
            for _ in 0..n {
                let t = trame as f64 / f64::from(TAUX);
                let v = (0.5 * (2.0 * std::f64::consts::PI * 40.0 * t).sin() * 32_767.0) as i16;
                crete_in = crete_in.max(i32::from(v).abs());
                pcm.extend_from_slice(&v.to_le_bytes());
                pcm.extend_from_slice(&v.to_le_bytes());
                trame += 1;
            }
            if let Some(eq) = courant.as_mut() {
                eq.process_pcm(&mut pcm, 16);
            }
            let crete_out = pcm
                .as_chunks::<2>()
                .0
                .iter()
                .map(|b| i32::from(i16::from_le_bytes(*b)).abs())
                .max()
                .unwrap_or(0);
            f64::from(crete_out) / f64::from(crete_in.max(1))
        };
        poste.poser(Some(profil));
        let mut avant = 0.0;
        for _ in 0..100 {
            avant = bloc(&mut courant);
        }
        poste.noter_le_volume(apres);
        poste.poser(None);
        poste.poser_vers_le_sec_rapide();
        let mut rapports = Vec::new();
        for _ in 0..100 {
            rapports.push(bloc(&mut courant));
        }
        // 0,6 s après la coupure, le niveau est atteint et ne bouge plus.
        let a_0_6 = rapports[60];
        let fin = *rapports.last().unwrap();
        let attendu = if apres <= 0.25 { 1.0 } else { avant };
        assert!(
            (fin - attendu).abs() < 0.01,
            "{apres} : rapport final {fin}, attendu {attendu} (avant la coupure : {avant})"
        );
        assert!(
            (a_0_6 - fin).abs() < 0.01,
            "{apres} : pas stabilisé à 0,6 s ({a_0_6} / {fin})"
        );
        assert!(
            rapports.iter().all(|r| *r <= 1.0 + 1e-3),
            "{apres} : jamais au-dessus du signal d'entrée"
        );
    }
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

/// Relecture de #9540 (m1) — l'ancien écran Égaliseur lit la portée : une
/// relance demandée sous le plancher arrive au plus tard à sa fin (5 s), la
/// réponse dit donc `restart` (« dans l'instant »), jamais `next_track`. Avant
/// ce correctif elle disait déjà `restart`… et la relance était abandonnée.
#[tokio::test]
async fn sous_le_plancher_la_portee_dit_relance_pas_piste_suivante() {
    let orch = orchestrateur();
    let zone_id = zone_reseau(&orch).await;
    regler_l_egaliseur(&orch, zone_id, true);
    jouer_np(&orch, zone_id, "local", "sid-portee").await;
    orch.eq_replay_last
        .lock()
        .unwrap()
        .insert(zone_id, std::time::Instant::now());
    regler_l_egaliseur(&orch, zone_id, false);
    let portee = orch.apply_eq_change_portee(zone_id).await;
    assert_eq!(portee, super::dsp::PorteeDuReglage::Relance);
    assert_eq!(portee.code(), "restart");
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

/// Relecture de #9540 (m2) — sur une station mono, un changement de
/// crossfeed seul n'entre pas dans le flux : la relance est évitée (la
/// session dit « mono ») ; un autre étage changé relance encore.
#[tokio::test]
async fn sur_une_radio_seul_le_crossfeed_change_est_reconnu() {
    let orch = orchestrateur();
    let zone_id = zone_reseau(&orch).await;
    jouer_np(&orch, zone_id, "radio", "sid-mono").await;
    flux_fabrique(&orch, zone_id, "sid-mono");
    SettingsRepo::with_backend(orch.db.clone())
        .set(
            &format!("zone_{zone_id}_crossfeed"),
            r#"{"enabled":true,"amount":0.4,"delay_ms":0.3}"#,
        )
        .unwrap();
    let np = orch.playback.get_state(zone_id).await.now_playing.unwrap();
    assert!(
        orch.flux_reseau_perime(zone_id, &np),
        "le crossfeed a changé"
    );
    assert!(orch.seuls_des_etages_stereo_ont_change(zone_id, &np));
    let _ri = regler_la_convolution(&orch, zone_id, "ri-tv-mono", 0.5);
    assert!(
        !orch.seuls_des_etages_stereo_ont_change(zone_id, &np),
        "la convolution agit aussi sur une station mono"
    );
}
