//! #6551 — une sortie locale disparue met sa zone hors ligne, et la remet en
//! ligne quand elle revient ; une sortie ASIO, que le rescan à chaud ne voit
//! jamais, n'est pas retirée.
//!
//! alec_eiffel (fil 2340, 1.0.0-rc6, macOS) : des AirPods Pro éloignés, et la
//! zone « AirPods Pro » restait verte. La garde `was_in_scan_scope` (19/06)
//! cherchait le nom de la sortie dans le balayage même dont elle était
//! absente : elle sautait TOUTE suppression, et la mise hors ligne de #1626 ne
//! s'exécutait jamais.
//!
//! Le banc donne à `appliquer_le_parc_local` un parc fabriqué : aucun
//! matériel n'est sondé, aucun son n'est joué.
use super::*;
use tune_core::db::zone_repo::ZoneRepo;
use tune_core::outputs::local::{AudioDevice, LocalOutput};
use tune_core::outputs::traits::OutputTarget;

const AIRPODS: &str = "AirPods Pro";
const ENCEINTES: &str = "MacBook Neo Speakers";

fn appareil(nom: &str, hote: &str) -> AudioDevice {
    AudioDevice {
        name: nom.into(),
        endpoint_id: String::new(),
        is_default: false,
        max_channels: 2,
        sample_rates: vec![44100, 48000],
        sample_rates_measured: true,
        backend: hote.into(),
        hardware_detail: None,
    }
}

fn sortie(nom: &str, backend: &str, hote: &str) -> LocalOutput {
    LocalOutput::with_options_and_endpoint(nom.into(), None, false, backend).with_origin_host(hote)
}

/// Un serveur dont le registre porte `sorties`, chacune avec sa zone EN LIGNE.
async fn banc(sorties: Vec<LocalOutput>) -> (AppState, ZoneRepo) {
    let state = AppState::new(":memory:", 0, Default::default()).expect("état");
    let repo = ZoneRepo::with_backend(state.backend.clone());
    {
        let mut outputs = state.outputs.lock().await;
        for s in sorties {
            let id = s.device_id().to_string();
            repo.create(s.name(), Some("local"), Some(&id))
                .expect("zone");
            repo.set_online_by_device(&id, true).expect("en ligne");
            outputs.register(Box::new(s));
        }
    }
    (state, repo)
}

fn en_ligne(repo: &ZoneRepo, device_id: &str) -> bool {
    repo.get_by_device_id(device_id)
        .expect("lecture")
        .expect("la zone doit être gardée")
        .online
}

async fn enregistree(state: &AppState, device_id: &str) -> bool {
    state.outputs.lock().await.contains(device_id)
}

#[test]
fn seule_une_sortie_asio_est_hors_de_portee_du_rescan_6551() {
    use PorteeDuRescanLocal::*;
    assert_eq!(
        portee_du_rescan_local(Some((Some("CoreAudio"), "auto"))),
        Balayee
    );
    assert_eq!(
        portee_du_rescan_local(Some((Some("WASAPI"), "wasapi"))),
        Balayee
    );
    assert_eq!(
        portee_du_rescan_local(Some((Some("ALSA"), "auto"))),
        Balayee
    );
    assert_eq!(portee_du_rescan_local(Some((None, "auto"))), Balayee);
    assert_eq!(
        portee_du_rescan_local(Some((Some("ASIO"), "asio"))),
        HorsDePortee
    );
    assert_eq!(
        portee_du_rescan_local(Some((Some("asio"), "auto"))),
        HorsDePortee
    );
    // Origine inconnue sous ASIO configuré : le réglage passe tel quel.
    assert_eq!(portee_du_rescan_local(Some((None, "asio"))), HorsDePortee);
    // Une sortie `local:` qui n'est pas une `LocalOutput` : on ne sait pas.
    assert_eq!(portee_du_rescan_local(None), HorsDePortee);
}

