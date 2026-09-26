//! Le contrôleur : UNE capture active au plus, la zone qui l'écoute, la
//! surveillance de sa fréquence, et l'état que `/etat` rapporte.
//!
//! ## Changement de fréquence
//!
//! Une interface S/PDIF prend la fréquence de la source : passer d'un CD
//! (44,1 kHz) à un DAT ou une TV (48 kHz) change la fréquence nominale du
//! périphérique sous la capture. Le flux déjà annoncé à la zone porte un
//! en-tête WAV à l'ancienne fréquence : continuer à le remplir serait jouer
//! tout le reste trop vite ou trop lentement. Le surveillant relit la
//! fréquence nominale chaque demi-seconde ; à un changement, il JOURNALISE,
//! redémarre la capture au nouveau format natif, et relance la lecture de la
//! zone : la zone reçoit un NOUVEAU flux, cohérent de bout en bout. L'ancien
//! est tenu en silence jusqu'à sa relève (`Fin::Relance`).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use tokio::runtime::Handle;
use tracing::{info, warn};
use tune_core::source_pcm::{Consommation, FluxDirect, FormatPcm};

use crate::anneau::{Anneau, Fin};
use crate::hote::{ElementDirect, HoteLecture};
use crate::lecteur::{LecteurCapture, Mesures, Reglages};
use crate::peripheriques::{Arret, Peripheriques};

/// Capacité de l'anneau : de quoi tenir l'amorce la plus longue et la marge
/// haute de la reprise.
pub const CAPACITE_ANNEAU_MS: u64 = 12_000;
/// Période de la surveillance de fréquence.
pub const PERIODE_SURVEILLANCE: Duration = Duration::from_millis(500);
/// Une capture que plus aucune session n'écoute s'arrête après ce délai (la
/// zone a été arrêtée autrement que par `/arreter`, ou a changé de source).
pub const CAPTURE_SANS_ECOUTE: Duration = Duration::from_secs(30);

struct Active {
    entree: String,
    format: FormatPcm,
    anneau: Arc<Anneau>,
    arret: Option<Box<dyn Arret>>,
    mesures: Arc<Mesures>,
    demarree: Instant,
    veille: Arc<AtomicBool>,
    zone_id: Option<i64>,
}

impl Active {
    fn arreter(&mut self, fin: Fin) {
        self.veille.store(false, Ordering::SeqCst);
        self.anneau.fermer(fin);
        if let Some(a) = self.arret.take() {
            a.arreter();
        }
    }
}

/// Un instantané pour `/etat`.
#[derive(Debug, Clone)]
pub struct Instantane {
    pub entree: String,
    pub format: FormatPcm,
    pub zone_id: Option<i64>,
    pub depuis_s: f64,
    pub crete: f32,
    pub silence_numerique_s: f64,
    pub debordements: u64,
    pub trames_perdues: u64,
    /// Blocs où un préambule IEC 61937 (Dolby/DTS non décodé) a été vu.
    pub blocs_iec61937: u64,
    pub reprises: u64,
    pub trames_retirees: u64,
    pub trames_comblees: u64,
    pub sous_remplissages: u64,
    pub trames_captees: u64,
    pub derives: crate::lecteur::Derives,
    pub fin: Option<Fin>,
}

pub struct Controleur {
    peripheriques: Arc<dyn Peripheriques>,
    hote: Arc<dyn HoteLecture>,
    active: Mutex<Option<Active>>,
    pub relances_de_frequence: AtomicU64,
    pub derniere_relance: Mutex<Option<(u32, u32)>>,
    runtime: Option<Handle>,
    moi: Weak<Controleur>,
    pub amorce: Mutex<Duration>,
    pub periode_surveillance: Duration,
    /// Sérialise les démarrages SANS tenir l'état.
    demarrages: Mutex<()>,
    demarrage_en_cours: Mutex<Option<(String, Instant)>>,
}

