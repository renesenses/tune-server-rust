//! #9091 — une annonce mDNS expirée ne retire pas une sortie OAAT vivante.
//!
//! Journal du testeur (rc12, serveur sous Docker, endpoint sur un Raspberry
//! Pi HifiBerry) :
//!
//! ```text
//! 12:08:26 WARN oaat: paquet envoye en retard … paquets=205707
//! 12:11:10 INFO tune_core::discovery::mdns: mdns_device_lost id=oaat:9c3766c5-… name=HifiBerry DAC+ / DAC+ Pro
//! 12:11:10 INFO tune_server::discovery_setup: mdns_output_removed_zone_offline id=oaat:9c3766c5-…
//! 12:11:10 INFO tune_core::outputs::oaat::output: oaat: stop device=HifiBerry DAC+ / DAC+ Pro
//! ```
//!
//! `ServiceRemoved` part aussi quand le cache mDNS EXPIRE, pas seulement sur
//! un « goodbye ». Multicast filtré (pont Docker, commutateur à IGMP
//! snooping sans querier) ou endpoint qui ne réannonce pas : la sortie était
//! retirée et ARRÊTÉE en pleine lecture, alors que le Pi recevait le flux et
//! répondait sur son port de contrôle.
//!
//! Une sortie OAAT perdue en mDNS reste donc inscrite, et sa zone en ligne,
//! tant que :
//! - une session est vivante (`OaatOutput::session_active`) ;
//! - ou l'endpoint accepte une connexion TCP sur son port de contrôle (sonde
//!   bornée par [`Reglages::delai_sonde`]).
//!
//! La décision est réévaluée toutes les [`Reglages::periode`] ; la sortie
//! n'est retirée, et la zone mise hors ligne, qu'après une sonde en échec
//! sans session. Une nouvelle annonce mDNS lève la garde
//! ([`GardesOaat::annonce_retrouvee`]) : la découverte reprend alors la main
//! et conserve la sortie qui joue (`enregistrer_la_sortie_mdns_et_sa_zone`).
//! Même logique que la tolérance DLNA/OpenHome de la perte SSDP : une
//! annonce qui manque n'est pas une preuve d'absence.
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::sync::Mutex;
use tracing::{debug, info};
use tune_core::db::backend::DbBackend;
use tune_core::event_bus::EventBus;
use tune_core::event_types::EventType;
use tune_core::outputs::{OutputRegistry, OutputTarget};

type Sortie = Arc<Mutex<Box<dyn OutputTarget>>>;

/// Bornes de la garde. Les essais les raccourcissent.
#[derive(Debug, Clone, Copy)]
pub(super) struct Reglages {
    /// Durée maximale de la connexion TCP vers le port de contrôle.
    pub delai_sonde: Duration,
    /// Attente du verrou de la sortie pour lire son état.
    pub delai_verrou: Duration,
    /// Intervalle entre deux réévaluations d'une sortie gardée.
    pub periode: Duration,
}

impl Reglages {
    pub(super) const PRODUCTION: Self = Self {
        delai_sonde: Duration::from_secs(2),
        delai_verrou: Duration::from_secs(1),
        periode: Duration::from_secs(30),
    };
}

/// Ce que la garde décide à chaque passage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Decision {
    /// Une session OAAT est vivante : rien ne prouve l'absence.
    GarderSessionActive,
    /// L'endpoint accepte une connexion sur son port de contrôle.
    GarderEndpointJoignable,
    /// La sortie est verrouillée (lecture qui démarre, arrêt en cours) :
    /// on ne tranche pas sans la lire, on repasse plus tard.
    GarderSortieOccupee,
    /// Ni session, ni réponse : vraie absence.
    Retirer,
}

impl Decision {
    pub(super) fn raison(self) -> &'static str {
        match self {
            Decision::GarderSessionActive => "session_active",
            Decision::GarderEndpointJoignable => "endpoint_joignable",
            Decision::GarderSortieOccupee => "sortie_occupee",
            Decision::Retirer => "sonde_en_echec",
        }
    }
}

