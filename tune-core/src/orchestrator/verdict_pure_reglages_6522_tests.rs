//! #6522, #6524 — le verdict PURE de la sortie locale, dans l'état que le
//! serveur produit RÉELLEMENT.
//!
//! PURE vide la chaîne vivante de la sortie : à la bascule,
//! `refresh_zone_pure_dsp` y installe les `None` que les chargeurs rendent en
//! PURE (`load_eq_processor`, `load_crossfeed_processor`, `zone_mono_downmix`).
//! Le verdict, qui regardait cette chaîne, disait donc « PURE actif : aucun
//! traitement armé, rien à contourner » quel que soit le réglage de la zone
//! (bouclier creux chez Didier, fil 2333, et chez Jean Valjean, fil 1798).
//!
//! Le témoin `pure_avec_un_egaliseur_arme_le_contourne_toujours`
//! (`outputs/local/tests.rs`) place un égaliseur dans la chaîne AVEC PURE
//! allumé : un état que l'orchestrateur ne produit jamais. Ici, chaque cas part
//! des réglages enregistrés, passe par la VRAIE bascule de l'orchestrateur, et
//! lit le verdict par `OutputTarget::signal_path_status`, la lecture du poller.
//! Le cas du greffon natif tiers exige une bibliothèque native : il vit dans
//! `greffon_natif_tiers_tests.rs`, avec les autres.
use super::PlaybackOrchestrator;
use crate::db::settings_repo::SettingsRepo;
use crate::db::zone_repo::ZoneRepo;
use crate::outputs::traits::{OutputDspState, OutputTarget};
use std::sync::Arc;

const SANS_OBJET: OutputDspState = OutputDspState::PureSansObjet;

fn orchestrateur() -> PlaybackOrchestrator {
    let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    let db: Arc<dyn crate::db::backend::DbBackend> = Arc::new(db);
    // #7632 — la convolution est un droit Premium, et un orchestrateur sans
    // licence n'y a pas droit : ce banc, qui arme une RI, porte un compte
    // Premium (rien d'autre n'y dépend de la licence).
    let licence = crate::license::licence_premium_pour_test(db.clone());
    let mut orch = PlaybackOrchestrator::new(
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
    );
    orch.license = Some(licence);
    orch
}

/// Une zone locale sur `local:DAC`, sortie enregistrée, sans aucun réglage.
async fn zone_locale(orch: &PlaybackOrchestrator) -> i64 {
    let zone_id = ZoneRepo::with_backend(orch.db.clone())
        .create("Haut-parleurs", Some("local"), Some("local:DAC"))
        .unwrap();
    orch.outputs
        .lock()
        .await
        .register(Box::new(crate::outputs::local::LocalOutput::new(
            "DAC".to_string(),
        )));
    zone_id
}

async fn avec_sortie_locale<T>(
    orch: &PlaybackOrchestrator,
    f: impl FnOnce(&crate::outputs::local::LocalOutput) -> T,
) -> T {
    let arc = orch.outputs.lock().await.get("local:DAC").unwrap();
    let sortie = arc.lock().await;
    let local = sortie
        .as_any()
        .downcast_ref::<crate::outputs::local::LocalOutput>()
        .expect("sortie locale");
    f(local)
}

fn reglages(orch: &PlaybackOrchestrator) -> SettingsRepo {
    SettingsRepo::with_backend(orch.db.clone())
}

