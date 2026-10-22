//! Surveiller l'éjection : un disque retiré pendant la lecture arrête la zone.
//!
//! Le flux, lui, s'arrête déjà à la première lecture qui échoue sur un
//! lecteur vide (`FluxPiste`). Mais la sortie a plusieurs secondes de tampon,
//! et une zone en PAUSE ne lit rien du tout : sans surveillance, elle resterait
//! « en lecture » sur un disque qui n'existe plus. Le surveillant interroge la
//! présence du disque et, au passage de « disque » à autre chose, arrête
//! chaque zone qui joue encore la source `cd` — comme le bouton Stop.
//!
//! #6410 — sauf quand TOUT le disque est en mémoire (#6043, `Fin::Termine`) :
//! comme sur Daphile, la lecture continue alors depuis la mémoire. Le
//! surveillant rend cette mémoire quand plus aucune zone ne joue le disque
//! (fin de la file, bouton Stop, autre source) depuis
//! [`TOURS_AVANT_LIBERATION`] tours ; un autre disque joué la rend aussi
//! (`MemoireCd::chargement`).

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Mutex, Notify};

use crate::fournisseur::SOURCE;
use crate::hote::HoteLecture;
use crate::lecteur::{LecteurDisque, Presence};
use crate::memoire::MemoireCd;
use crate::source::PublicationSource;

/// Zones sur lesquelles le greffon a lancé le disque.
pub type ZonesDuDisque = Arc<Mutex<HashSet<i64>>>;

/// #6410 — tours consécutifs sans aucune zone qui joue le disque gardé en
/// mémoire avant de la rendre. Le passage d'une piste à la suivante ne doit
/// pas la faire tomber : quelques secondes de marge.
pub const TOURS_AVANT_LIBERATION: u32 = 5;