/// L'état de la sortie lu sous son verrou, sans réseau.
#[derive(Debug, Clone)]
pub(super) struct EtatDeSortie {
    pub session_active: bool,
    pub adresse: Option<(String, u16)>,
}

/// Les sorties sous garde : `device_id` → génération. Une génération par
/// garde ouverte, pour qu'une garde levée puis rouverte (annonce retrouvée,
/// puis perdue de nouveau) ne laisse pas deux boucles sur la même sortie.
#[derive(Debug, Clone, Default)]
pub(super) struct GardesOaat {
    gardes: Arc<std::sync::Mutex<HashMap<String, u64>>>,
    compteur: Arc<AtomicU64>,
}

impl GardesOaat {
    /// Ouvre une garde ; `None` si la sortie est déjà gardée.
    fn ouvrir(&self, id: &str) -> Option<u64> {
        let mut g = self.gardes.lock().unwrap_or_else(|e| e.into_inner());
        if g.contains_key(id) {
            return None;
        }
        let generation = self.compteur.fetch_add(1, Ordering::SeqCst);
        g.insert(id.to_string(), generation);
        Some(generation)
    }

    fn est_courante(&self, id: &str, generation: u64) -> bool {
        self.gardes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(id)
            .is_some_and(|g| *g == generation)
    }

    fn fermer(&self, id: &str, generation: u64) {
        let mut g = self.gardes.lock().unwrap_or_else(|e| e.into_inner());
        if g.get(id).is_some_and(|x| *x == generation) {
            g.remove(id);
        }
    }

    /// L'appareil s'est réannoncé : la garde est levée, la découverte
    /// reprend la main. Rend vrai si une garde était ouverte.
    pub(super) fn annonce_retrouvee(&self, id: &str) -> bool {
        self.gardes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(id)
            .is_some()
    }

    #[cfg(test)]
    pub(super) fn est_gardee(&self, id: &str) -> bool {
        self.gardes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(id)
    }
}

/// Ce dont la garde a besoin, cloné depuis l'état du serveur.
#[derive(Clone)]
pub(super) struct ContextePerte {
    pub outputs: Arc<Mutex<OutputRegistry>>,
    pub db: Arc<dyn DbBackend>,
    pub event_bus: Arc<EventBus>,
    pub gardes: GardesOaat,
    pub reglages: Reglages,
}

/// Connexion TCP vers le port de contrôle, bornée. Aucun octet n'est envoyé :
/// la connexion acceptée suffit à prouver que l'endpoint est là.
pub(super) async fn sonder_le_port_de_controle(hote: &str, port: u16, delai: Duration) -> bool {
    matches!(
        tokio::time::timeout(delai, tokio::net::TcpStream::connect((hote, port))).await,
        Ok(Ok(_))
    )
}

/// La décision, depuis l'état lu de la sortie. La session passe AVANT la
/// sonde : une lecture vivante n'a pas à attendre une connexion.
pub(super) async fn decider(etat: &EtatDeSortie, reglages: Reglages) -> Decision {
    if etat.session_active {
        return Decision::GarderSessionActive;
    }
    match &etat.adresse {
        Some((hote, port))
            if sonder_le_port_de_controle(hote, *port, reglages.delai_sonde).await =>
        {
            Decision::GarderEndpointJoignable
        }
        _ => Decision::Retirer,
    }
}

/// `None` si le verrou de la sortie n'a pas été obtenu à temps.
async fn lire_l_etat(sortie: &Sortie, delai: Duration) -> Option<EtatDeSortie> {
    let garde = tokio::time::timeout(delai, sortie.lock()).await.ok()?;
    let oaat = garde
        .as_any()
        .downcast_ref::<tune_core::outputs::oaat::OaatOutput>();
    Some(EtatDeSortie {
        session_active: oaat.is_some_and(|o| o.session_active()),
        adresse: oaat.map(|o| {
            let (hote, port) = o.adresse_de_controle();
            (hote.to_string(), port)
        }),
    })
}

