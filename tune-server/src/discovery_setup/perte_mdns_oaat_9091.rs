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
//! - une session est vivante (`OaatOutput::session_active`) : AUCUNE sonde
//!   n'est alors envoyée ;
//! - ou, sans session, l'endpoint accepte une connexion TCP sur son port de
//!   contrôle (sonde bornée par [`Reglages::delai_sonde`]).
//!
//! Pourquoi pas de sonde en session : l'endpoint OAAT ne sert qu'UNE
//! connexion de contrôle et n'appelle pas `accept()` pendant une session
//! (`oaat-endpoint/src/transport.rs`). Chaque sonde resterait dans sa file
//! d'attente ; la file pleine (~129 sondes, ~65 min), les SYN sont jetés, la
//! sonde finit en délai dépassé et la garde couperait une lecture saine.
//!
//! LIMITE CONNUE (rc13) : un endpoint débranché en pleine session n'est pas
//! détecté par cette garde. L'audio part en UDP (l'envoi réussit vers un hôte
//! mort) et la perte du TCP de contrôle n'est que journalisée par
//! `oaat-controller` : la zone reste « en lecture » (ou « en pause ») jusqu'au
//! Stop, puis la garde sonde et retire au premier échec. Le correctif propre
//! (indicateur « contrôle perdu » exposé par `oaat-controller`, ou sonde sans
//! connexion TCP) est prévu en rc14.
//!
//! La décision est réévaluée toutes les [`Reglages::periode`].
//! Une nouvelle annonce mDNS lève la garde
//! ([`GardesOaat::annonce_retrouvee`]) : la découverte reprend alors la main
//! et conserve la sortie qui joue (`enregistrer_la_sortie_mdns_et_sa_zone`).
//! Même logique que la tolérance DLNA/OpenHome de la perte SSDP : une
//! annonce qui manque n'est pas une preuve d'absence.
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::sync::Mutex;
use tracing::{debug, info, warn};
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

/// Premier passage « sortie occupée » de suite qui alerte ; ensuite une
/// alerte tous les [`ALERTE_OCCUPEE_TOUS_LES`] passages.
pub(super) const PREMIERE_ALERTE_OCCUPEE: u32 = 4;
pub(super) const ALERTE_OCCUPEE_TOUS_LES: u32 = 10;

/// M2 — faut-il alerter au `n`-ième passage « occupée » de suite ? Au 4e,
/// puis au 14e, 24e… : pas à chaque passage.
pub(super) fn alerter_sortie_occupee(n: u32) -> bool {
    n >= PREMIERE_ALERTE_OCCUPEE && (n - PREMIERE_ALERTE_OCCUPEE) % ALERTE_OCCUPEE_TOUS_LES == 0
}

/// Ce que la garde décide à chaque passage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Decision {
    /// Une session OAAT est vivante : la zone est gardée SANS sonde (voir
    /// la limite connue en tête du module).
    GarderSessionActive,
    /// L'endpoint accepte une connexion sur son port de contrôle.
    GarderEndpointJoignable,
    /// La sortie est verrouillée (lecture qui démarre, arrêt en cours) :
    /// on ne tranche pas sans la lire, on repasse plus tard.
    GarderSortieOccupee,
    /// Sans session, sonde en échec : vraie absence.
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

/// Issue d'une sonde du port de contrôle. La cause d'un échec est
/// journalisée : « refusée » (hôte présent, port fermé), « délai dépassé »
/// (hôte muet ou parti), ou l'erreur du système.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Sonde {
    Reussie,
    Refusee,
    DelaiDepasse,
    Erreur(String),
}

impl Sonde {
    pub(super) fn cause(&self) -> String {
        match self {
            Sonde::Reussie => "reussie".into(),
            Sonde::Refusee => "connexion_refusee".into(),
            Sonde::DelaiDepasse => "delai_depasse".into(),
            Sonde::Erreur(e) => format!("erreur: {e}"),
        }
    }
}