/// Cas 2 et 3 : l'appareil disparaît, puis revient.
#[tokio::test]
async fn une_sortie_disparue_met_sa_zone_hors_ligne_puis_revient_en_ligne_6551() {
    let id = format!("local:{AIRPODS}");
    let (state, repo) = banc(vec![
        sortie(AIRPODS, "auto", "CoreAudio"),
        sortie(ENCEINTES, "auto", "CoreAudio"),
    ])
    .await;
    let mut evenements = state.event_bus.subscribe();

    // Les AirPods s'éloignent : le balayage CoreAudio ne les rend plus.
    appliquer_le_parc_local(&state, &[appareil(ENCEINTES, "CoreAudio")], "auto", false).await;
    assert!(
        !enregistree(&state, &id).await,
        "« {id} » a disparu du balayage CoreAudio qui l'avait énumérée : elle \
         doit quitter le registre (#6551, fil 2340)"
    );
    assert!(
        !en_ligne(&repo, &id),
        "la zone de « {id} » est restée EN LIGNE alors que sa sortie a disparu \
         (#6551, alec_eiffel, fil 2340)"
    );
    assert!(
        en_ligne(&repo, &format!("local:{ENCEINTES}")),
        "la sortie toujours présente ne doit pas être touchée"
    );

    // Ils reviennent.
    appliquer_le_parc_local(
        &state,
        &[
            appareil(ENCEINTES, "CoreAudio"),
            appareil(AIRPODS, "CoreAudio"),
        ],
        "auto",
        false,
    )
    .await;
    assert!(
        enregistree(&state, &id).await,
        "« {id} » doit être réenregistrée à son retour"
    );
    assert!(
        en_ligne(&repo, &id),
        "la zone de « {id} » doit revenir en ligne d'elle-même avec l'appareil"
    );

    let mut hors_ligne = false;
    let mut retour = false;
    while let Ok(e) = evenements.try_recv() {
        if e.event_type == "zone.updated" && e.data["device_id"] == id.as_str() {
            match e.data["online"].as_bool() {
                Some(false) => hors_ligne = true,
                Some(true) => retour = hors_ligne,
                None => {}
            }
        }
    }
    assert!(
        hors_ligne,
        "les télécommandes doivent apprendre la mise hors ligne"
    );
    assert!(retour, "…puis le retour en ligne, dans cet ordre");
}

/// Cas 1 : la protection d'ASIO (99421206f, 2c82c4c82). Le rescan forcé en
/// WASAPI ne voit jamais les sorties ASIO : leur absence ne prouve rien, et
/// les retirer appellerait le `Drop` du pilote (`abort()`).
#[tokio::test]
async fn une_sortie_asio_absente_du_balayage_wasapi_est_gardee_6551() {
    let asio = "local:HoloAudio ASIO Driver";
    let (state, repo) = banc(vec![
        sortie("HoloAudio ASIO Driver", "asio", "ASIO"),
        // Origine inconnue, ASIO configuré : gardée elle aussi.
        LocalOutput::with_options_and_endpoint("ASIO4ALL v2".into(), None, false, "asio"),
    ])
    .await;

    appliquer_le_parc_local(&state, &[appareil("Haut-parleurs", "WASAPI")], "asio", true).await;

    for id in [asio, "local:ASIO4ALL v2"] {
        assert!(
            enregistree(&state, id).await,
            "« {id} » est une sortie ASIO, absente par construction d'un \
             balayage WASAPI : la retirer appelle le Drop du pilote ASIO"
        );
        assert!(
            en_ligne(&repo, id),
            "la zone de « {id} » doit rester en ligne"
        );
    }
}

/// Sous ASIO configuré, une sortie WASAPI (repli, rescan forcé) peut, elle,
/// disparaître — et sa zone doit revenir avec elle, alors que cette branche
/// ne crée aucune zone.
#[tokio::test]
async fn sous_asio_une_sortie_wasapi_disparue_revient_en_ligne_6551() {
    let id = "local:Speakers";
    let (state, repo) = banc(vec![
        sortie("HoloAudio ASIO Driver", "asio", "ASIO"),
        sortie("Speakers", "asio", "WASAPI"),
        sortie("Haut-parleurs", "asio", "WASAPI"),
    ])
    .await;

    appliquer_le_parc_local(&state, &[appareil("Haut-parleurs", "WASAPI")], "asio", true).await;
    assert!(!enregistree(&state, id).await);
    assert!(
        !en_ligne(&repo, id),
        "le DAC WASAPI débranché : zone hors ligne"
    );
    assert!(enregistree(&state, "local:HoloAudio ASIO Driver").await);

    appliquer_le_parc_local(
        &state,
        &[
            appareil("Haut-parleurs", "WASAPI"),
            appareil("Speakers", "WASAPI"),
        ],
        "asio",
        true,
    )
    .await;
    assert!(enregistree(&state, id).await);
    assert!(
        en_ligne(&repo, id),
        "rebranché, le DAC WASAPI doit remettre sa zone en ligne même sous ASIO"
    );
}

/// Un balayage vide veut dire « n'a pas pu énumérer » (WASAPI tenu en
/// exclusif par foobar2000), pas « tout a disparu » (2c82c4c82).
#[tokio::test]
async fn un_balayage_vide_ne_retire_rien_6551() {
    let id = format!("local:{AIRPODS}");
    let (state, repo) = banc(vec![sortie(AIRPODS, "auto", "CoreAudio")]).await;
    appliquer_le_parc_local(&state, &[], "auto", false).await;
    assert!(enregistree(&state, &id).await);
    assert!(en_ligne(&repo, &id));
}