/// Point d'entrée de `MdnsEvent::DeviceLost`. Rend vrai si la perte est prise
/// en charge ici (sortie OAAT inscrite) ; faux sinon, et l'appelant applique
/// le retrait ordinaire.
pub(super) async fn prendre_en_charge(ctx: &ContextePerte, id: &str) -> bool {
    let sortie = {
        let reg = ctx.outputs.lock().await;
        if reg.type_of(id).as_deref() != Some("oaat") {
            return false;
        }
        match reg.get(id) {
            Some(s) => s,
            None => return false,
        }
    };
    let Some(generation) = ctx.gardes.ouvrir(id) else {
        debug!(id = %id, "mdns_oaat_perte_deja_sous_garde");
        return true;
    };
    let ctx = ctx.clone();
    let id = id.to_string();
    tokio::spawn(async move { garder(ctx, id, sortie, generation).await });
    true
}

async fn garder(ctx: ContextePerte, id: String, sortie: Sortie, generation: u64) {
    let mut premier_passage = true;
    loop {
        if !ctx.gardes.est_courante(&id, generation) {
            debug!(id = %id, "mdns_oaat_garde_levee_annonce_retrouvee");
            return;
        }
        let toujours_inscrite = ctx
            .outputs
            .lock()
            .await
            .get(&id)
            .is_some_and(|actuelle| Arc::ptr_eq(&actuelle, &sortie));
        if !toujours_inscrite {
            debug!(id = %id, "mdns_oaat_garde_levee_sortie_remplacee");
            ctx.gardes.fermer(&id, generation);
            return;
        }
        let etat = lire_l_etat(&sortie, ctx.reglages.delai_verrou).await;
        let decision = match &etat {
            Some(etat) => decider(etat, ctx.reglages).await,
            None => Decision::GarderSortieOccupee,
        };
        let (hote, port) = etat
            .and_then(|e| e.adresse)
            .map_or((String::new(), 0), |(h, p)| (h, p));
        if decision == Decision::Retirer {
            retirer(&ctx, &id, &sortie, generation, &hote, port).await;
            return;
        }
        if premier_passage {
            info!(
                id = %id,
                host = %hote,
                port,
                raison = decision.raison(),
                prochaine_sonde_s = ctx.reglages.periode.as_secs(),
                "mdns_oaat_perdu_zone_conservee"
            );
        } else {
            debug!(id = %id, raison = decision.raison(), "mdns_oaat_toujours_present_zone_conservee");
        }
        premier_passage = false;
        tokio::time::sleep(ctx.reglages.periode).await;
    }
}

/// Le retrait ordinaire, sous le verrou du registre tenu jusqu'à la mise
/// hors ligne : une annonce retrouvée entre-temps ne peut pas être écrasée.
async fn retirer(
    ctx: &ContextePerte,
    id: &str,
    sortie: &Sortie,
    generation: u64,
    hote: &str,
    port: u16,
) {
    let mut reg = ctx.outputs.lock().await;
    let meme = reg.get(id).is_some_and(|a| Arc::ptr_eq(&a, sortie));
    if !meme || !ctx.gardes.est_courante(id, generation) {
        debug!(id = %id, "mdns_oaat_retrait_abandonne_sortie_retrouvee");
        ctx.gardes.fermer(id, generation);
        return;
    }
    // #8029 — retirée, la sortie est aussi arrêtée.
    tune_core::outputs::registry::arreter_les_sorties_retirees_en_tache(reg.remove(id));
    super::set_zone_online(&ctx.event_bus, &ctx.db, id, false);
    ctx.gardes.fermer(id, generation);
    drop(reg);
    ctx.event_bus.emit_typed(
        EventType::DeviceLost,
        serde_json::json!({ "device_id": id }),
    );
    info!(
        id = %id,
        host = %hote,
        port,
        raison = Decision::Retirer.raison(),
        "mdns_output_removed_zone_offline"
    );
}

