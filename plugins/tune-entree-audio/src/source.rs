//! Les entrées audio dans le registre commun des sources physiques (#5065,
//! étape 3).
//!
//! Le greffon y déclare CHAQUE entrée que le système énumère (la même
//! énumération que `GET /entrees`, `Peripheriques::lister`), classée
//! `entree`, `virtuelle` ou `hdmi` ([`crate::routes::genre_de_source`]), et
//! la tient à jour en tâche de fond :
//!
//! * toutes les [`PERIODE`], et juste après un « Jouer » délégué ;
//! * l'énumération passe par `spawn_blocking`, bornée par
//!   [`crate::routes::SYSTEME_MUET`] : un CoreAudio muet ne bloque ni
//!   l'exécuteur, ni les tours suivants (un seul tour à la fois) ;
//! * `sources.changed` ne part que sur un vrai changement (le registre
//!   compare) ; une entrée débranchée est retirée.
//!
//! ## États
//!
//! * l'entrée ÉCOUTÉE : ce que dit la capture (`signal`, `silence`,
//!   `autorisation_refusee`, `indisponible`) ;
//! * les autres : `disponible` — RIEN n'est capté pour les mesurer, aucune
//!   capture ne démarre sans un geste de l'utilisateur ;
//! * macOS refuse l'accès (TCC) : `autorisation_refusee`. Le statut est LU
//!   (`authorizationStatusForMediaType:`), jamais demandé ; « jamais demandé »
//!   reste `disponible` avec `detail.autorisation = "non_demandee"` — c'est le
//!   clic « Jouer » qui fera poser la question par macOS, et le client le dit.
//!
//! « Jouer » délègue à [`crate::routes::jouer_entree`], la route `/jouer`.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::sync::Notify;
use tune_core::sources_physiques::{
    DemandeJouer, EtatSource, JoueurSource, RefusSource, RegistreSources, Source,
};

use crate::autorisation::Autorisation;
use crate::controleur::Instantane;
use crate::format::dbfs;
use crate::peripheriques::DescriptionEntree;
use crate::routes::{EtatRoutes, SYSTEME_MUET, etat_de_source, genre_de_source, id_de_source};

/// Le nom du greffon, propriétaire des sources `entree:*`.
pub const GREFFON: &str = "entree-audio";

/// Le rythme du rafraîchissement de fond. Trente secondes, la borne haute
/// du besoin : sous ALSA, énumérer ouvre chaque périphérique et la
/// bibliothèque écrit sur la sortie d'erreur à chaque tour.
pub const PERIODE: Duration = Duration::from_secs(30);

/// Les sources que déclare cette liste d'entrées. Pure : c'est elle que les
/// témoins éprouvent.
pub fn sources_des_entrees(
    entrees: &[DescriptionEntree],
    active: Option<&Instantane>,
    autorisation: Autorisation,
) -> Vec<Source> {
    entrees
        .iter()
        .map(|e| {
            let ecoutee = active.filter(|i| i.entree == e.nom);
            let etat = match (ecoutee, autorisation) {
                (Some(i), _) => etat_de_source(i, autorisation),
                (None, Autorisation::Refusee) => EtatSource::AutorisationRefusee,
                (None, _) => EtatSource::Disponible,
            };
            Source {
                id: id_de_source(&e.nom),
                genre: genre_de_source(&e.nom, e.virtuelle),
                greffon: GREFFON.into(),
                nom: e.nom.clone(),
                etat,
                detail: json!({
                    "frequence": e.frequence_courante,
                    "canaux": e.canaux,
                    "niveau_db": ecoutee.and_then(|i| dbfs(i.crete)),
                    "virtuelle": e.virtuelle,
                    "autorisation": autorisation,
                }),
            }
        })
        .collect()
}

pub struct PublicationEntrees {
    pub registre: Arc<RegistreSources>,
    pub routes: EtatRoutes,
    /// Réveille la tâche de fond (après un « Jouer »).
    pub reveil: Arc<Notify>,
    en_cours: Arc<AtomicBool>,
}

impl PublicationEntrees {
    pub fn new(registre: Arc<RegistreSources>, routes: EtatRoutes) -> Arc<Self> {
        Arc::new(Self {
            registre,
            routes,
            reveil: Arc::new(Notify::new()),
            en_cours: Arc::default(),
        })
    }