pub struct Surveillant {
    pub lecteur: Arc<dyn LecteurDisque>,
    pub hote: Arc<dyn HoteLecture>,
    pub zones: ZonesDuDisque,
    derniere: Option<Presence>,
    derniere_generation: Option<u64>,
    /// #5065 — la source `cd` du registre commun, remise à jour à chaque
    /// changement de présence (insertion, éjection, lecteur débranché).
    publication: Option<Arc<PublicationSource>>,
    /// Fil 2135 — l'éjection commandée (`POST /ejecter`) réveille la boucle :
    /// la source passe à `vide` sans attendre la fin de l'intervalle.
    reveil: Arc<Notify>,
    /// #6043 — le disque chargé en mémoire, libéré quand le disque part.
    memoire: Option<Arc<MemoireCd>>,
    /// #6410 — tours d'affilée sans zone qui joue le disque gardé.
    tours_sans_lecture: u32,
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
            reveil: Arc::default(),
            memoire: None,
            tours_sans_lecture: 0,
        }
    }

    pub fn avec_memoire(mut self, memoire: Arc<MemoireCd>) -> Self {
        self.memoire = Some(memoire);
        self
    }

    pub fn avec_reveil(mut self, reveil: Arc<Notify>) -> Self {
        self.reveil = reveil;
        self
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
        let parti =
            avant == Some(Presence::Disque) && (presence != Presence::Disque || lecteur_remplace);
        // #6043 — le disque est parti (ou le lecteur a changé) : rendre la
        // mémoire du disque chargé, et arrêter un chargement en cours.
        // #6410 — sauf si le disque est ENTIER en mémoire et seulement
        // éjecté : la lecture continue depuis la mémoire.
        let mut garde = false;
        if parti && let Some(m) = self.memoire.clone() {
            garde = tokio::task::spawn_blocking(move || {
                if lecteur_remplace {
                    m.liberer();
                    false
                } else {
                    m.garder_apres_ejection()
                }
            })
            .await
            .unwrap_or(false);
        }
        if parti && garde {
            let suivies: Vec<i64> = self.zones.lock().await.iter().copied().collect();
            tracing::info!(zones = ?suivies, "cd_ejecte_lecture_continue_depuis_la_memoire");
            self.tours_sans_lecture = 0;
        } else if !parti {
            self.rendre_la_memoire_gardee_en_fin_de_lecture().await;
        }
        if parti && !garde {
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
            // #6596 — un disque qui ARRIVE (pas celui déjà là au premier
            // tour) est annoncé : le client propose de le charger. Rien
            // d'autre : ni file, ni lecture.
            let insertion = presence == Presence::Disque
                && (lecteur_remplace || avant.is_some_and(|a| a != Presence::Disque));
            p.publier(presence, insertion).await;
        }
        arretees
    }

    /// #6410 — le disque gardé en mémoire après son éjection : le rendre
    /// quand plus aucune zone ne le joue (fin de la file, Stop, autre source)
    /// depuis [`TOURS_AVANT_LIBERATION`] tours.
    async fn rendre_la_memoire_gardee_en_fin_de_lecture(&mut self) {
        let Some(m) = self.memoire.clone() else {
            return;
        };
        if m.disque_garde().is_none() {
            self.tours_sans_lecture = 0;
            return;
        }
        let suivies: Vec<i64> = self.zones.lock().await.iter().copied().collect();
        let mut lisent = Vec::new();
        for z in suivies {
            if self.hote.lit_le_disque(z).await {
                lisent.push(z);
            }
        }
        if !lisent.is_empty() {
            self.tours_sans_lecture = 0;
            return;
        }
        self.tours_sans_lecture += 1;
        if self.tours_sans_lecture < TOURS_AVANT_LIBERATION {
            return;
        }
        self.tours_sans_lecture = 0;
        tracing::info!("cd_memoire_gardee_liberee_fin_de_lecture");
        let _ = tokio::task::spawn_blocking(move || m.liberer()).await;
        // Plus aucune zone ne joue ce disque : elles ne sont plus suivies.
        self.zones.lock().await.clear();
    }

    /// La boucle de production : une seconde entre deux tours tant qu'une
    /// zone joue le disque, trois sinon (seule l'insertion est alors à voir),
    /// ou aussitôt réveillée par une éjection commandée.
    pub async fn tourner(mut self) {
        loop {
            self.un_tour().await;
            let rythme = if self.zones.lock().await.is_empty() {
                3
            } else {
                1
            };
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(rythme)) => {}
                _ = self.reveil.notified() => {}
            }
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

    // ─── #6410 : disque entier en mémoire, l'éjection ne coupe plus ────────

    use crate::discid::disc_id;
    use crate::fournisseur::{FournisseurCd, source_id};
    use crate::memoire::{Fin, Reglages};
    use crate::simule::contenu_des_secteurs;
    use crate::toc::{PisteToc, Toc};
    use std::io::Read;
    use tune_core::source_pcm::FournisseurPcm;

    /// Trois pistes : 300, 800 et 400 secteurs — de quoi charger en entier.
    fn petite_toc() -> Toc {
        let p = |numero, debut| PisteToc {
            numero,
            debut,
            audio: true,
        };
        Toc::nouvelle(vec![p(1, 0), p(2, 300), p(3, 1_100)], 1_500).unwrap()
    }

    struct Banc {
        lecteur: Arc<LecteurSimule>,
        hote: Arc<HoteTemoin>,
        zones: ZonesDuDisque,
        memoire: Arc<MemoireCd>,
        surveillant: Surveillant,
        fournisseur: FournisseurCd,
    }

    /// La zone 7 joue le disque ; `charge` : la fin du chargement en mémoire.
    async fn banc(ejecter_apres: Option<u32>) -> (Banc, Option<Fin>) {
        let lecteur = Arc::new(LecteurSimule::new(petite_toc()));
        let hote = Arc::new(HoteTemoin::default());
        let zones: ZonesDuDisque = Arc::default();
        hote.sources.lock().await.insert(7, SOURCE.into());
        zones.lock().await.insert(7);
        let memoire = Arc::new(MemoireCd::new(Reglages {
            actif: true,
            plafond_octets: u64::MAX,
        }));
        let mut surveillant = Surveillant::new(lecteur.clone(), hote.clone(), zones.clone())
            .avec_memoire(memoire.clone());
        assert!(surveillant.un_tour().await.is_empty(), "disque présent");
        if let Some(n) = ejecter_apres {
            lecteur.ejecter_apres(n);
        }
        let l: Arc<dyn LecteurDisque> = lecteur.clone();
        let fin = memoire
            .chargement(&l, &petite_toc(), &disc_id(&petite_toc()), 0, 0)
            .map(|c| c.attendre_la_fin());
        let fournisseur = FournisseurCd {
            lecteur: lecteur.clone(),
            memoire: Some(memoire.clone()),
        };
        (
            Banc {
                lecteur,
                hote,
                zones,
                memoire,
                surveillant,
                fournisseur,
            },
            fin.flatten(),
        )
    }

    fn lire_piste(f: &FournisseurCd, piste: u8) -> Result<Vec<u8>, String> {
        let mut flux = f.ouvrir(&source_id(&disc_id(&petite_toc()), piste), 0)?;
        let mut v = Vec::new();
        flux.lecteur
            .read_to_end(&mut v)
            .map_err(|e| e.to_string())?;
        Ok(v)
    }

    /// Le disque est entier en mémoire : l'éjection n'arrête pas la zone, la
    /// mémoire est gardée, et la piste suivante s'ouvre sans le lecteur.
    #[tokio::test]
    async fn disque_entier_en_memoire_l_ejection_ne_coupe_pas_la_lecture() {
        let (mut b, fin) = banc(None).await;
        assert_eq!(fin, Some(Fin::Termine));
        b.lecteur.ejecter();
        assert!(
            b.surveillant.un_tour().await.is_empty(),
            "aucune zone arrêtée"
        );
        assert!(b.hote.arrets.lock().await.is_empty());
        assert!(b.zones.lock().await.contains(&7), "la zone reste suivie");
        assert!(b.memoire.disque_garde().is_some(), "mémoire gardée");

        let appels = b.lecteur.appels();
        assert_eq!(
            lire_piste(&b.fournisseur, 2).unwrap(),
            contenu_des_secteurs(300, 800),
            "la piste suivante sort de la mémoire, au bit près"
        );
        assert_eq!(b.lecteur.appels(), appels, "le lecteur vide a été relu");

        // Tant que la zone joue, la mémoire reste.
        for _ in 0..TOURS_AVANT_LIBERATION * 2 {
            assert!(b.surveillant.un_tour().await.is_empty());
        }
        assert!(b.memoire.disque_garde().is_some());
        b.memoire.liberer();
    }

    /// La lecture finit (fin de la file, ou bouton Stop) : la mémoire gardée
    /// est rendue après `TOURS_AVANT_LIBERATION` tours, pas avant.
    #[tokio::test]
    async fn la_memoire_gardee_est_rendue_a_la_fin_de_la_lecture() {
        let (mut b, _) = banc(None).await;
        b.lecteur.ejecter();
        b.surveillant.un_tour().await;
        // Arrêt explicite (ou fin de la file : la zone passe à `Stopped`).
        b.hote.arreter(7).await;
        for _ in 1..TOURS_AVANT_LIBERATION {
            b.surveillant.un_tour().await;
            assert!(
                b.memoire.disque_garde().is_some(),
                "rendue trop tôt : un changement de piste la ferait tomber"
            );
        }
        b.surveillant.un_tour().await;
        assert!(b.memoire.disque_garde().is_none());
        assert!(b.memoire.progression().is_none(), "mémoire rendue");
        assert!(b.zones.lock().await.is_empty());
        assert!(
            lire_piste(&b.fournisseur, 1).is_err(),
            "plus de disque, plus de mémoire : la piste ne s'ouvre pas"
        );
    }

    /// Un autre disque joué rend la mémoire du disque gardé.
    #[tokio::test]
    async fn un_autre_disque_rend_la_memoire_gardee() {
        let (mut b, _) = banc(None).await;
        b.lecteur.ejecter();
        b.surveillant.un_tour().await;
        let ancien = b.memoire.disque_garde().unwrap();
        let l: Arc<dyn LecteurDisque> = b.lecteur.clone();
        let autre = b
            .memoire
            .chargement(&l, &petite_toc(), "autre", 0, 0)
            .unwrap();
        assert_eq!(ancien.progression().fin, Some(Fin::Libere));
        assert_eq!(ancien.progression().octets_ram, 0, "mémoire rendue");
        assert!(b.memoire.disque_garde().is_none());
        autre.attendre_la_fin();
        b.memoire.liberer();
    }

    /// Le disque n'est PAS entier en mémoire (éjecté pendant le chargement) :
    /// le comportement de #6043 reste — mémoire rendue, zone arrêtée.
    #[tokio::test]
    async fn disque_charge_en_partie_l_ejection_arrete_comme_avant() {
        let (mut b, fin) = banc(Some(3)).await;
        assert_eq!(fin, Some(Fin::Ejecte));
        assert_eq!(b.surveillant.un_tour().await, vec![7]);
        assert_eq!(*b.hote.arrets.lock().await, vec![7]);
        assert!(b.memoire.progression().is_none());
        assert!(b.zones.lock().await.is_empty());
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
