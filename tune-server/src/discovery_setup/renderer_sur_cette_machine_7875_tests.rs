//! #7875 — Tune OS sur un NUC : la sortie USB de la machine apparaissait
//! comme une zone DLNA au lieu d'une sortie locale.
//!
//! Témoins :
//! - une annonce SSDP d'un renderer DLNA tiers qui tourne sur la machine Tune
//!   (adresse de la machine, LOCATION qui n'est pas une façade Tune), quand
//!   une sortie locale est inscrite, ne crée PAS de zone DLNA neuve ; sa
//!   sortie reste inscrite (zone possible à la main) ;
//! - le même renderer qui a DÉJÀ sa zone la garde et la remet en ligne ;
//! - la même annonce depuis une autre adresse du réseau reste une zone, et
//!   sans sortie locale inscrite la règle ne s'arme pas ;
//! - une carte ALSA USB simulée, à côté de `default` et du HDMI d'un NUC,
//!   reçoit la zone locale automatique (comportement existant, gardé ici).
use super::*;
use tune_core::db::zone_repo::ZoneRepo;
use tune_core::discovery::device::{DiscoveredDevice, OutputType};
use tune_core::outputs::mock::MockOutput;

/// L'adresse réseau du NUC, telle que `advertised_ip` la donne.
const NUC: &str = "192.0.2.10";
/// Une autre machine du réseau.
const VOISIN: &str = "192.0.2.60";

/// L'annonce d'un renderer UPnP tiers (upmpdcli, gmediarender…) : port
/// 49152, description à la racine, aucun chemin de façade Tune.
fn renderer_tiers(host: &str) -> DiscoveredDevice {
    let mut dev = DiscoveredDevice::new(
        format!("uuid:renderer-tiers-7875-{host}"),
        "UpMpd".into(),
        OutputType::Dlna,
        host.into(),
        49152,
    );
    dev.location = Some(format!("http://{host}:49152/description.xml"));
    dev.capabilities.insert(
        "service_urls".into(),
        serde_json::json!({"avtransport": "/av", "renderingcontrol": "/rc"}),
    );
    dev
}

fn etat_du_nuc() -> AppState {
    let config = TuneConfig {
        advertised_ip: Some(NUC.into()),
        ..Default::default()
    };
    AppState::new(":memory:", 0, config).unwrap()
}

/// Le NUC avec sa carte USB inscrite comme sortie locale.
async fn etat_du_nuc_avec_sa_carte() -> AppState {
    let state = etat_du_nuc();
    state.outputs.lock().await.register(Box::new(
        MockOutput::new("local:XMOS X20, USB Audio", "XMOS X20, USB Audio").with_type("local"),
    ));
    state
}

async fn annoncer(state: &AppState, dev: &DiscoveredDevice) {
    let mut seen = std::collections::HashSet::new();
    let listener: Option<Arc<UpnpEventListener>> = None;
    handle_ssdp_discovered(
        dev,
        &state.outputs,
        &state.backend,
        &state.config,
        &state.event_bus,
        &listener,
        &state.playback,
        &state.license,
        &mut seen,
    )
    .await;
}

fn zones_dlna(state: &AppState) -> Vec<(String, Option<String>, Option<String>)> {
    ZoneRepo::with_backend(state.backend.clone())
        .list()
        .unwrap()
        .into_iter()
        .filter(|z| z.output_type.as_deref() != Some("local"))
        .map(|z| (z.name, z.output_type, z.output_device_id))
        .collect()
}

/// Témoin 1 — l'annonce SSDP de la machine elle-même ne crée pas de zone
/// DLNA neuve en double de la sortie locale.
#[tokio::test]
async fn un_renderer_tiers_de_cette_machine_ne_devient_pas_une_zone_dlna_7875() {
    let state = etat_du_nuc_avec_sa_carte().await;
    let dev = renderer_tiers(NUC);
    annoncer(&state, &dev).await;

    let zones = zones_dlna(&state);
    assert!(
        zones.is_empty(),
        "#7875 : un renderer UPnP qui annonce l'adresse de la machine Tune est devenu \
         une zone DLNA en double de la sortie locale : {zones:?}"
    );
    assert!(
        state.outputs.lock().await.contains(&dev.id),
        "#7875 : la sortie du renderer reste inscrite, une zone doit pouvoir se créer à la main"
    );
}

