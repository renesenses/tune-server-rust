//! #6947 — deux Raspberry Pi sous Verox, chacun avec un endpoint OAAT et un
//! récepteur AirPlay : Tune doit montrer les QUATRE sorties, chacune dans sa
//! zone et sous le nom qu'elle annonce. En rc8.1, le testeur ne voyait que les
//! deux OAAT ; Roon voyait les deux AirPlay.
//!
//! Le banc rejoue les annonces mDNS sur une vraie base (`:memory:`), par la
//! fonction même que la boucle de découverte appelle
//! (`enregistrer_la_sortie_mdns_et_sa_zone`). La forme des annonces vient du
//! journal joint au fil 2424 (rc7) :
//!
//! ```text
//! mdns_device_discovered id=airplay-bc:64:8f:xx:xx:xx name=atelier [Verox - Airplay] service="_airplay._tcp.local."
//! mdns_output_registered name=atelier [Verox - Airplay] host=[adresse locale] port=7000 type="airplay2"
//! mdns_zone_upgraded_to_higher_priority name=[nom de machine] id=oaat:verox-01ae8794-… old_id=Some("airplay-[adresse locale]-7000") old_type=Some("airplay") new_type="oaat"
//! mdns_device_discovered id=airplay-[adresse locale]-7000 name=Zone 6 service="_raop._tcp.local."
//! ```
//!
//! Ce que le journal masque est remplacé par des valeurs d'exemple : les
//! adresses (192.168.1.61 et .62), la fin de la MAC, la fin des identifiants
//! OAAT et le nom de machine que chaque endpoint OAAT annonce.

use super::*;
use tune_core::db::zone_repo::ZoneRepo;
use tune_core::discovery::device::{DiscoveredDevice, OutputType};

const HOTE_ATELIER: &str = "192.168.1.61";
const HOTE_ZONE_6: &str = "192.168.1.62";

const OAAT_ATELIER: &str = "oaat:verox-5a7e1e70-0000-4000-8000-000000006947";
const OAAT_ZONE_6: &str = "oaat:verox-01ae8794-0000-4000-8000-000000006947";
/// `_airplay._tcp` avec son TXT `deviceid` : l'identifiant en dérive.
const AIRPLAY_ATELIER: &str = "airplay-bc:64:8f:69:47:01";
/// `_raop._tcp` sans identité : l'identifiant dérive de l'adresse.
const AIRPLAY_ZONE_6: &str = "airplay-192.168.1.62-7000";

fn oaat(id: &str, nom: &str, hote: &str) -> DiscoveredDevice {
    DiscoveredDevice::new(id.into(), nom.into(), OutputType::Oaat, hote.into(), 9740)
}

fn airplay_atelier() -> DiscoveredDevice {
    let mut d = DiscoveredDevice::new(
        AIRPLAY_ATELIER.into(),
        "atelier [Verox - Airplay]".into(),
        OutputType::Airplay,
        HOTE_ATELIER.into(),
        7000,
    );
    d.airplay_version = Some("2".into());
    d.mac_address = Some("bc:64:8f:69:47:01".into());
    d.stable_id = Some("bc:64:8f:69:47:01".into());
    d
}

fn airplay_zone_6() -> DiscoveredDevice {
    DiscoveredDevice::new(
        AIRPLAY_ZONE_6.into(),
        "Zone 6".into(),
        OutputType::Airplay,
        HOTE_ZONE_6.into(),
        7000,
    )
}

fn les_deux_oaat() -> [DiscoveredDevice; 2] {
    [
        oaat(OAAT_ATELIER, "verox-atelier", HOTE_ATELIER),
        oaat(OAAT_ZONE_6, "verox-salon", HOTE_ZONE_6),
    ]
}

fn les_deux_airplay() -> [DiscoveredDevice; 2] {
    [airplay_atelier(), airplay_zone_6()]
}

/// Une annonce, traitée comme la boucle de `spawn_mdns_handler` la traite :
/// la sortie construite selon le protocole, puis la décision de zone. Le
/// démon AirPlay 2 est supposé présent, comme chez le testeur
/// (`type="airplay2"` au journal).
async fn annoncer(state: &AppState, dev: &DiscoveredDevice) {
    let (sortie, type_de_sortie): (Box<dyn tune_core::outputs::OutputTarget>, &str) =
        match dev.device_type {
            OutputType::Oaat => (
                Box::new(tune_core::outputs::oaat::OaatOutput::new(
                    dev.name.clone(),
                    dev.host.clone(),
                    dev.port,
                    dev.id.clone(),
                )),
                "oaat",
            ),
            OutputType::Airplay if dev.mac_address.is_some() => (
                Box::new(tune_core::outputs::airplay2::Airplay2Output::new(
                    dev.name.clone(),
                    dev.host.clone(),
                    dev.port,
                    dev.id.clone(),
                    dev.mac_address.clone().unwrap(),
                )),
                "airplay2",
            ),
            OutputType::Airplay => (
                Box::new(tune_core::outputs::airplay::AirplayOutput::new(
                    dev.name.clone(),
                    dev.id.clone(),
                    dev.host.clone(),
                    dev.port,
                )),
                "airplay",
            ),
            autre => panic!("protocole hors du banc : {autre}"),
        };
    enregistrer_la_sortie_mdns_et_sa_zone(
        &state.outputs,
        &state.backend,
        &state.event_bus,
        &state.playback,
        dev,
        sortie,
        type_de_sortie,
        false,
        AnnonceAppareil::Decouverte,
    )
    .await;
}