impl Controleur {
    pub fn new(peripheriques: Arc<dyn Peripheriques>, hote: Arc<dyn HoteLecture>) -> Arc<Self> {
        Arc::new_cyclic(|moi| Self {
            peripheriques,
            hote,
            active: Mutex::new(None),
            relances_de_frequence: AtomicU64::new(0),
            derniere_relance: Mutex::new(None),
            runtime: Handle::try_current().ok(),
            moi: moi.clone(),
            amorce: Mutex::new(Reglages::default().amorce),
            periode_surveillance: PERIODE_SURVEILLANCE,
            demarrages: Mutex::new(()),
            demarrage_en_cours: Mutex::new(None),
        })
    }

    pub fn peripheriques(&self) -> &Arc<dyn Peripheriques> {
        &self.peripheriques
    }

    fn verrou(&self) -> std::sync::MutexGuard<'_, Option<Active>> {
        self.active.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Démarre `entree` si ce n'est pas déjà ELLE qui tourne (la précédente
    /// est arrêtée). Rend son format servi.
    ///
    /// Le verrou de l'état n'est JAMAIS tenu pendant l'appel au système :
    /// CoreAudio peut ne pas répondre (constaté pour un serveur lancé par
    /// `launchd`), et `/etat` doit pouvoir le dire au lieu de rester muet.
    pub fn assurer_capture(&self, entree: &str) -> Result<(String, FormatPcm), String> {
        let _un_a_la_fois = self.demarrages.lock().unwrap_or_else(|e| e.into_inner());
        {
            let mut garde = self.verrou();
            if let Some(a) = garde.as_ref() {
                if (a.entree == entree) && a.anneau.fin().is_none() {
                    return Ok((a.entree.clone(), a.format));
                }
            }
            if let Some(mut a) = garde.take() {
                a.arreter(Fin::Normale);
            }
        }
        let a = self.demarrer_en_le_disant(entree, None)?;
        let r = (a.entree.clone(), a.format);
        *self.verrou() = Some(a);
        Ok(r)
    }

    /// `demarrer`, en publiant qu'un démarrage est en cours (pour `/etat`).
    fn demarrer_en_le_disant(&self, entree: &str, zone_id: Option<i64>) -> Result<Active, String> {
        *self
            .demarrage_en_cours
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some((entree.to_string(), Instant::now()));
        let r = self.demarrer(entree, zone_id);
        *self
            .demarrage_en_cours
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = None;
        r
    }

