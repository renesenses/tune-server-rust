//! La source `cd` dans le registre commun des sources physiques (#5065).
//!
//! Le greffon y déclare son lecteur, et le tient à jour au fil de la
//! surveillance qui voit déjà l'éjection (`ejection.rs`) :
//!
//! * `disque` — un disque est inséré ; le détail reprend `/disque` (album,
//!   artiste, nombre de pistes audio, pochette) ;
//! * `vide` — le lecteur est là, sans disque ;
//! * `non_pris_en_charge` — la plateforme n'a pas d'implémentation ;
//! * absente — pas de lecteur, ou greffon arrêté (`teardown`).
//!
//! « Jouer » délègue à `POST /jouer` du greffon (`routes::jouer_disque`).

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use tune_core::sources_physiques::{
    DemandeJouer, EtatSource, JoueurSource, RefusSource, RegistreSources, Source, TypeSource,
};

use crate::discid::disc_id;
use crate::lecteur::{LecteurDisque, Presence, plateforme_prise_en_charge};
use crate::musicbrainz::Consultation;
use crate::routes::{EtatRoutes, jouer_disque};

/// L'identifiant de la source ET le nom du greffon.
pub const ID: &str = "cd";

pub struct PublicationSource {
    pub registre: Arc<RegistreSources>,
    pub lecteur: Option<Arc<dyn LecteurDisque>>,
    pub consultation: Arc<dyn Consultation>,
    pub joueur: Arc<dyn JoueurSource>,
}

impl PublicationSource {
    /// Branchée sur les routes du greffon : même lecteur, même consultation
    /// (et donc même mémoire MusicBrainz), même « jouer ».
    pub fn new(registre: Arc<RegistreSources>, routes: &EtatRoutes) -> Self {
        Self {
            registre,
            lecteur: routes.lecteur.clone(),
            consultation: routes.consultation.clone(),
            joueur: Arc::new(JoueurCd(routes.clone())),
        }
    }

    /// Sans lecteur, la surveillance ne tourne pas : la source se déclare
    /// ici, une fois. `non_pris_en_charge` si la plateforme n'a pas
    /// d'implémentation ; rien si elle en a une mais qu'aucun lecteur n'est
    /// branché (seules les sources présentes sont listées). Avec un lecteur,
    /// c'est le premier tour de la surveillance qui publie — jamais `setup`,
    /// qui ne doit pas attendre MusicBrainz au démarrage du serveur.
    pub fn publier_sans_lecteur(&self) {
        if self.lecteur.is_none() && !plateforme_prise_en_charge() {
            self.inscrire(
                EtatSource::NonPrisEnCharge,
                "Lecteur CD".into(),
                json!({}),
                false,
            );
        }
    }

    /// Met la source à jour d'après la présence vue au lecteur.
    pub async fn publier(&self, presence: Presence) {
        match presence {
            Presence::AucunLecteur => {
                self.registre.retirer(ID, ID);
            }
            Presence::Vide => self.inscrire(EtatSource::Vide, "Lecteur CD".into(), json!({}), true),
            Presence::Disque => {
                let Some(l) = self.lecteur.clone() else {
                    return;
                };
                let toc = tokio::task::spawn_blocking(move || l.lire_toc()).await;
                let Ok(Ok(toc)) = toc else {
                    // Disque vu mais TOC illisible (pas encore prêt) : le
                    // prochain tour réessaiera s'il change d'état ; d'ici là,
                    // c'est un lecteur sans disque jouable.
                    self.inscrire(EtatSource::Vide, "Lecteur CD".into(), json!({}), true);
                    return;
                };
                let infos = self.consultation.consulter(&disc_id(&toc)).await;
                let album = infos
                    .as_ref()
                    .map(|i| i.titre.clone())
                    .filter(|t| !t.is_empty());
                let artiste = infos
                    .as_ref()
                    .map(|i| i.artiste.clone())
                    .filter(|a| !a.is_empty());
                let nom = match (&artiste, &album) {
                    (Some(a), Some(t)) => format!("{a} — {t}"),
                    (None, Some(t)) => t.clone(),
                    _ => "CD audio".into(),
                };
                let detail = json!({
                    "album": album,
                    "artiste": artiste,
                    "pistes": toc.pistes_audio().count(),
                    "pochette": infos.as_ref().and_then(|i| i.pochette.clone()),
                });
                self.inscrire(EtatSource::Disque, nom, detail, true);
            }
        }
    }

    fn inscrire(&self, etat: EtatSource, nom: String, detail: Value, jouable: bool) {
        let source = Source {
            id: ID.into(),
            genre: TypeSource::Cd,
            greffon: ID.into(),
            nom,
            etat,
            detail,
        };
        let joueur = jouable.then(|| self.joueur.clone());
        if let Err(e) = self.registre.inscrire(source, joueur) {
            tracing::warn!(proprietaire = %e.proprietaire, "cd_source_identifiant_pris");
        }
    }
}