/// `(identifiant de sortie, nom, type)` de chaque zone visible, triés.
fn zones_visibles(state: &AppState) -> Vec<(String, String, String)> {
    let mut v: Vec<_> = ZoneRepo::with_backend(state.backend.clone())
        .list()
        .unwrap()
        .into_iter()
        .map(|z| {
            (
                z.output_device_id.unwrap_or_default(),
                z.name,
                z.output_type.unwrap_or_default(),
            )
        })
        .collect();
    v.sort();
    v
}

fn quatre_zones_attendues() -> Vec<(String, String, String)> {
    let mut v: Vec<(String, String, String)> = [
        (AIRPLAY_ZONE_6, "Zone 6", "airplay"),
        (AIRPLAY_ATELIER, "atelier [Verox - Airplay]", "airplay2"),
        (OAAT_ZONE_6, "verox-salon", "oaat"),
        (OAAT_ATELIER, "verox-atelier", "oaat"),
    ]
    .into_iter()
    .map(|(a, b, c)| (a.to_string(), b.to_string(), c.to_string()))
    .collect();
    v.sort();
    v
}

fn etat() -> AppState {
    AppState::new(":memory:", 0, Default::default()).unwrap()
}

/// Le cas du testeur en rc8.1 : les endpoints OAAT ont déjà leur zone quand
/// les récepteurs AirPlay s'annoncent. Chaque AirPlay était écarté comme
/// doublon de l'OAAT du même hôte (`mdns_zone_skipped_conflicting_protocol`).
#[tokio::test]
async fn deux_pi_verox_les_airplay_annonces_apres_les_oaat_ont_leur_zone() {
    let state = etat();
    for d in les_deux_oaat().iter().chain(les_deux_airplay().iter()) {
        annoncer(&state, d).await;
    }
    assert_eq!(
        zones_visibles(&state),
        quatre_zones_attendues(),
        "#6947 : les récepteurs AirPlay des deux Pi doivent avoir leur zone à côté des OAAT"
    );
}

/// L'ordre inverse : les récepteurs AirPlay ont déjà leur zone quand les
/// endpoints OAAT s'annoncent. L'OAAT reprenait la zone AirPlay du même hôte
/// (`mdns_zone_upgraded_to_higher_priority`) et en gardait le nom : d'où la
/// zone OAAT « Zone 6 » du journal, et plus aucune zone AirPlay.
#[tokio::test]
async fn deux_pi_verox_les_oaat_annonces_apres_les_airplay_ne_prennent_pas_leur_zone() {
    let state = etat();
    for d in les_deux_airplay().iter().chain(les_deux_oaat().iter()) {
        annoncer(&state, d).await;
    }
    assert_eq!(
        zones_visibles(&state),
        quatre_zones_attendues(),
        "#6947 : l'endpoint OAAT ne doit pas prendre la zone du récepteur AirPlay du même hôte"
    );
}

/// L'état que la rc7 a laissé chez le testeur : l'endpoint OAAT tient la zone
/// « Zone 6 », reprise au récepteur AirPlay. Le récepteur, qui s'annonce
/// toujours « Zone 6 », se voyait refuser la zone
/// (`mdns_zone_meme_nom_retrogradation_refusee`) et n'en avait aucune. Il doit
/// avoir la sienne, et la zone OAAT reste à l'OAAT.
#[tokio::test]
async fn la_zone_oaat_heritee_zone_6_laisse_l_airplay_du_meme_nom_avoir_la_sienne() {
    let state = etat();
    let zones = ZoneRepo::with_backend(state.backend.clone());
    let zone_oaat = zones
        .create("Zone 6", Some("oaat"), Some(OAAT_ZONE_6))
        .unwrap();
    annoncer(&state, &oaat(OAAT_ZONE_6, "verox-salon", HOTE_ZONE_6)).await;
    annoncer(&state, &airplay_zone_6()).await;

    let oaat_apres = zones.get(zone_oaat).unwrap().unwrap();
    assert_eq!(
        oaat_apres.output_device_id.as_deref(),
        Some(OAAT_ZONE_6),
        "la zone OAAT ne doit pas changer de sortie"
    );
    assert_eq!(oaat_apres.output_type.as_deref(), Some("oaat"));
    let airplay = zones
        .get_by_device_id(AIRPLAY_ZONE_6)
        .unwrap()
        .expect("#6947 : le récepteur AirPlay « Zone 6 » doit avoir sa propre zone");
    assert_eq!(airplay.name, "Zone 6");
    assert_eq!(airplay.output_type.as_deref(), Some("airplay"));
}

/// La règle ne vaut que pour la paire OAAT / AirPlay : les autres paires du
/// même hôte restent un seul appareil (Bluesound Node, téléviseur DLNA).
#[test]
fn seule_la_paire_oaat_airplay_echappe_au_dedoublonnage() {
    assert!(oaat_et_airplay_sont_deux_sorties("oaat", "airplay"));
    assert!(oaat_et_airplay_sont_deux_sorties("airplay2", "oaat"));
    assert!(oaat_et_airplay_sont_deux_sorties("OAAT", "AirPlay"));
    assert!(!oaat_et_airplay_sont_deux_sorties("bluos", "airplay"));
    assert!(!oaat_et_airplay_sont_deux_sorties("dlna", "airplay2"));
    assert!(!oaat_et_airplay_sont_deux_sorties("oaat", "dlna"));
    assert!(!oaat_et_airplay_sont_deux_sorties("airplay", "airplay2"));
    assert!(!oaat_et_airplay_sont_deux_sorties("oaat", "oaat"));
}