fn regler_pure(orch: &PlaybackOrchestrator, zone_id: i64, actif: bool) {
    reglages(orch)
        .set(
            &format!("zone_{zone_id}_audiophile"),
            &format!(r#"{{"enabled":{actif}}}"#),
        )
        .unwrap();
}

/// Égaliseur ACTIVÉ et audible (Graves +, +8 dB à 80 Hz), greffon installé.
fn armer_l_egaliseur(orch: &PlaybackOrchestrator, zone_id: i64) {
    let profil = crate::audio::eq::EqProfile {
        enabled: true,
        bands: vec![crate::audio::eq::EqBandSpec {
            freq: 80.0,
            gain: 8.0,
            q: 0.71,
            band_type: "low_shelf".into(),
            ..Default::default()
        }],
        ..Default::default()
    };
    let s = reglages(orch);
    s.set(
        &format!("zone_{zone_id}_eq_profile"),
        &serde_json::to_string(&profil).unwrap(),
    )
    .unwrap();
    s.set("plugin_equalizer_installed", "true").unwrap();
}

fn armer_le_crossfeed(orch: &PlaybackOrchestrator, zone_id: i64) {
    reglages(orch)
        .set(
            &format!("zone_{zone_id}_crossfeed"),
            r#"{"enabled":true,"amount":0.3}"#,
        )
        .unwrap();
}

fn armer_le_repli_mono(orch: &PlaybackOrchestrator, zone_id: i64) {
    reglages(orch)
        .set(&format!("zone_{zone_id}_mono_downmix"), "true")
        .unwrap();
}

/// La correction de pièce : la clé ET un fichier qui existe, comme l'exige
/// `traitement_que_pure_gouverne`. Le fichier est rendu pour vivre le temps
/// du test.
fn armer_la_convolution(orch: &PlaybackOrchestrator, zone_id: i64) -> tempfile::NamedTempFile {
    let ir = tempfile::Builder::new().suffix(".wav").tempfile().unwrap();
    reglages(orch)
        .set(&format!("ir_path_{zone_id}"), &ir.path().to_string_lossy())
        .unwrap();
    ir
}

/// Ce que fait le chemin de lecture au démarrage d'une piste HORS PURE : la
/// chaîne reçoit ce que les chargeurs rendent pour les réglages de la zone.
async fn demarrer_une_piste_hors_pure(orch: &PlaybackOrchestrator, zone_id: i64) {
    avec_sortie_locale(orch, |local| {
        local.declare_current_format_for_test(44_100, 2);
        local.set_pure_bypass(false);
        local.set_eq(orch.load_eq_processor(zone_id, 44_100, 2));
        local.set_crossfeed(orch.load_crossfeed_processor(zone_id, 44_100));
        local.set_mono_downmix(orch.zone_mono_downmix(zone_id));
    })
    .await;
}

/// Le verdict tel que le poller le relève : le contrat publié par la sortie
/// (même fonction que le callback Windows), lu par `signal_path_status`.
async fn verdict(orch: &PlaybackOrchestrator) -> OutputDspState {
    avec_sortie_locale(orch, |local| {
        local.publier_le_contrat_pour_test();
        local
            .signal_path_status()
            .expect("la sortie a publié son contrat")
            .dsp
    })
    .await
}

/// L'état réel : réglage enregistré, piste démarrée hors PURE, puis PURE
/// allumé par la vraie bascule. Rend le verdict lu après la bascule.
async fn verdict_apres_la_bascule_pure(
    armer: impl FnOnce(&PlaybackOrchestrator, i64),
) -> OutputDspState {
    let orch = orchestrateur();
    let zone_id = zone_locale(&orch).await;
    armer(&orch, zone_id);
    demarrer_une_piste_hors_pure(&orch, zone_id).await;

    regler_pure(&orch, zone_id, true);
    assert!(
        orch.refresh_zone_pure_dsp(zone_id).await,
        "une sortie locale vivante doit recevoir la bascule"
    );
    avec_sortie_locale(&orch, |local| {
        assert!(local.pure_bypass_for_test(), "PURE doit être armé");
        assert!(
            !local.has_eq(),
            "prémisse de #6522 : PURE a vidé l'égaliseur de la chaîne vivante"
        );
    })
    .await;
    verdict(&orch).await
}

#[tokio::test]
async fn pure_sur_une_zone_avec_egaliseur_arme_le_contourne() {
    assert_eq!(
        verdict_apres_la_bascule_pure(armer_l_egaliseur).await,
        OutputDspState::BypassedPure,
        "égaliseur armé dans les réglages + PURE = « DSP contourné par PURE », \
         pas « rien à contourner » (#6524, fil 1798)"
    );
}

#[tokio::test]
async fn pure_sur_une_zone_avec_crossfeed_arme_le_contourne() {
    assert_eq!(
        verdict_apres_la_bascule_pure(armer_le_crossfeed).await,
        OutputDspState::BypassedPure
    );
}

#[tokio::test]
async fn pure_sur_une_zone_avec_repli_mono_arme_le_contourne() {
    assert_eq!(
        verdict_apres_la_bascule_pure(armer_le_repli_mono).await,
        OutputDspState::BypassedPure
    );
}

#[tokio::test]
async fn pure_sur_une_zone_avec_correction_de_piece_la_contourne() {
    let mut garde = None;
    let dsp = verdict_apres_la_bascule_pure(|orch, zone_id| {
        garde = Some(armer_la_convolution(orch, zone_id));
    })
    .await;
    assert_eq!(dsp, OutputDspState::BypassedPure);
    drop(garde);
}

/// Le témoin inverse : rien d'armé, PURE n'a rien à contourner, et le
/// verdict de #4176 tient (le client l'affiche par sa description).
#[tokio::test]
async fn pure_sur_une_zone_sans_aucun_traitement_reste_sans_objet() {
    assert_eq!(verdict_apres_la_bascule_pure(|_, _| {}).await, SANS_OBJET);
}

/// Un réglage ENREGISTRÉ mais inaudible (égaliseur activé à gains nuls,
/// crossfeed coché à dosage nul) n'est pas un traitement : sans objet.
#[tokio::test]
async fn pure_sur_des_reglages_inaudibles_reste_sans_objet() {
    let dsp = verdict_apres_la_bascule_pure(|orch, zone_id| {
        let plat = crate::audio::eq::EqProfile {
            enabled: true,
            ..Default::default()
        };
        let s = reglages(orch);
        s.set(
            &format!("zone_{zone_id}_eq_profile"),
            &serde_json::to_string(&plat).unwrap(),
        )
        .unwrap();
        s.set("plugin_equalizer_installed", "true").unwrap();
        s.set(
            &format!("zone_{zone_id}_crossfeed"),
            r#"{"enabled":true,"amount":0.0}"#,
        )
        .unwrap();
        s.set(&format!("zone_{zone_id}_mono_downmix"), "false")
            .unwrap();
    })
    .await;
    assert_eq!(dsp, SANS_OBJET);
}

/// Sous PURE, armer l'égaliseur en cours d'écoute (le bandeau « L'égaliseur
/// n'agit pas en mode PURE ») change ce que PURE contourne : le verdict doit
/// le suivre sans attendre la piste suivante. Puis le retirer le ramène à
/// « sans objet ».
#[tokio::test]
async fn armer_puis_retirer_l_egaliseur_sous_pure_suit_le_verdict() {
    let orch = Arc::new(orchestrateur());
    let zone_id = zone_locale(&orch).await;
    demarrer_une_piste_hors_pure(&orch, zone_id).await;
    regler_pure(&orch, zone_id, true);
    assert!(orch.refresh_zone_pure_dsp(zone_id).await);
    assert_eq!(verdict(&orch).await, SANS_OBJET);

    armer_l_egaliseur(&orch, zone_id);
    orch.apply_eq_change(zone_id).await;
    assert_eq!(
        verdict(&orch).await,
        OutputDspState::BypassedPure,
        "l'égaliseur vient d'être armé sous PURE : PURE le contourne"
    );

    reglages(&orch)
        .delete(&format!("zone_{zone_id}_eq_profile"))
        .unwrap();
    orch.apply_eq_change(zone_id).await;
    assert_eq!(verdict(&orch).await, SANS_OBJET);
}

/// Sortir de PURE : le verdict redevient celui de la chaîne (« appliqué »),
/// le drapeau des réglages ne le touche plus.
#[tokio::test]
async fn hors_pure_le_verdict_reste_celui_de_la_chaine() {
    let orch = orchestrateur();
    let zone_id = zone_locale(&orch).await;
    armer_l_egaliseur(&orch, zone_id);
    demarrer_une_piste_hors_pure(&orch, zone_id).await;
    regler_pure(&orch, zone_id, true);
    assert!(orch.refresh_zone_pure_dsp(zone_id).await);
    regler_pure(&orch, zone_id, false);
    assert!(orch.refresh_zone_pure_dsp(zone_id).await);
    assert_eq!(verdict(&orch).await, OutputDspState::Applied);
}

// ── #4176 (campagne du 04/11) : égaliseur coupé, PURE et « Remettre à plat » ──

/// Le profil de `armer_l_egaliseur`, mis sur « Inactif » (bandes gardées) ou
/// remis à plat (bandes à 0 dB, toujours inactif) : ce qu'enregistre le
/// client web pour ces deux gestes.
fn enregistrer_l_egaliseur_coupe(orch: &PlaybackOrchestrator, zone_id: i64, gain: f64) {
    let profil = crate::audio::eq::EqProfile {
        enabled: false,
        bands: vec![crate::audio::eq::EqBandSpec {
            freq: 80.0,
            gain,
            q: 0.71,
            band_type: "low_shelf".into(),
            ..Default::default()
        }],
        ..Default::default()
    };
    reglages(orch)
        .set(
            &format!("zone_{zone_id}_eq_profile"),
            &serde_json::to_string(&profil).unwrap(),
        )
        .unwrap();
}

async fn regler_le_volume(orch: &PlaybackOrchestrator, volume: f64) {
    let arc = orch.outputs.lock().await.get("local:DAC").unwrap();
    arc.lock()
        .await
        .set_volume(volume)
        .await
        .expect("set_volume");
}

async fn preampli_garde_db(orch: &PlaybackOrchestrator) -> Option<f64> {
    avec_sortie_locale(orch, |local| local.preampli_garde().map(|g| g.db)).await
}

/// Égaliseur armé, piste démarrée, puis « Inactif » : la sortie garde le
/// préampli de l'égaliseur (décision du 10/10). PURE allumé ensuite retire ce
/// gain — le niveau remonte. Les réglages, eux, disent « égaliseur
/// désactivé » : le bouclier doit pourtant être plein, PURE a contourné
/// quelque chose.
#[tokio::test]
async fn pure_sur_un_egaliseur_coupe_qui_garde_son_preampli_le_contourne_4176() {
    let orch = orchestrateur();
    let zone_id = zone_locale(&orch).await;
    armer_l_egaliseur(&orch, zone_id);
    demarrer_une_piste_hors_pure(&orch, zone_id).await;

    enregistrer_l_egaliseur_coupe(&orch, zone_id, 8.0);
    orch.refresh_zone_eq(zone_id).await;
    let garde = preampli_garde_db(&orch).await;
    assert!(
        garde.is_some_and(|db| db < -3.0),
        "prémisse : l'égaliseur coupé garde son préampli ({garde:?})"
    );
    assert!(
        !orch.pure_contourne_un_reglage(zone_id),
        "prémisse : les réglages disent l'égaliseur désactivé"
    );

    regler_pure(&orch, zone_id, true);
    assert!(orch.refresh_zone_pure_dsp(zone_id).await);
    assert_eq!(
        verdict(&orch).await,
        OutputDspState::BypassedPure,
        "PURE vient de retirer le préampli gardé de l'égaliseur coupé ({garde:?} dB) : \
         bouclier plein, pas « rien à contourner » (#4176)"
    );

    // Une piste suivante sous PURE (chaîne vide) ne l'oublie pas…
    avec_sortie_locale(&orch, |local| {
        local.set_pure_bypass(true);
        orch.poser_ce_que_pure_contourne(zone_id, local);
        local.set_eq(orch.load_eq_processor(zone_id, 44_100, 2));
    })
    .await;
    assert_eq!(verdict(&orch).await, OutputDspState::BypassedPure);

    // … et sortir de PURE l'efface : rien n'est plus monté.
    regler_pure(&orch, zone_id, false);
    assert!(orch.refresh_zone_pure_dsp(zone_id).await);
    regler_pure(&orch, zone_id, true);
    assert!(orch.refresh_zone_pure_dsp(zone_id).await);
    assert_eq!(verdict(&orch).await, SANS_OBJET);
}

/// Volume à 50 %, égaliseur joué, volume monté à 100 %, puis « Inactif » : la
/// remontée du gain gardé s'arrête au niveau d'avant l'égaliseur (#6741), une
/// part reste gardée. « Remettre à plat » est le réglage suivant : le gain
/// gardé doit alors revenir à 0 dB, en rampe, et l'égaliseur coupé quitter la
/// chaîne (verdict « inactif »).
#[tokio::test]
async fn remettre_a_plat_rend_tout_le_niveau_garde_par_l_egaliseur_coupe_4176() {
    let orch = orchestrateur();
    let zone_id = zone_locale(&orch).await;
    armer_l_egaliseur(&orch, zone_id);
    regler_le_volume(&orch, 0.5).await;
    demarrer_une_piste_hors_pure(&orch, zone_id).await;
    avec_sortie_locale(&orch, |local| local.jouer_du_pcm_pour_test(1.0)).await;
    regler_le_volume(&orch, 1.0).await;

    enregistrer_l_egaliseur_coupe(&orch, zone_id, 8.0);
    orch.refresh_zone_eq(zone_id).await;
    avec_sortie_locale(&orch, |local| local.jouer_du_pcm_pour_test(10.0)).await;
    let reste = preampli_garde_db(&orch).await;
    assert!(
        reste.is_some_and(|db| db < -1.0),
        "prémisse (#6741) : volume monté pendant l'égaliseur, une part du gain reste \
         gardée après la coupure ({reste:?})"
    );
    assert_eq!(verdict(&orch).await, OutputDspState::Applied);

    enregistrer_l_egaliseur_coupe(&orch, zone_id, 0.0);
    orch.refresh_zone_eq(zone_id).await;
    let juste_apres = avec_sortie_locale(&orch, |local| local.preampli_garde()).await;
    assert!(
        juste_apres.as_ref().is_some_and(|g| g.en_retour),
        "« Remettre à plat » relance la remontée du gain gardé, sans saut ({juste_apres:?})"
    );
    avec_sortie_locale(&orch, |local| local.jouer_du_pcm_pour_test(10.0)).await;
    assert_eq!(
        preampli_garde_db(&orch).await,
        None,
        "après « Remettre à plat », le gain gardé ({reste:?} dB) doit être rendu en entier : \
         le niveau ne remonte qu'à moitié (#4176)"
    );
    assert_eq!(verdict(&orch).await, OutputDspState::Inactive);
}