    /// Inscrit les sources de `entrees` et retire celles du greffon qui n'y
    /// sont plus.
    pub fn publier(&self, entrees: &[DescriptionEntree]) {
        let active = self.routes.controleur.instantane();
        let sources = sources_des_entrees(entrees, active.as_ref(), (self.routes.autorisation)());
        let presentes: BTreeSet<String> = sources.iter().map(|s| s.id.clone()).collect();
        let joueur: Arc<dyn JoueurSource> = Arc::new(JoueurEntree {
            routes: self.routes.clone(),
            reveil: self.reveil.clone(),
        });
        for s in sources {
            if let Err(e) = self.registre.inscrire(s, Some(joueur.clone())) {
                tracing::warn!(id = %e.id, proprietaire = %e.proprietaire, "entree_audio_source_identifiant_pris");
            }
        }
        for s in self.registre.lister() {
            if s.greffon == GREFFON && !presentes.contains(&s.id) {
                self.registre.retirer(GREFFON, &s.id);
            }
        }
    }

    /// Un tour : énumère hors de l'exécuteur, puis publie. Sans effet si le
    /// tour précédent attend encore le système, s'il ne répond pas dans
    /// [`SYSTEME_MUET`], ou si la capture n'est pas compilée.
    pub async fn un_tour(&self) {
        if self.en_cours.swap(true, Ordering::SeqCst) {
            return;
        }
        let c = self.routes.controleur.clone();
        let drapeau = self.en_cours.clone();
        let tache = tokio::task::spawn_blocking(move || {
            let r = c.peripheriques().lister();
            drapeau.store(false, Ordering::SeqCst);
            r
        });
        match tokio::time::timeout(SYSTEME_MUET, tache).await {
            Ok(Ok(Ok(entrees))) => self.publier(&entrees),
            Ok(Ok(Err(e))) => tracing::debug!(error = %e, "entree_audio_enumeration_impossible"),
            Ok(Err(e)) => {
                self.en_cours.store(false, Ordering::SeqCst);
                tracing::warn!(error = %e, "entree_audio_enumeration_interrompue");
            }
            // Le fil bloqué rendra le drapeau quand le système répondra.
            Err(_) => tracing::warn!("entree_audio_enumeration_sans_reponse"),
        }
    }

    /// La tâche de fond : un tour tout de suite, puis toutes les
    /// [`PERIODE`] ou sur réveil.
    pub async fn tourner(self: Arc<Self>) {
        loop {
            self.un_tour().await;
            tokio::select! {
                _ = tokio::time::sleep(PERIODE) => {}
                _ = self.reveil.notified() => {}
            }
        }
    }
}

/// « Jouer » une source `entree:*` : `POST /jouer` du greffon.
struct JoueurEntree {
    routes: EtatRoutes,
    reveil: Arc<Notify>,
}