    /// Le démarrage en cours, s'il y en a un : (entrée, depuis combien).
    pub fn demarrage_en_cours(&self) -> Option<(String, Duration)> {
        self.demarrage_en_cours
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|(e, t)| (e.clone(), t.elapsed()))
    }

    fn demarrer(&self, entree: &str, zone_id: Option<i64>) -> Result<Active, String> {
        let (nom, format) = self.peripheriques.format_natif(entree)?;
        let opt = format.octets_par_trame() as usize;
        let anneau = Arc::new(Anneau::new(format.frequence, opt, CAPACITE_ANNEAU_MS));
        let capture = self.peripheriques.demarrer(entree, anneau.clone())?;
        if capture.format != format {
            // Le format a bougé entre la lecture et l'ouverture : l'anneau
            // serait au mauvais pas. On refuse plutôt que servir faux.
            capture.arret.arreter();
            return Err(format!(
                "le format de « {nom} » a changé pendant l'ouverture ({:?} → {:?})",
                format, capture.format
            ));
        }
        info!(
            entree = %capture.nom,
            frequence = format.frequence,
            bits = format.bits,
            canaux = format.canaux,
            pile = self.peripheriques.pile(),
            "entree_audio_capture_demarree"
        );
        let veille = Arc::new(AtomicBool::new(true));
        self.surveiller(
            capture.nom.clone(),
            format.frequence,
            veille.clone(),
            anneau.clone(),
        );
        Ok(Active {
            entree: capture.nom,
            format,
            anneau,
            arret: Some(capture.arret),
            mesures: Arc::default(),
            demarree: Instant::now(),
            veille,
            zone_id,
        })
    }

    fn surveiller(
        &self,
        entree: String,
        frequence: u32,
        veille: Arc<AtomicBool>,
        anneau: Arc<Anneau>,
    ) {
        let moi = self.moi.clone();
        let periode = self.periode_surveillance;
        let periph = self.peripheriques.clone();
        let _ = std::thread::Builder::new()
            .name("entree-audio-frequence".into())
            .spawn(move || {
                while veille.load(Ordering::SeqCst) {
                    std::thread::sleep(periode);
                    if !veille.load(Ordering::SeqCst) {
                        return;
                    }
                    if let Some(c) = moi.upgrade() {
                        c.relever_la_position();
                    }
                    if anneau
                        .sans_lecteur()
                        .is_some_and(|d| d >= CAPTURE_SANS_ECOUTE)
                    {
                        if let Some(c) = moi.upgrade() {
                            info!(entree = %entree, "entree_audio_capture_arretee_sans_ecoute");
                            c.arreter_capture_de(&anneau);
                        }
                        return;
                    }
                    let Some(f) = periph.frequence_courante(&entree) else {
                        continue;
                    };
                    if f != frequence {
                        if let Some(c) = moi.upgrade() {
                            c.changement_de_frequence(&entree, frequence, f);
                        }
                        return;
                    }
                }
            });
    }

    /// Le périphérique a changé de fréquence sous la capture.
    pub fn changement_de_frequence(&self, entree: &str, ancienne: u32, nouvelle: u32) {
        warn!(
            entree,
            ancienne, nouvelle, "entree_audio_changement_de_frequence — capture relancée"
        );
        let _un_a_la_fois = self.demarrages.lock().unwrap_or_else(|e| e.into_inner());
        let zone = {
            let precedente = {
                let mut garde = self.verrou();
                let Some(a) = garde.take() else {
                    return;
                };
                if a.entree != entree {
                    *garde = Some(a);
                    return;
                }
                a
            };
            let mut a = precedente;
            a.arreter(Fin::Relance);
            let zone = a.zone_id;
            match self.demarrer_en_le_disant(entree, zone) {
                Ok(n) => {
                    info!(
                        entree,
                        frequence = n.format.frequence,
                        bits = n.format.bits,
                        "entree_audio_capture_relancee"
                    );
                    *self.verrou() = Some(n);
                }
                Err(e) => {
                    warn!(entree, error = %e, "entree_audio_relance_impossible");
                    return;
                }
            }
            zone
        };
        self.relances_de_frequence.fetch_add(1, Ordering::Relaxed);
        *self
            .derniere_relance
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some((ancienne, nouvelle));
        let Some(zone_id) = zone else { return };
        let (Some(rt), Some(moi)) = (self.runtime.clone(), self.moi.upgrade()) else {
            return;
        };
        let entree = entree.to_string();
        rt.spawn(async move {
            let format = moi.instantane().map(|i| i.format);
            let Some(format) = format else { return };
            if let Err(e) = moi
                .hote
                .jouer(zone_id, ElementDirect { entree, format })
                .await
            {
                warn!(zone_id, error = %e, "entree_audio_relance_zone_echouee");
            }
        });
    }

    /// `POST /jouer` : démarre la capture et lance la zone.
    pub async fn jouer(&self, entree: &str, zone_id: i64) -> Result<(String, FormatPcm), String> {
        let (nom, format) = {
            let entree = entree.to_string();
            let moi = self.moi.upgrade().ok_or("contrôleur arrêté")?;
            tokio::task::spawn_blocking(move || moi.assurer_capture(&entree))
                .await
                .map_err(|e| e.to_string())??
        };
        if let Some(a) = self.verrou().as_mut() {
            a.zone_id = Some(zone_id);
        }
        let element = ElementDirect {
            entree: nom.clone(),
            format,
        };
        if let Err(e) = self.hote.jouer(zone_id, element).await {
            self.arreter_capture();
            return Err(e);
        }
        Ok((nom, format))
    }

    /// `POST /arreter` : arrête la zone qui écoute, puis la capture.
    pub async fn arreter(&self) -> Option<(String, Option<i64>)> {
        let (entree, zone) = {
            let g = self.verrou();
            let a = g.as_ref()?;
            (a.entree.clone(), a.zone_id)
        };
        if let Some(z) = zone {
            if self.hote.source_en_cours(z).await.as_deref() == Some(crate::fournisseur::SOURCE) {
                self.hote.arreter(z).await;
            }
        }
        self.arreter_capture();
        info!(entree = %entree, zone_id = ?zone, "entree_audio_arretee");
        Some((entree, zone))
    }

    /// Relève la position JOUÉE par la zone qui écoute (la mesure de dérive
    /// la compare au débit capté).
    fn relever_la_position(&self) {
        let (zone, mesures) = {
            let g = self.verrou();
            let Some(a) = g.as_ref() else { return };
            let Some(z) = a.zone_id else { return };
            (z, a.mesures.clone())
        };
        let Some(rt) = self.runtime.clone() else {
            return;
        };
        if let Some(p) = rt.block_on(self.hote.position_ms(zone)) {
            mesures.noter_position(p);
        }
    }

    /// Arrête la capture si c'est encore CELLE-LÀ (pas une relancée depuis).
    fn arreter_capture_de(&self, anneau: &Arc<Anneau>) {
        let mut g = self.verrou();
        if g.as_ref().is_some_and(|a| Arc::ptr_eq(&a.anneau, anneau)) {
            if let Some(mut a) = g.take() {
                a.arreter(Fin::Normale);
            }
        }
    }

    pub fn arreter_capture(&self) {
        if let Some(mut a) = self.verrou().take() {
            a.arreter(Fin::Normale);
        }
    }

    /// Appelé par l'orchestrateur (fil bloquant) : un lecteur sur la capture
    /// de `entree`, démarrée à la demande.
    pub fn ouvrir_lecteur(
        &self,
        entree: &str,
        consommation: Consommation,
    ) -> Result<FluxDirect, String> {
        self.assurer_capture(entree)?;
        let garde = self.verrou();
        let a = garde.as_ref().ok_or("aucune capture active")?;
        let reglages = Reglages {
            amorce: *self.amorce.lock().unwrap_or_else(|e| e.into_inner()),
            ..Reglages::default()
        };
        // Compteurs remis à zéro : ils décrivent la session servie.
        let mesures: Arc<Mesures> = Arc::default();
        let lecteur =
            LecteurCapture::new(a.anneau.clone(), mesures.clone(), consommation, &reglages);
        drop(garde);
        if let Some(a) = self.verrou().as_mut() {
            a.mesures = mesures.clone();
        }
        let format = self
            .instantane()
            .map(|i| i.format)
            .ok_or("capture arrêtée pendant l'ouverture")?;
        Ok(FluxDirect {
            format,
            lecteur: Box::new(lecteur),
            etat: Some(mesures),
        })
    }

    pub fn instantane(&self) -> Option<Instantane> {
        let g = self.verrou();
        let a = g.as_ref()?;
        let m = &a.mesures;
        Some(Instantane {
            entree: a.entree.clone(),
            format: a.format,
            zone_id: a.zone_id,
            depuis_s: a.demarree.elapsed().as_secs_f64(),
            crete: a.anneau.crete(),
            silence_numerique_s: a.anneau.silence_numerique().as_secs_f64(),
            debordements: a.anneau.debordements.load(Ordering::Relaxed),
            trames_perdues: a.anneau.trames_perdues.load(Ordering::Relaxed),
            blocs_iec61937: a.anneau.blocs_iec61937.load(Ordering::Relaxed),
            reprises: m.reprises.load(Ordering::Relaxed),
            trames_retirees: m.trames_retirees.load(Ordering::Relaxed),
            trames_comblees: m.trames_comblees.load(Ordering::Relaxed),
            sous_remplissages: m.sous_remplissages.load(Ordering::Relaxed),
            trames_captees: a.anneau.trames_captees(),
            derives: m.derives(),
            fin: a.anneau.fin(),
        })
    }
}