/// Témoin 1 bis — un renderer de la machine qui a DÉJÀ sa zone la garde :
/// la règle ne retient que la création, jamais le rattachement.
#[tokio::test]
async fn un_renderer_de_cette_machine_qui_a_deja_sa_zone_la_garde_7875() {
    let state = etat_du_nuc_avec_sa_carte().await;
    let dev = renderer_tiers(NUC);
    let repo = ZoneRepo::with_backend(state.backend.clone());
    let zid = repo.create("UpMpd", Some("dlna"), Some(&dev.id)).unwrap();
    repo.set_online_by_device(&dev.id, false).unwrap();

    annoncer(&state, &dev).await;

    let zone = repo
        .get(zid)
        .unwrap()
        .expect("la zone existante doit rester");
    assert!(
        zone.online,
        "#7875 : la zone existante d'un renderer de cette machine doit se rattacher \
         et repasser en ligne, pas rester hors ligne"
    );
    assert!(
        state.outputs.lock().await.contains(&dev.id),
        "#7875 : la zone existante doit garder sa sortie inscrite"
    );
    assert_eq!(zones_dlna(&state).len(), 1, "ni doublon ni zone perdue");
}

/// Contre-épreuve permanente du témoin 1 : le même renderer, sur une autre
/// machine du réseau, reste une zone DLNA.
#[tokio::test]
async fn le_meme_renderer_sur_une_autre_machine_reste_une_zone_dlna_7875() {
    let state = etat_du_nuc_avec_sa_carte().await;
    let dev = renderer_tiers(VOISIN);
    annoncer(&state, &dev).await;

    let zones = zones_dlna(&state);
    assert_eq!(zones.len(), 1, "un renderer du réseau doit garder sa zone");
    assert_eq!(zones[0].1.as_deref(), Some("dlna"));
    assert!(state.outputs.lock().await.contains(&dev.id));
}

/// Sans sortie locale inscrite, il n'y a rien à doubler : le renderer de la
/// machine reçoit sa zone comme avant.
#[tokio::test]
async fn sans_sortie_locale_le_renderer_de_cette_machine_reste_une_zone_7875() {
    let state = etat_du_nuc();
    let dev = renderer_tiers(NUC);
    annoncer(&state, &dev).await;

    let zones = zones_dlna(&state);
    assert_eq!(
        zones.len(),
        1,
        "#7875 : sans sortie locale, la règle ne doit pas retenir la zone : {zones:?}"
    );
}

/// La règle seule : par l'hôte ou par la LOCATION, jamais pour une façade
/// Tune (la nôtre a son propre rideau, celle d'un autre Tune reste une zone),
/// ni pour la boucle locale d'un banc d'essai, ni pour un voisin.
#[test]
fn la_regle_reconnait_la_machine_et_rien_d_autre_7875() {
    let nous: Vec<String> = ["127.0.0.1", "localhost", NUC]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let loc = format!("http://{NUC}:49152/description.xml");

    assert_eq!(
        renderer_tiers_sur_cette_machine(NUC, Some(&loc), &nous),
        Some(loc.as_str())
    );
    // Hôte relevé autrement, LOCATION seule à porter notre adresse.
    assert_eq!(
        renderer_tiers_sur_cette_machine("", Some(&loc), &nous),
        Some(loc.as_str())
    );
    // Sans LOCATION, l'hôte suffit.
    assert_eq!(
        renderer_tiers_sur_cette_machine(NUC, None, &nous),
        Some(NUC)
    );

    let voisin = format!("http://{VOISIN}:49152/description.xml");
    assert_eq!(
        renderer_tiers_sur_cette_machine(VOISIN, Some(&voisin), &nous),
        None
    );
    let facade = format!("http://{NUC}:9999/upnp/renderer/3/description.xml");
    assert_eq!(
        renderer_tiers_sur_cette_machine(NUC, Some(&facade), &nous),
        None,
        "une façade Tune n'est pas l'affaire de cette règle"
    );
    let banc = "http://127.0.0.1:40123/description.xml";
    assert_eq!(
        renderer_tiers_sur_cette_machine("127.0.0.1", Some(banc), &nous),
        None,
        "un faux renderer en boucle locale reste une zone"
    );
}