#[async_trait]
impl JoueurSource for JoueurEntree {
    async fn jouer(&self, id: &str, d: DemandeJouer) -> Result<Value, RefusSource> {
        let r = crate::routes::jouer_entree(&self.routes, id, d.zone_id).await;
        // L'état de la source (signal, silence…) change : republier.
        self.reveil.notify_one();
        r.map_err(|(code, motif, message)| RefusSource {
            statut: code.as_u16(),
            motif: motif.into(),
            message,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::controleur::Controleur;
    use crate::hote::tests::HoteTemoin;
    use crate::simule::Simulees;
    use tune_core::event_bus::EventBus;
    use tune_core::sources_physiques::{ErreurJouer, TypeSource};

    fn accordee() -> Autorisation {
        Autorisation::Accordee
    }
    fn refusee() -> Autorisation {
        Autorisation::Refusee
    }
    fn jamais() -> Autorisation {
        Autorisation::NonDemandee
    }

    fn changements(
        rx: &mut tokio::sync::broadcast::Receiver<tune_core::event_bus::TuneEvent>,
    ) -> usize {
        std::iter::from_fn(|| rx.try_recv().ok())
            .filter(|e| e.event_type == "sources.changed")
            .count()
    }

    fn publication(
        s: Arc<Simulees>,
        hote: Arc<HoteTemoin>,
        autorisation: crate::autorisation::LireAutorisation,
    ) -> (
        Arc<PublicationEntrees>,
        Arc<RegistreSources>,
        Arc<Controleur>,
    ) {
        let registre = Arc::new(RegistreSources::new());
        let c = Controleur::new(s, hote);
        let p = PublicationEntrees::new(
            registre.clone(),
            EtatRoutes {
                controleur: c.clone(),
                autorisation,
            },
        );
        (p, registre, c)
    }

    /// Témoin : trois entrées simulées sont inscrites, classées, `disponible`
    /// sans qu'AUCUNE capture ne démarre ; un tour sans changement n'émet
    /// rien ; une entrée débranchée disparaît.
    #[tokio::test]
    async fn les_entrees_du_systeme_sont_inscrites_classees_sans_rien_capter() {
        let s = Simulees::avec("Yeti X", 48_000);
        s.brancher("BlackHole 2ch", 44_100);
        s.brancher("USB3 HDMI Capture", 48_000);
        let (p, registre, _c) = publication(s.clone(), Arc::default(), accordee);
        let bus = Arc::new(EventBus::new());
        let mut rx = bus.subscribe();
        registre.brancher_bus(bus);

        p.un_tour().await;
        let l = registre.lister();
        let vus: Vec<(&str, TypeSource, EtatSource)> =
            l.iter().map(|s| (s.id.as_str(), s.genre, s.etat)).collect();
        assert_eq!(
            vus,
            vec![
                (
                    "entree:blackhole-2ch",
                    TypeSource::Virtuelle,
                    EtatSource::Disponible
                ),
                (
                    "entree:usb3-hdmi-capture",
                    TypeSource::Hdmi,
                    EtatSource::Disponible
                ),
                ("entree:yeti-x", TypeSource::Entree, EtatSource::Disponible),
            ]
        );
        assert!(l.iter().all(|s| s.greffon == GREFFON));
        assert_eq!(l[0].detail["frequence"], 44_100);
        assert_eq!(l[0].detail["autorisation"], "accordee");
        assert!(
            s.demarrages.lock().unwrap().is_empty(),
            "l'énumération ne doit RIEN capter"
        );
        assert!(changements(&mut rx) >= 1);

        p.un_tour().await;
        assert_eq!(changements(&mut rx), 0, "rien n'a changé");

        s.debrancher("USB3 HDMI Capture");
        p.un_tour().await;
        assert_eq!(registre.lister().len(), 2);
        assert!(registre.source("entree:usb3-hdmi-capture").is_none());
        assert_eq!(changements(&mut rx), 1);
    }

    /// macOS : un refus TCC se lit sur chaque entrée ; « jamais demandé »
    /// reste `disponible` et le dit dans le détail — rien n'est demandé.
    #[tokio::test]
    async fn l_autorisation_macos_est_lue_jamais_demandee() {
        let s = Simulees::avec("Loopback Audio", 48_000);
        let (p, registre, _c) = publication(s.clone(), Arc::default(), refusee);
        p.un_tour().await;
        let e = registre.source("entree:loopback-audio").unwrap();
        assert_eq!(e.genre, TypeSource::Virtuelle);
        assert_eq!(e.etat, EtatSource::AutorisationRefusee);

        let (p, registre, _c) = publication(s.clone(), Arc::default(), jamais);
        p.un_tour().await;
        let e = registre.source("entree:loopback-audio").unwrap();
        assert_eq!(e.etat, EtatSource::Disponible);
        assert_eq!(e.detail["autorisation"], "non_demandee");
        assert!(s.demarrages.lock().unwrap().is_empty());
    }

    /// « Jouer » par le registre délègue au `/jouer` du greffon : la zone
    /// est lancée sur l'entrée désignée par son `id` de source.
    #[tokio::test]
    async fn jouer_par_le_registre_lance_la_capture_de_l_entree() {
        let s = Simulees::avec("Yeti X", 48_000);
        s.brancher("BlackHole 2ch", 44_100);
        let hote = Arc::new(HoteTemoin::default());
        let (p, registre, c) = publication(s.clone(), hote.clone(), accordee);
        *c.amorce.lock().unwrap() = Duration::from_millis(100);
        p.un_tour().await;
        let v = registre
            .jouer(
                "entree:blackhole-2ch",
                DemandeJouer {
                    zone_id: 7,
                    piste: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(v["zone_id"], 7);
        assert_eq!(v["entree"], "BlackHole 2ch");
        assert_eq!(s.demarrages.lock().unwrap()[0].0, "BlackHole 2ch");
        c.arreter_capture();

        assert!(matches!(
            registre
                .jouer(
                    "entree:absente",
                    DemandeJouer {
                        zone_id: 7,
                        piste: None
                    }
                )
                .await,
            Err(ErreurJouer::Inconnue)
        ));
    }
}