impl Drop for Controleur {
    fn drop(&mut self) {
        if let Some(mut a) = self.active.get_mut().ok().and_then(|a| a.take()) {
            a.arreter(Fin::Normale);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hote::tests::HoteTemoin;
    use crate::simule::Simulees;
    use std::io::Read;

    fn controleur(s: Arc<Simulees>, hote: Arc<HoteTemoin>) -> Arc<Controleur> {
        let c = Controleur::new(s, hote);
        *c.amorce.lock().unwrap() = Duration::from_millis(100);
        c
    }

    /// Un S/PDIF passe de 44,1 à 48 kHz : la capture repart au NOUVEAU
    /// format natif, la zone est relancée sur un nouveau flux, l'ancien
    /// lecteur tient en silence au lieu de continuer à l'ancienne fréquence.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn un_changement_de_frequence_relance_la_capture_et_la_zone() {
        let s = Simulees::avec("SPDIF", 44_100);
        let hote = Arc::new(HoteTemoin::default());
        let c = controleur(s.clone(), hote.clone());
        c.jouer("SPDIF", 4).await.unwrap();
        let mut flux = {
            let c = c.clone();
            tokio::task::spawn_blocking(move || {
                c.ouvrir_lecteur("SPDIF", Consommation::new(|| 0)).unwrap()
            })
            .await
            .unwrap()
        };
        assert_eq!(flux.format.frequence, 44_100);
        let mut buf = vec![0u8; 4096];
        let (flux_r, n) = tokio::task::spawn_blocking(move || {
            let n = flux.lecteur.read(&mut buf).unwrap();
            (flux, n)
        })
        .await
        .unwrap();
        let mut flux = flux_r;
        assert!(n > 0);

        s.regler_frequence("SPDIF", 48_000);
        let mut relancee = false;
        for _ in 0..40 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            if hote.joues.lock().await.len() == 2 {
                relancee = true;
                break;
            }
        }
        assert!(relancee, "la zone n'a pas été relancée");
        assert_eq!(c.relances_de_frequence.load(Ordering::Relaxed), 1);
        assert_eq!(*c.derniere_relance.lock().unwrap(), Some((44_100, 48_000)));
        let joues = hote.joues.lock().await.clone();
        assert_eq!(joues[1].0, 4);
        assert_eq!(joues[1].1.format.frequence, 48_000);
        assert_eq!(c.instantane().unwrap().format.frequence, 48_000);
        assert_eq!(
            s.demarrages.lock().unwrap().clone(),
            vec![("SPDIF".into(), 44_100), ("SPDIF".into(), 48_000)]
        );
        // L'ancien lecteur : du silence, jamais des échantillons à 48 kHz
        // sous un en-tête à 44,1 kHz.
        let v = tokio::task::spawn_blocking(move || {
            let mut b = vec![0u8; 4096];
            let mut tout = Vec::new();
            for _ in 0..5 {
                let n = flux.lecteur.read(&mut b).unwrap();
                tout.extend_from_slice(&b[..n]);
            }
            tout
        })
        .await
        .unwrap();
        assert!(!v.is_empty());
        assert!(
            v.iter().all(|&b| b == 0),
            "l'ancien flux n'est pas tenu en silence"
        );
        c.arreter_capture();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn une_frequence_stable_ne_relance_rien() {
        let s = Simulees::avec("Yeti X", 48_000);
        let hote = Arc::new(HoteTemoin::default());
        let c = controleur(s.clone(), hote.clone());
        c.jouer("Yeti X", 1).await.unwrap();
        tokio::time::sleep(Duration::from_millis(1_300)).await;
        assert_eq!(c.relances_de_frequence.load(Ordering::Relaxed), 0);
        assert_eq!(hote.joues.lock().await.len(), 1);
        c.arreter_capture();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn un_nouveau_lecteur_remplace_l_ancien_sur_la_meme_capture() {
        let s = Simulees::avec("Yeti X", 48_000);
        let c = controleur(s.clone(), Arc::new(HoteTemoin::default()));
        let c2 = c.clone();
        let (mut a, mut b) = tokio::task::spawn_blocking(move || {
            let a = c2
                .ouvrir_lecteur("Yeti X", Consommation::new(|| 0))
                .unwrap();
            let b = c2
                .ouvrir_lecteur("Yeti X", Consommation::new(|| 0))
                .unwrap();
            (a, b)
        })
        .await
        .unwrap();
        let (na, nb) = tokio::task::spawn_blocking(move || {
            let mut buf = vec![0u8; 4096];
            let na = a.lecteur.read(&mut buf).unwrap();
            let nb = b.lecteur.read(&mut buf).unwrap();
            (na, nb)
        })
        .await
        .unwrap();
        assert_eq!(na, 0, "l'ancien lecteur s'efface");
        assert!(nb > 0);
        assert_eq!(s.demarrages.lock().unwrap().len(), 1, "une seule capture");
        c.arreter_capture();
    }
}