/// « Jouer » la source `cd` : `POST /jouer` du greffon.
struct JoueurCd(EtatRoutes);

#[async_trait]
impl JoueurSource for JoueurCd {
    async fn jouer(&self, _id: &str, d: DemandeJouer) -> Result<Value, RefusSource> {
        let piste = match d.piste.map(u8::try_from) {
            None => None,
            Some(Ok(p)) => Some(p),
            Some(Err(_)) => {
                return Err(RefusSource {
                    statut: 400,
                    motif: "piste_inconnue".into(),
                    message: "Numéro de piste hors d'un CD audio.".into(),
                });
            }
        };
        jouer_disque(&self.0, d.zone_id, piste)
            .await
            .map_err(|(code, motif, message)| RefusSource {
                statut: code.as_u16(),
                motif: motif.into(),
                message,
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discid::tests::toc_du_vecteur;
    use crate::ejection::Surveillant;
    use crate::ejection::tests::HoteTemoin;
    use crate::musicbrainz::InfosDisque;
    use crate::simule::LecteurSimule;
    use tune_core::event_bus::EventBus;
    use tune_core::sources_physiques::ErreurJouer;

    struct Fixture;
    #[async_trait]
    impl Consultation for Fixture {
        async fn consulter(&self, disc: &str) -> Option<InfosDisque> {
            let v: Value = serde_json::from_str(include_str!(
                "../tests/fixtures/discid_Wn8eRBtfLDfM0qjYPdxrz.Zjs_U-.json"
            ))
            .unwrap();
            crate::musicbrainz::lire_reponse(&v, disc)
        }
    }

    fn orchestrateur() -> tune_core::orchestrator::PlaybackOrchestrator {
        use tokio::sync::Mutex;
        use tune_core::db::migrations::run_migrations;
        use tune_core::db::sqlite::SqliteDb;
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        run_migrations(&db).unwrap();
        tune_core::orchestrator::PlaybackOrchestrator::new(
            Arc::new(db),
            Arc::new(tune_core::playback::PlaybackManager::new()),
            Arc::new(tune_core::http::streamer::AudioStreamer::new(0)),
            Arc::new(Mutex::new(
                tune_core::streaming::registry::ServiceRegistry::new(),
            )),
            Arc::new(Mutex::new(
                tune_core::outputs::registry::OutputRegistry::new(),
            )),
            Some("127.0.0.1".into()),
        )
    }

    fn changements(
        rx: &mut tokio::sync::broadcast::Receiver<tune_core::event_bus::TuneEvent>,
    ) -> usize {
        std::iter::from_fn(|| rx.try_recv().ok())
            .filter(|e| e.event_type == "sources.changed")
            .count()
    }

    /// Témoin : le lecteur simulé, vide → disque inséré (`disque` et le
    /// détail) → éjecté (`vide`) → greffon arrêté (absent). Un événement par
    /// vrai changement, aucun pour un tour de surveillance sans changement.
    #[tokio::test]
    async fn la_source_cd_suit_le_lecteur() {
        let bus = Arc::new(EventBus::new());
        let mut rx = bus.subscribe();
        // Le registre de l'ORCHESTRATEUR, celui que le greffon reçoit par ses
        // `HostServices` et que `teardown` vide.
        let orch = Arc::new(orchestrateur());
        let registre = orch.sources_physiques().clone();
        registre.brancher_bus(bus);
        let lecteur = Arc::new(LecteurSimule::new(toc_du_vecteur()));
        lecteur.ejecter();
        let hote = Arc::new(HoteTemoin::default());
        let routes = EtatRoutes {
            lecteur: Some(lecteur.clone()),
            hote: hote.clone(),
            consultation: Arc::new(Fixture),
            zones: Arc::default(),
        };
        let publication = Arc::new(PublicationSource::new(registre.clone(), &routes));
        let mut s = Surveillant::new(lecteur.clone(), hote.clone(), routes.zones.clone())
            .avec_publication(publication.clone());

        s.un_tour().await;
        let l = registre.lister();
        assert_eq!(l.len(), 1);
        assert_eq!(l[0].etat, EtatSource::Vide);
        assert_eq!(l[0].genre, TypeSource::Cd);
        assert_eq!(changements(&mut rx), 1);
        s.un_tour().await;
        assert_eq!(changements(&mut rx), 0, "rien n'a changé");

        lecteur.inserer();
        s.un_tour().await;
        let cd = registre.source(ID).unwrap();
        assert_eq!(cd.etat, EtatSource::Disque);
        assert_eq!(cd.nom, "Dark Tranquillity — Fiction");
        assert_eq!(cd.detail["album"], "Fiction");
        assert_eq!(cd.detail["artiste"], "Dark Tranquillity");
        assert_eq!(cd.detail["pistes"], 10);
        assert!(cd.detail.get("pochette").is_some());
        assert_eq!(changements(&mut rx), 1);

        // « Jouer » par le registre délègue au `/jouer` du greffon.
        let v = registre
            .jouer(
                ID,
                DemandeJouer {
                    zone_id: 3,
                    piste: Some(4),
                },
            )
            .await
            .unwrap();
        assert_eq!(v["zone_id"], 3);
        assert_eq!(v["piste"], 4);
        assert_eq!(hote.files.lock().await[0].2, 3);

        lecteur.ejecter();
        s.un_tour().await;
        assert_eq!(registre.source(ID).unwrap().etat, EtatSource::Vide);
        assert_eq!(changements(&mut rx), 1);
        // Lecteur vide : le greffon refuse, avec son motif.
        match registre
            .jouer(
                ID,
                DemandeJouer {
                    zone_id: 3,
                    piste: None,
                },
            )
            .await
        {
            Err(ErreurJouer::Refus(r)) => {
                assert_eq!((r.statut, r.motif.as_str()), (409, "aucun_disque"))
            }
            autre => panic!("{autre:?}"),
        }

        // Arrêt / désinstallation du greffon : `teardown` retire la source.
        let mut greffon = crate::CdPlugin::new(crate::HostServices {
            backend: orch.db.clone(),
            orchestrator: orch.clone(),
            playback: orch.playback.clone(),
        });
        tune_core::plugin_sdk::TunePlugin::teardown(&mut greffon)
            .await
            .unwrap();
        assert!(registre.lister().is_empty());
        assert_eq!(changements(&mut rx), 1);
    }

    /// #5161 — le lecteur est branché APRÈS le démarrage du greffon : tant
    /// qu'il n'est pas là, aucune source ; branché, la surveillance le voit,
    /// la source `cd` apparaît et `sources.changed` part. Débranché, elle
    /// disparaît ; rebranché, elle revient.
    #[tokio::test]
    async fn la_source_cd_apparait_quand_le_lecteur_est_branche_apres_coup() {
        use crate::lecteur::tests::SystemeFactice;
        use std::sync::atomic::Ordering;
        use std::time::Duration;

        let bus = Arc::new(EventBus::new());
        let mut rx = bus.subscribe();
        let registre = Arc::new(RegistreSources::new());
        registre.brancher_bus(bus);
        let systeme = Arc::new(SystemeFactice::default());
        let lecteur: Arc<dyn LecteurDisque> = Arc::new(systeme.lecteur(Duration::ZERO));
        let hote = Arc::new(HoteTemoin::default());
        let routes = EtatRoutes {
            lecteur: Some(lecteur.clone()),
            hote: hote.clone(),
            consultation: Arc::new(Fixture),
            zones: Arc::default(),
        };
        let publication = Arc::new(PublicationSource::new(registre.clone(), &routes));
        publication.publier_sans_lecteur();
        let mut s =
            Surveillant::new(lecteur, hote, routes.zones.clone()).avec_publication(publication);

        s.un_tour().await;
        s.un_tour().await;
        assert!(registre.lister().is_empty(), "rien de branché");
        assert_eq!(changements(&mut rx), 0);

        systeme.branche.store(true, Ordering::SeqCst);
        s.un_tour().await;
        let cd = registre.source(ID).expect("la source cd, lecteur branché");
        assert_eq!(cd.etat, EtatSource::Disque);
        assert_eq!(cd.detail["pistes"], 10);
        assert_eq!(changements(&mut rx), 1);

        systeme.branche.store(false, Ordering::SeqCst);
        systeme
            .dernier
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .debrancher();
        s.un_tour().await;
        assert!(registre.lister().is_empty(), "débranché");
        assert_eq!(changements(&mut rx), 1);

        systeme.branche.store(true, Ordering::SeqCst);
        s.un_tour().await;
        assert_eq!(registre.source(ID).unwrap().etat, EtatSource::Disque);
        assert_eq!(changements(&mut rx), 1);
    }

    /// Sans lecteur : `non_pris_en_charge` si la plateforme n'a pas
    /// d'implémentation, rien du tout sinon (seules les sources présentes).
    #[tokio::test]
    async fn sans_lecteur() {
        let registre = Arc::new(RegistreSources::new());
        let routes = EtatRoutes {
            lecteur: None,
            hote: Arc::new(HoteTemoin::default()),
            consultation: Arc::new(Fixture),
            zones: Arc::default(),
        };
        PublicationSource::new(registre.clone(), &routes).publier_sans_lecteur();
        if plateforme_prise_en_charge() {
            assert!(registre.lister().is_empty());
        } else {
            let l = registre.lister();
            assert_eq!(l[0].etat, EtatSource::NonPrisEnCharge);
            assert!(matches!(
                registre
                    .jouer(
                        ID,
                        DemandeJouer {
                            zone_id: 1,
                            piste: None
                        }
                    )
                    .await,
                Err(ErreurJouer::NonJouable { .. })
            ));
        }
    }
}