/// Connexion TCP vers le port de contrôle, bornée. Aucun octet n'est envoyé :
/// la connexion acceptée suffit à prouver que l'endpoint est là.
pub(super) async fn sonder_le_port_de_controle(hote: &str, port: u16, delai: Duration) -> Sonde {
    match tokio::time::timeout(delai, tokio::net::TcpStream::connect((hote, port))).await {
        Ok(Ok(_)) => Sonde::Reussie,
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::ConnectionRefused => Sonde::Refusee,
        Ok(Err(e)) => Sonde::Erreur(e.to_string()),
        Err(_) => Sonde::DelaiDepasse,
    }
}

/// Compteurs d'une garde, d'un passage à l'autre.
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct Compteurs {
    /// Sondes en échec de suite ; une sonde réussie le remet à zéro.
    pub echecs_de_sonde: u32,
    /// Passages de suite où la sortie était verrouillée.
    pub occupee: u32,
}

/// Un passage de la garde : la décision et la sonde qui l'a fondée.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Passage {
    pub decision: Decision,
    pub sonde: Option<Sonde>,
}

/// Un passage, depuis l'état lu de la sortie (`None` : verrou non obtenu).
/// En session, AUCUNE sonde : l'endpoint ne lit pas sa file de connexions
/// de contrôle pendant une session (voir la tête du module). Sans session,
/// une sonde ; le premier échec retire.
pub(super) async fn passer(
    etat: Option<&EtatDeSortie>,
    compteurs: &mut Compteurs,
    reglages: Reglages,
) -> Passage {
    let Some(etat) = etat else {
        compteurs.occupee += 1;
        return Passage {
            decision: Decision::GarderSortieOccupee,
            sonde: None,
        };
    };
    compteurs.occupee = 0;
    if etat.session_active {
        compteurs.echecs_de_sonde = 0;
        return Passage {
            decision: Decision::GarderSessionActive,
            sonde: None,
        };
    }
    let sonde = match &etat.adresse {
        Some((hote, port)) => sonder_le_port_de_controle(hote, *port, reglages.delai_sonde).await,
        None => Sonde::Erreur("adresse de contrôle inconnue".into()),
    };
    if sonde == Sonde::Reussie {
        compteurs.echecs_de_sonde = 0;
        return Passage {
            decision: Decision::GarderEndpointJoignable,
            sonde: Some(sonde),
        };
    }
    compteurs.echecs_de_sonde += 1;
    Passage {
        decision: Decision::Retirer,
        sonde: Some(sonde),
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
    let mut compteurs = Compteurs::default();
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
        let passage = passer(etat.as_ref(), &mut compteurs, ctx.reglages).await;
        let decision = passage.decision;
        let session_active = etat.as_ref().is_some_and(|e| e.session_active);
        let (hote, port) = etat
            .and_then(|e| e.adresse)
            .map_or((String::new(), 0), |(h, p)| (h, p));
        let cause = passage.sonde.as_ref().map(Sonde::cause);
        if let Some(cause) = cause.as_deref()
            && passage.sonde != Some(Sonde::Reussie)
        {
            info!(
                id = %id,
                host = %hote,
                port,
                cause,
                echecs = compteurs.echecs_de_sonde,
                session_active,
                "mdns_oaat_sonde_en_echec"
            );
        }
        if decision == Decision::GarderSortieOccupee && alerter_sortie_occupee(compteurs.occupee)
        {
            warn!(
                id = %id,
                passages = compteurs.occupee,
                "mdns_oaat_sortie_occupee_prolongee"
            );
        }
        if decision == Decision::Retirer {
            retirer(
                &ctx,
                &id,
                &sortie,
                generation,
                &hote,
                port,
                cause.as_deref().unwrap_or(""),
                compteurs.echecs_de_sonde,
            )
            .await;
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
#[allow(clippy::too_many_arguments)]
async fn retirer(
    ctx: &ContextePerte,
    id: &str,
    sortie: &Sortie,
    generation: u64,
    hote: &str,
    port: u16,
    cause: &str,
    echecs: u32,
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
        cause,
        echecs,
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

    fn etat_sur(port: u16, session_active: bool) -> EtatDeSortie {
        EtatDeSortie {
            session_active,
            adresse: Some(("127.0.0.1".into(), port)),
        }
    }

    /// D1 — en session, aucune sonde : l'endpoint ne lit pas sa file de
    /// connexions de contrôle pendant une session. Le port ÉCOUTE mais
    /// n'accepte jamais ; après de nombreux passages, la zone est toujours
    /// gardée et AUCUNE connexion n'attend dans la file.
    #[tokio::test]
    async fn perte_9091_session_active_aucune_sonde_sur_de_nombreux_passages() {
        let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let etat = etat_sur(ecoute.local_addr().unwrap().port(), true);
        let mut c = Compteurs::default();
        for n in 1..=200 {
            let p = passer(Some(&etat), &mut c, reglages_rapides()).await;
            assert_eq!(p.decision, Decision::GarderSessionActive, "passage {n}");
            assert_eq!(p.sonde, None, "passage {n} : aucune sonde en session");
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(200), ecoute.accept())
                .await
                .is_err(),
            "D1 : une sonde en session remplirait la file de l'endpoint"
        );
    }

    /// Même en session avec un port fermé, la zone reste gardée : la session
    /// suffit (limite connue M1, correctif en rc14).
    #[tokio::test]
    async fn perte_9091_session_active_port_ferme_toujours_gardee() {
        let etat = etat_sur(port_ferme().await, true);
        let mut c = Compteurs::default();
        for n in 1..=10 {
            let p = passer(Some(&etat), &mut c, reglages_rapides()).await;
            assert_eq!(p.decision, Decision::GarderSessionActive, "passage {n}");
        }
        assert_eq!(c.echecs_de_sonde, 0);
    }

    /// Sans session, le premier échec retire.
    #[tokio::test]
    async fn perte_9091_sans_session_premier_echec_retire() {
        let etat = etat_sur(port_ferme().await, false);
        let mut c = Compteurs::default();
        let p = passer(Some(&etat), &mut c, reglages_rapides()).await;
        assert_eq!(p.decision, Decision::Retirer);
        assert_eq!(p.sonde, Some(Sonde::Refusee));
    }

    /// Sans session, une sonde réussie garde la zone.
    #[tokio::test]
    async fn perte_9091_sans_session_sonde_reussie_garde() {
        let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let etat = etat_sur(ecoute.local_addr().unwrap().port(), false);
        let mut c = Compteurs::default();
        let p = passer(Some(&etat), &mut c, reglages_rapides()).await;
        assert_eq!(p.decision, Decision::GarderEndpointJoignable);
        assert_eq!(p.sonde, Some(Sonde::Reussie));
    }

    /// M2 — la sortie verrouillée est comptée de suite ; une lecture
    /// réussie remet le compte à zéro.
    #[tokio::test]
    async fn perte_9091_sortie_occupee_comptee_de_suite() {
        let mut c = Compteurs::default();
        for n in 1..=4 {
            let p = passer(None, &mut c, reglages_rapides()).await;
            assert_eq!(p.decision, Decision::GarderSortieOccupee);
            assert_eq!(c.occupee, n);
        }
        let alertes: Vec<u32> = (1..=40).filter(|n| alerter_sortie_occupee(*n)).collect();
        assert_eq!(alertes, vec![4, 14, 24, 34], "au 4e passage, puis tous les 10");
        let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let etat = etat_sur(ecoute.local_addr().unwrap().port(), false);
        passer(Some(&etat), &mut c, reglages_rapides()).await;
        assert_eq!(c.occupee, 0);
    }

    /// M3 — la cause d'un délai dépassé est distinguée d'un refus.
    #[test]
    fn perte_9091_causes_de_sonde_journalisees() {
        assert_eq!(Sonde::Refusee.cause(), "connexion_refusee");
        assert_eq!(Sonde::DelaiDepasse.cause(), "delai_depasse");
        assert!(Sonde::Erreur("x".into()).cause().contains('x'));
    }

    /// Les autres sorties mDNS gardent le retrait immédiat.
    #[tokio::test]
    async fn perte_9091_sortie_non_oaat_non_prise_en_charge() {
        let state = AppState::new(":memory:", 0, Default::default()).unwrap();
        let ctx = contexte(&state);
        assert!(!prendre_en_charge(&ctx, "airplay-192.168.1.62-7000").await);
    }
}