#[cfg(test)]
mod tests {
    use super::super::*;
    use super::*;
    use tune_core::db::zone_repo::ZoneRepo;
    use tune_core::discovery::device::{DiscoveredDevice, OutputType};

    const ID: &str = "oaat:9c3766c5-0000-4000-8000-000000009091";
    const NOM: &str = "HifiBerry DAC+ / DAC+ Pro";

    fn reglages_rapides() -> Reglages {
        Reglages {
            delai_sonde: Duration::from_millis(500),
            delai_verrou: Duration::from_millis(500),
            periode: Duration::from_millis(100),
        }
    }

    fn contexte(state: &AppState) -> ContextePerte {
        ContextePerte {
            outputs: state.outputs.clone(),
            db: state.backend.clone(),
            event_bus: state.event_bus.clone(),
            gardes: GardesOaat::default(),
            reglages: reglages_rapides(),
        }
    }

    /// L'endpoint annoncé, inscrit et sa zone créée, par la fonction même
    /// de la boucle de découverte.
    async fn annoncer(state: &AppState, port: u16) -> Sortie {
        let dev = DiscoveredDevice::new(
            ID.into(),
            NOM.into(),
            OutputType::Oaat,
            "127.0.0.1".into(),
            port,
        );
        let sortie = Box::new(tune_core::outputs::oaat::OaatOutput::new(
            dev.name.clone(),
            dev.host.clone(),
            dev.port,
            dev.id.clone(),
        ));
        enregistrer_la_sortie_mdns_et_sa_zone(
            &state.outputs,
            &state.backend,
            &state.event_bus,
            &state.playback,
            &dev,
            sortie,
            "oaat",
            false,
            AnnonceAppareil::Decouverte,
        )
        .await;
        assert!(zone_en_ligne(state), "la zone doit naître en ligne");
        state.outputs.lock().await.get(ID).expect("sortie inscrite")
    }

    fn zone_en_ligne(state: &AppState) -> bool {
        ZoneRepo::with_backend(state.backend.clone())
            .get_by_device_id(ID)
            .unwrap()
            .expect("zone de l'endpoint")
            .online
    }

    async fn meme_sortie_inscrite(state: &AppState, sortie: &Sortie) -> bool {
        state
            .outputs
            .lock()
            .await
            .get(ID)
            .is_some_and(|a| Arc::ptr_eq(&a, sortie))
    }

