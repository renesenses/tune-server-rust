//! Surveiller l'éjection : un disque retiré pendant la lecture arrête la zone.
//!
//! Le flux, lui, s'arrête déjà à la première lecture qui échoue sur un
//! lecteur vide (`FluxPiste`). Mais la sortie a plusieurs secondes de tampon,
//! et une zone en PAUSE ne lit rien du tout : sans surveillance, elle resterait
//! « en lecture » sur un disque qui n'existe plus. Le surveillant interroge la
//! présence du disque et, au passage de « disque » à autre chose, arrête
//! chaque zone qui joue encore la source `cd` — comme le bouton Stop.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Mutex;

use crate::fournisseur::SOURCE;
use crate::hote::HoteLecture;
use crate::lecteur::{LecteurDisque, Presence};
use crate::source::PublicationSource;

/// Zones sur lesquelles le greffon a lancé le disque.
pub type ZonesDuDisque = Arc<Mutex<HashSet<i64>>>;

pub struct Surveillant {
    pub lecteur: Arc<dyn LecteurDisque>,
    pub hote: Arc<dyn HoteLecture>,
    pub zones: ZonesDuDisque,
    derniere: Option<Presence>,
    derniere_generation: Option<u64>,
    /// #5065 — la source `cd` du registre commun, remise à jour à chaque
    /// changement de présence (insertion, éjection, lecteur débranché).
    publication: Option<Arc<PublicationSource>>,
}

impl Surveillant {
    pub fn new(
        lecteur: Arc<dyn LecteurDisque>,
        hote: Arc<dyn HoteLecture>,
        zones: ZonesDuDisque,
    ) -> Self {
        Self {
            lecteur,
            hote,
            zones,
            derniere: None,
            derniere_generation: None,
            publication: None,
        }
    }

    pub fn avec_publication(mut self, publication: Arc<PublicationSource>) -> Self {
        self.publication = Some(publication);
        self
    }

    /// Un tour de surveillance. Rend les zones arrêtées.
    pub async fn un_tour(&mut self) -> Vec<i64> {
        let lecteur = self.lecteur.clone();
        let (presence, generation) = tokio::task::spawn_blocking(move || {
            let presence = lecteur.presence();
            (presence, lecteur.generation_lecteur())
        })
        .await
        .unwrap_or((Presence::AucunLecteur, 0));
        let avant = self.derniere.replace(presence);
        let generation_avant = self.derniere_generation.replace(generation);
        let lecteur_remplace = generation_avant.is_some_and(|g| g != generation);
        let mut arretees = Vec::new();
        if avant == Some(Presence::Disque) && (presence != Presence::Disque || lecteur_remplace) {
            let zones: Vec<i64> = self.zones.lock().await.drain().collect();
            for zone_id in zones {
                if self.hote.source_en_cours(zone_id).await.as_deref() == Some(SOURCE) {
                    tracing::warn!(zone_id, "cd_ejecte_pendant_la_lecture_zone_arretee");
                    self.hote.arreter(zone_id).await;
                    arretees.push(zone_id);
                }
            }
        }
        // La publication d'un nouveau disque peut consulter MusicBrainz :
        // arrêter d'abord les zones encore liées à l'ancien lecteur.
        if (avant != Some(presence) || lecteur_remplace)
            && let Some(p) = &self.publication
        {
            p.publier(presence).await;
        }
        arretees
    }

    /// La boucle de production : une seconde entre deux tours tant qu'une
    /// zone joue le disque, trois sinon (seule l'insertion est alors à voir).
    pub async fn tourner(mut self) {
        loop {
            self.un_tour().await;
            let rythme = if self.zones.lock().await.is_empty() {
                3
            } else {
                1
            };
            tokio::time::sleep(Duration::from_secs(rythme)).await;
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::hote::ElementFile;
    use crate::lecteur::tests::SystemeFactice;
    use crate::simule::LecteurSimule;
    use async_trait::async_trait;
    use std::sync::atomic::Ordering;

    /// Un hôte qui retient ce qu'on lui demande.
    #[derive(Default)]
    pub(crate) struct HoteTemoin {
        pub files: Mutex<Vec<(i64, Vec<ElementFile>, usize)>>,
        pub arrets: Mutex<Vec<i64>>,
        pub sources: Mutex<std::collections::HashMap<i64, String>>,
    }

    #[async_trait]
    impl HoteLecture for HoteTemoin {
        async fn jouer_file(
            &self,
            zone_id: i64,
            elements: Vec<ElementFile>,
            depart: usize,
        ) -> Result<(), String> {
            self.sources.lock().await.insert(zone_id, SOURCE.into());
            self.files.lock().await.push((zone_id, elements, depart));
            Ok(())
        }
        async fn arreter(&self, zone_id: i64) {
            self.sources.lock().await.remove(&zone_id);
            self.arrets.lock().await.push(zone_id);
        }
        async fn source_en_cours(&self, zone_id: i64) -> Option<String> {
            self.sources.lock().await.get(&zone_id).cloned()
        }
    }

    /// Témoin 7 — l'éjection pendant la lecture arrête la zone, et elle
    /// seule : une zone passée à autre chose n'est pas touchée.
    #[tokio::test]
    async fn l_ejection_pendant_la_lecture_arrete_la_zone() {
        let lecteur = Arc::new(LecteurSimule::new(crate::discid::tests::toc_du_vecteur()));
        let hote = Arc::new(HoteTemoin::default());
        let zones: ZonesDuDisque = Arc::default();
        hote.sources.lock().await.insert(7, SOURCE.into());
        hote.sources.lock().await.insert(8, "radio".into());
        zones.lock().await.extend([7, 8]);
        let mut s = Surveillant::new(lecteur.clone(), hote.clone(), zones.clone());

        assert!(
            s.un_tour().await.is_empty(),
            "disque présent : rien à faire"
        );
        assert!(s.un_tour().await.is_empty());
        lecteur.ejecter();
        assert_eq!(s.un_tour().await, vec![7]);
        assert_eq!(*hote.arrets.lock().await, vec![7]);
        assert!(zones.lock().await.is_empty());
        // Un second tour sans disque n'arrête plus rien.
        assert!(s.un_tour().await.is_empty());
    }

    /// A disparaît alors que B est déjà visible : la présence reste `Disque`,
    /// mais continuer la lecture servirait désormais les secteurs de B.
    #[tokio::test]
    async fn lecteur_remplace_sans_intervalle_vide_arrete_la_lecture() {
        let systeme = Arc::new(SystemeFactice::default());
        systeme.branche.store(true, Ordering::SeqCst);
        let lecteur: Arc<dyn LecteurDisque> = Arc::new(systeme.lecteur(Duration::ZERO));
        let hote = Arc::new(HoteTemoin::default());
        let zones: ZonesDuDisque = Arc::default();
        hote.sources.lock().await.insert(7, SOURCE.into());
        zones.lock().await.insert(7);
        let mut s = Surveillant::new(lecteur.clone(), hote.clone(), zones);

        assert!(s.un_tour().await.is_empty());
        let generation_a = lecteur.generation_lecteur();
        systeme
            .dernier
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .debrancher();

        assert_eq!(s.un_tour().await, vec![7]);
        assert_eq!(lecteur.presence(), Presence::Disque);
        assert_ne!(lecteur.generation_lecteur(), generation_a);
        assert_eq!(*hote.arrets.lock().await, vec![7]);
    }
}