/// La décision de zone : seulement une zone `dlna`, seulement quand une
/// sortie locale est inscrite. Un mandataire OpenHome de la machine
/// (BubbleUPnP Server, qui joue sur un autre appareil) n'est pas retenu.
#[test]
fn la_zone_n_est_retenue_que_si_elle_doublerait_la_sortie_locale_7875() {
    let nous: Vec<String> = ["127.0.0.1", "localhost", NUC]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let loc = format!("http://{NUC}:49152/description.xml");

    assert_eq!(
        zone_auto_retenue_sur_cette_machine("dlna", NUC, Some(&loc), &nous, true),
        Some(loc.as_str())
    );
    assert_eq!(
        zone_auto_retenue_sur_cette_machine("openhome", NUC, Some(&loc), &nous, true),
        None,
        "un renderer OpenHome de la machine garde sa zone automatique"
    );
    assert_eq!(
        zone_auto_retenue_sur_cette_machine("dlna", NUC, Some(&loc), &nous, false),
        None,
        "sans sortie locale inscrite, rien n'est doublé"
    );
    let voisin = format!("http://{VOISIN}:49152/description.xml");
    assert_eq!(
        zone_auto_retenue_sur_cette_machine("dlna", VOISIN, Some(&voisin), &nous, true),
        None
    );
}

/// Le registre dit s'il porte une sortie locale, par son type.
#[test]
fn le_registre_dit_s_il_porte_une_sortie_locale_7875() {
    let mut reg = OutputRegistry::new();
    assert!(!sortie_locale_inscrite(&reg));
    reg.register(Box::new(MockOutput::new("dlna:x", "X").with_type("dlna")));
    assert!(!sortie_locale_inscrite(&reg));
    reg.register(Box::new(
        MockOutput::new("local:USB", "USB").with_type("local"),
    ));
    assert!(sortie_locale_inscrite(&reg));
}

/// Témoin 2 — une carte ALSA USB simulée est proposée comme sortie locale.
///
/// Le parc est celui qu'ALSA énumère sur un NUC : le greffon `default`
/// (sortie système), le HDMI de la carte Intel, et le DAC USB, dont l'endpoint
/// se trie APRÈS celui de la carte interne : seul le rang USB le fait gagner.
/// On rejoue l'enchaînement de `register_local_outputs` : cible de la zone
/// automatique, identifiant `local:<nom>`, puis l'action de zone.
#[test]
fn une_carte_alsa_usb_simulee_recoit_la_zone_locale_7875() {
    use crate::startup::{
        LocalZoneAction, SortieCandidate, local_zone_action, sortie_de_la_zone_automatique,
    };
    let parc = [
        SortieCandidate {
            nom: "Default Audio Device",
            endpoint_id: "alsa:default",
            est_le_defaut: true,
        },
        SortieCandidate {
            nom: "HDA Intel PCH, ALC256 Analog",
            endpoint_id: "alsa:hw:CARD=PCH,DEV=0",
            est_le_defaut: false,
        },
        SortieCandidate {
            nom: "HDA Intel PCH, HDMI 0",
            endpoint_id: "alsa:hw:CARD=PCH,DEV=3",
            est_le_defaut: false,
        },
        SortieCandidate {
            nom: "XMOS X20, USB Audio",
            endpoint_id: "alsa:hw:CARD=X20,DEV=0",
            est_le_defaut: false,
        },
    ];
    let cible = sortie_de_la_zone_automatique(&parc, |_| false);
    assert_eq!(
        cible,
        Some("XMOS X20, USB Audio"),
        "#7875 : la carte USB de la machine doit recevoir la zone locale"
    );
    let zone_auto = cible.map(|nom| format!("local:{nom}"));
    let device_id = "local:XMOS X20, USB Audio".to_string();
    let est_la_cible = zone_auto.as_deref() == Some(device_id.as_str());
    assert_eq!(
        local_zone_action(false, true, est_la_cible),
        LocalZoneAction::Create,
        "la sortie USB doit donner une zone locale sur une base neuve"
    );
}