    async fn attendre_hors_ligne(state: &AppState, max: Duration) -> bool {
        let debut = std::time::Instant::now();
        while debut.elapsed() < max {
            if !zone_en_ligne(state) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        false
    }

    /// Un port local qui n'écoute plus : connexion refusée.
    async fn port_ferme() -> u16 {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        l.local_addr().unwrap().port()
    }

    /// Le cas du testeur : l'annonce mDNS expire, l'endpoint répond toujours
    /// sur son port de contrôle. Avant le correctif, la sortie était retirée,
    /// arrêtée, et la zone mise hors ligne.
    #[tokio::test]
    async fn perte_9091_endpoint_joignable_zone_et_sortie_conservees() {
        let state = AppState::new(":memory:", 0, Default::default()).unwrap();
        let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = ecoute.local_addr().unwrap().port();
        let sortie = annoncer(&state, port).await;
        let ctx = contexte(&state);

        traiter_la_perte_mdns(&ctx, ID.to_string()).await;

        assert!(
            !attendre_hors_ligne(&state, Duration::from_millis(600)).await,
            "#9091 : l'endpoint répond sur son port de contrôle, la zone doit rester en ligne"
        );
        assert!(
            meme_sortie_inscrite(&state, &sortie).await,
            "#9091 : la sortie qui joue ne doit être ni retirée ni remplacée"
        );
        assert!(ctx.gardes.est_gardee(ID), "la garde réévalue plus tard");
        drop(ecoute);
    }

    /// Vraie absence : ni session, ni réponse sur le port de contrôle. Le
    /// retrait ordinaire s'applique.
    #[tokio::test]
    async fn perte_9091_endpoint_absent_zone_hors_ligne_et_sortie_retiree() {
        let state = AppState::new(":memory:", 0, Default::default()).unwrap();
        let port = port_ferme().await;
        let _sortie = annoncer(&state, port).await;
        let ctx = contexte(&state);

        traiter_la_perte_mdns(&ctx, ID.to_string()).await;

        assert!(
            attendre_hors_ligne(&state, Duration::from_secs(3)).await,
            "endpoint absent : la zone passe hors ligne"
        );
        assert!(
            !state.outputs.lock().await.contains(ID),
            "endpoint absent : la sortie est retirée"
        );
        assert!(!ctx.gardes.est_gardee(ID));
    }

    /// La garde réessaie : l'endpoint d'abord joignable, puis vraiment
    /// parti. La zone tombe au passage suivant, sans nouvelle annonce.
    #[tokio::test]
    async fn perte_9091_endpoint_parti_ensuite_retire_au_passage_suivant() {
        let state = AppState::new(":memory:", 0, Default::default()).unwrap();
        let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = ecoute.local_addr().unwrap().port();
        let _sortie = annoncer(&state, port).await;
        let ctx = contexte(&state);

        traiter_la_perte_mdns(&ctx, ID.to_string()).await;
        assert!(!attendre_hors_ligne(&state, Duration::from_millis(300)).await);

        drop(ecoute);
        assert!(
            attendre_hors_ligne(&state, Duration::from_secs(3)).await,
            "sonde en échec au passage suivant : la zone passe hors ligne"
        );
        assert!(!state.outputs.lock().await.contains(ID));
    }

    /// L'appareil se réannonce : la garde est levée, plus rien ne sonde ni
    /// ne retire derrière la découverte.
    #[tokio::test]
    async fn perte_9091_annonce_retrouvee_leve_la_garde() {
        let state = AppState::new(":memory:", 0, Default::default()).unwrap();
        let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = ecoute.local_addr().unwrap().port();
        let sortie = annoncer(&state, port).await;
        let ctx = contexte(&state);

        traiter_la_perte_mdns(&ctx, ID.to_string()).await;
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(ctx.gardes.annonce_retrouvee(ID));
        drop(ecoute);

        assert!(
            !attendre_hors_ligne(&state, Duration::from_millis(500)).await,
            "garde levée par l'annonce : elle ne retire plus rien"
        );
        assert!(meme_sortie_inscrite(&state, &sortie).await);
    }

    /// Une session vivante suffit, même sans réponse à la sonde : la lecture
    /// n'est jamais coupée sur la seule foi de mDNS.
    #[tokio::test]
    async fn perte_9091_session_active_garde_sans_sonde() {
        let etat = EtatDeSortie {
            session_active: true,
            adresse: Some(("127.0.0.1".into(), port_ferme().await)),
        };
        assert_eq!(
            decider(&etat, reglages_rapides()).await,
            Decision::GarderSessionActive
        );
        let sans_session = EtatDeSortie {
            session_active: false,
            ..etat
        };
        assert_eq!(
            decider(&sans_session, reglages_rapides()).await,
            Decision::Retirer
        );
    }

    /// Les autres sorties mDNS gardent le retrait immédiat.
    #[tokio::test]
    async fn perte_9091_sortie_non_oaat_non_prise_en_charge() {
        let state = AppState::new(":memory:", 0, Default::default()).unwrap();
        let ctx = contexte(&state);
        assert!(!prendre_en_charge(&ctx, "airplay-192.168.1.62-7000").await);
    }
}
