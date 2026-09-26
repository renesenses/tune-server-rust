//! Une source audio FOURNIE PAR UN GREFFON, sous forme de PCM brut (#4863).
//!
//! ## Pourquoi cette brique existe
//!
//! Jusqu'ici une source jouable était soit un fichier de la bibliothèque, soit
//! une URL (radio, podcast, serveur UPnP, service de streaming). Un greffon
//! natif qui PRODUIT lui-même l'audio — la lecture d'un CD audio, premier
//! client — n'avait aucune porte : il ne possède ni fichier ni URL, seulement
//! des octets PCM qu'il sait lire au rythme où on les lui demande.
//!
//! Cette brique est la plus petite interface qui lui permette d'entrer dans le
//! chemin de lecture EXISTANT, sans pipeline parallèle :
//!
//! * le greffon inscrit un [`FournisseurPcm`] sous le nom d'une `source`
//!   (`"cd"`), dans le registre [`SourcesPcm`] que porte l'orchestrateur ;
//! * une ligne de file dont la `source` porte ce nom se résout par
//!   `resolve_stream`, exactement comme une radio ou un podcast. Il n'y a donc
//!   rien à réécrire pour la file, l'avance, la piste suivante/précédente, le
//!   pré-armement gapless (`resolve_queue_item_url`) ni l'avance dans la piste
//!   (`replay_zone_at_position` repasse par `resolve_stream` avec `seek_ms`) ;
//! * l'orchestrateur ouvre le flux, crée une session de flux FINIE (la même
//!   que celle d'un transcodage en WAV) et la sert en `/stream/<id>.wav` : le
//!   WAV progressif que les sorties locale, OAAT et DLNA consomment déjà.
//!
//! Rien ici ne connaît le CD. Un autre greffon producteur de PCM (un tuner,
//! une entrée ligne…) passerait par la même porte.
//!
//! ## Le contrat de longueur
//!
//! [`FluxPcm::octets`] est le nombre EXACT d'octets que le lecteur rendra. Il
//! devient le `Content-Length` de la session (voir
//! `StreamInfo::wav_content_length`) : une durée en millisecondes ne tombe
//! presque jamais sur une trame entière, et une longueur arrondie tronquerait
//! la fin de la piste — donc la jonction avec la suivante.
//!
//! ## Le mode « en direct » (#5051)
//!
//! Une entrée audio (platine par préampli USB, S/PDIF, sortie optique d'une
//! TV) n'a ni longueur ni fin : elle joue tant qu'on l'écoute. Un fournisseur
//! qui répond `true` à [`FournisseurPcm::en_direct`] est ouvert par
//! [`FournisseurPcm::ouvrir_direct`] au lieu de [`FournisseurPcm::ouvrir`], et
//! l'orchestrateur le sert comme une RADIO : session sans longueur
//! (`create_radio_session`), en-tête WAV de longueur indéterminée, corps
//! découpé à la volée — ce que les zones réseau savent déjà consommer pour
//! une webradio, et ce que la sortie locale lit en continu.
//!
//! Le mode « longueur connue » ne change pas : un fournisseur qui ne dit rien
//! (le CD) garde `ouvrir`, la session finie et son `Content-Length` exact.
//!
//! Pour mesurer la dérive entre l'horloge de l'entrée et celle de la sortie,
//! l'hôte passe au fournisseur un compteur [`Consommation`] : les octets que
//! le consommateur a RÉELLEMENT tirés de la session (`bytes_sent`). Le
//! fournisseur compare ce débit à son débit capté ; la compensation qu'il
//! applique est publiée par [`EtatDirect`] pour que le chemin du signal ne
//! revendique pas un bit-perfect qu'il n'y a plus.

use std::collections::HashMap;
use std::io::Read;
use std::sync::{Arc, LazyLock, RwLock};

/// Format du PCM rendu : entiers signés petit-boutistes, canaux entrelacés.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FormatPcm {
    pub frequence: u32,
    pub canaux: u16,
    pub bits: u16,
}

impl FormatPcm {
    /// Le format du CD audio : 44,1 kHz, stéréo, 16 bits.
    pub const CD: FormatPcm = FormatPcm {
        frequence: 44_100,
        canaux: 2,
        bits: 16,
    };

    pub fn octets_par_trame(&self) -> u64 {
        self.canaux as u64 * (self.bits as u64 / 8)
    }

    pub fn octets_par_seconde(&self) -> u64 {
        self.frequence as u64 * self.octets_par_trame()
    }
}

/// Un flux ouvert par un fournisseur.
pub struct FluxPcm {
    pub format: FormatPcm,
    /// Nombre EXACT d'octets PCM que `lecteur` rendra avant sa fin normale,
    /// à partir de la position demandée.
    pub octets: u64,
    /// Durée de l'élément ENTIER (pas du reste), en millisecondes : c'est
    /// elle que la zone affiche.
    pub duree_ms: u64,
    /// La source des octets. Une erreur de lecture est une fin ANORMALE du
    /// flux (disque éjecté, périphérique disparu) : la session s'arrête là.
    pub lecteur: Box<dyn Read + Send>,
}

/// Ce qu'un greffon inscrit pour fournir une source PCM.
pub trait FournisseurPcm: Send + Sync {
    /// Ouvre l'élément `source_id` (le `source_id` de la ligne de file) à
    /// partir de `depuis_ms` millisecondes. Appelé depuis un fil bloquant :
    /// l'implémentation peut parler au matériel.
    fn ouvrir(&self, source_id: &str, depuis_ms: u64) -> Result<FluxPcm, String>;

    /// Vrai pour une source SANS FIN (#5051) : l'orchestrateur appelle alors
    /// [`Self::ouvrir_direct`] et sert un flux en direct, jamais `ouvrir`.
    fn en_direct(&self) -> bool {
        false
    }

    /// Ouvre l'élément `source_id` en direct. `consommation` rend, à tout
    /// instant, les octets que le consommateur a tirés de la session. Appelé
    /// depuis un fil bloquant.
    fn ouvrir_direct(
        &self,
        source_id: &str,
        consommation: Consommation,
    ) -> Result<FluxDirect, String> {
        let _ = consommation;
        Err(format!("« {source_id} » n'est pas une source en direct"))
    }
}

/// Un flux en direct : pas de longueur, pas de durée, pas d'avance possible.
pub struct FluxDirect {
    pub format: FormatPcm,
    /// La source des octets. `Ok(0)` est une fin NORMALE (arrêt demandé,
    /// relance sur une autre fréquence) ; une erreur est une fin ANORMALE
    /// (périphérique débranché), dite à la zone.
    pub lecteur: Box<dyn Read + Send>,
    /// Ce que la compensation de dérive fait au signal, pour le chemin du
    /// signal. `None` : le fournisseur ne transforme rien.
    pub etat: Option<Arc<dyn EtatDirect>>,
}

/// Les octets que le consommateur a tirés de la session en direct.
#[derive(Clone)]
pub struct Consommation(Arc<dyn Fn() -> u64 + Send + Sync>);

impl Consommation {
    pub fn new(f: impl Fn() -> u64 + Send + Sync + 'static) -> Self {
        Self(Arc::new(f))
    }

    pub fn octets(&self) -> u64 {
        (self.0)()
    }
}

/// Ce que la compensation de dérive fait au signal d'une source en direct.
#[derive(Debug, Clone, PartialEq)]
pub struct Compensation {
    /// `"tampon_avec_reprise"` ou `"reechantillonnage_adaptatif"`.
    pub methode: &'static str,
    /// Un rééchantillonnage est-il en cours ? Si oui, rien n'est bit-perfect.
    pub reechantillonne: bool,
    /// Trames retirées ou ajoutées pour ramener le tampon dans sa fenêtre.
    /// Zéro : le signal servi est, octet pour octet, le signal capté.
    pub reprises: u64,
    /// Dérive mesurée entre l'entrée et le consommateur, en ppm.
    pub derive_ppm: Option<f64>,
}

impl Compensation {
    /// Le signal servi est-il celui capté, à l'octet près ?
    pub fn bit_perfect(&self) -> bool {
        !self.reechantillonne && self.reprises == 0
    }
}

/// Publié par un fournisseur en direct, relu par le chemin du signal.
pub trait EtatDirect: Send + Sync {
    fn compensation(&self) -> Compensation;
}

/// Les flux en direct en cours, par `stream_id` de session. Global parce que
/// son lecteur — le chemin du signal, côté serveur HTTP — ne tient pas
/// l'orchestrateur ; inscrit et retiré par l'orchestrateur seul.
static DIRECTS: LazyLock<RwLock<HashMap<String, Arc<dyn EtatDirect>>>> =
    LazyLock::new(Default::default);

#[doc(hidden)]
pub fn inscrire_direct(stream_id: &str, etat: Arc<dyn EtatDirect>) {
    if let Ok(mut m) = DIRECTS.write() {
        m.insert(stream_id.to_string(), etat);
    }
}

#[doc(hidden)]
pub fn retirer_direct(stream_id: &str) {
    if let Ok(mut m) = DIRECTS.write() {
        m.remove(stream_id);
    }
}

/// La compensation appliquée au flux en direct `stream_id`, s'il en est un.
pub fn compensation_du_direct(stream_id: &str) -> Option<Compensation> {
    DIRECTS
        .read()
        .ok()
        .and_then(|m| m.get(stream_id).map(|e| e.compensation()))
}

/// Registre des sources PCM inscrites, par nom de `source`.
#[derive(Clone, Default)]
pub struct SourcesPcm(Arc<RwLock<HashMap<String, Arc<dyn FournisseurPcm>>>>);

impl SourcesPcm {
    pub fn inscrire(&self, source: &str, fournisseur: Arc<dyn FournisseurPcm>) {
        if let Ok(mut m) = self.0.write() {
            m.insert(source.to_string(), fournisseur);
        }
    }

    pub fn retirer(&self, source: &str) {
        if let Ok(mut m) = self.0.write() {
            m.remove(source);
        }
    }

    pub fn fournisseur(&self, source: &str) -> Option<Arc<dyn FournisseurPcm>> {
        self.0.read().ok().and_then(|m| m.get(source).cloned())
    }
}

/// Comment une pompe s'est arrêtée.
#[derive(Debug, PartialEq, Eq)]
pub enum FinDePompe {
    /// Les `octets` annoncés ont tous été remis.
    Complete,
    /// Le consommateur a lâché la session (piste passée, zone arrêtée).
    ConsommateurParti,
    /// Le lecteur a échoué ou s'est tu avant la fin annoncée.
    Interrompue { remis: u64, raison: String },
}

/// Taille d'un tronçon remis à la session : 32 Kio, l'unité que les
/// producteurs de transcodage emploient déjà (`wait_prefill_ready`).
pub const TRONCON: usize = 32 * 1024;

/// Lit `lecteur` jusqu'à `octets` et remet chaque tronçon à `remettre`.
///
/// Pure et synchrone : elle ne connaît ni la session ni tokio, ce qui permet
/// de la prouver sans serveur. `remettre` rend `false` quand plus personne
/// n'écoute. Jamais plus de `octets` octets ne sont remis : c'est le
/// `Content-Length` annoncé, le dépasser casserait la réponse HTTP.
pub fn pomper(
    lecteur: &mut dyn Read,
    octets: u64,
    mut remettre: impl FnMut(Vec<u8>) -> bool,
) -> FinDePompe {
    let mut remis: u64 = 0;
    while remis < octets {
        let voulu = (octets - remis).min(TRONCON as u64) as usize;
        let mut tampon = vec![0u8; voulu];
        let lu = match lecteur.read(&mut tampon) {
            Ok(0) => {
                return FinDePompe::Interrompue {
                    remis,
                    raison: "fin de flux avant la longueur annoncée".into(),
                };
            }
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => {
                return FinDePompe::Interrompue {
                    remis,
                    raison: e.to_string(),
                };
            }
        };
        tampon.truncate(lu);
        remis += lu as u64;
        if !remettre(tampon) {
            return FinDePompe::ConsommateurParti;
        }
    }
    FinDePompe::Complete
}

/// Lit un flux EN DIRECT et remet chaque tronçon à `remettre`, sans fin
/// annoncée (#5051).
///
/// `Ok(0)` du lecteur est la fin normale ([`FinDePompe::Complete`]) ; une
/// erreur, la fin anormale. Rien n'est jamais comblé : un lecteur qui n'a
/// rien à rendre BLOQUE — c'est lui qui connaît le rythme de la capture.
pub fn pomper_sans_fin(
    lecteur: &mut dyn Read,
    mut remettre: impl FnMut(Vec<u8>) -> bool,
) -> FinDePompe {
    let mut remis: u64 = 0;
    loop {
        let mut tampon = vec![0u8; TRONCON];
        let lu = match lecteur.read(&mut tampon) {
            Ok(0) => return FinDePompe::Complete,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => {
                return FinDePompe::Interrompue {
                    remis,
                    raison: e.to_string(),
                };
            }
        };
        tampon.truncate(lu);
        remis += lu as u64;
        if !remettre(tampon) {
            return FinDePompe::ConsommateurParti;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fournisseur;
    impl FournisseurPcm for Fournisseur {
        fn ouvrir(&self, _: &str, _: u64) -> Result<FluxPcm, String> {
            Err("jamais".into())
        }
    }

    #[test]
    fn le_registre_rend_ce_qu_on_y_inscrit_et_oublie_ce_qu_on_retire() {
        let r = SourcesPcm::default();
        assert!(r.fournisseur("cd").is_none());
        r.inscrire("cd", Arc::new(Fournisseur));
        assert!(r.fournisseur("cd").is_some());
        assert!(r.fournisseur("radio").is_none());
        r.retirer("cd");
        assert!(r.fournisseur("cd").is_none());
    }

    #[test]
    fn la_pompe_remet_exactement_les_octets_annonces_et_pas_un_de_plus() {
        let source: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
        let mut lecteur = std::io::Cursor::new(source.clone());
        let mut recu = Vec::new();
        let fin = pomper(&mut lecteur, 70_001, |t| {
            recu.extend_from_slice(&t);
            true
        });
        assert_eq!(fin, FinDePompe::Complete);
        assert_eq!(recu, source[..70_001]);
    }

    #[test]
    fn un_flux_trop_court_est_dit_interrompu() {
        let mut lecteur = std::io::Cursor::new(vec![1u8; 10]);
        let fin = pomper(&mut lecteur, 20, |_| true);
        assert!(matches!(fin, FinDePompe::Interrompue { remis: 10, .. }));
    }

    #[test]
    fn un_consommateur_parti_arrete_la_pompe() {
        let mut lecteur = std::io::Cursor::new(vec![1u8; TRONCON * 3]);
        let mut appels = 0;
        let fin = pomper(&mut lecteur, (TRONCON * 3) as u64, |_| {
            appels += 1;
            false
        });
        assert_eq!(fin, FinDePompe::ConsommateurParti);
        assert_eq!(appels, 1);
    }

    /// Un lecteur qui rend un nombre donné de tronçons, puis `fin`.
    struct Direct {
        restants: usize,
        fin: Option<std::io::ErrorKind>,
    }
    impl Read for Direct {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.restants == 0 {
                return match self.fin {
                    None => Ok(0),
                    Some(k) => Err(std::io::Error::new(k, "débranché")),
                };
            }
            self.restants -= 1;
            let n = buf.len().min(1000);
            buf[..n].fill(7);
            Ok(n)
        }
    }

    /// #5051 — le mode en direct n'a PAS de longueur : il remet tout ce que
    /// le lecteur rend, bien au-delà de n'importe quelle borne, jusqu'à la
    /// fin que le lecteur décide.
    #[test]
    fn la_pompe_sans_fin_remet_tout_jusqu_a_la_fin_du_lecteur() {
        let mut l = Direct {
            restants: 5_000,
            fin: None,
        };
        let mut recu = 0usize;
        let fin = pomper_sans_fin(&mut l, |t| {
            recu += t.len();
            true
        });
        assert_eq!(fin, FinDePompe::Complete);
        assert_eq!(recu, 5_000 * 1000);
    }

    #[test]
    fn la_pompe_sans_fin_dit_une_fin_anormale_et_un_consommateur_parti() {
        let mut l = Direct {
            restants: 3,
            fin: Some(std::io::ErrorKind::BrokenPipe),
        };
        let fin = pomper_sans_fin(&mut l, |_| true);
        assert!(matches!(fin, FinDePompe::Interrompue { remis: 3000, .. }));

        let mut l = Direct {
            restants: 10,
            fin: None,
        };
        let fin = pomper_sans_fin(&mut l, |_| false);
        assert_eq!(fin, FinDePompe::ConsommateurParti);
    }

    #[test]
    fn un_fournisseur_qui_ne_dit_rien_n_est_pas_en_direct() {
        let f = Fournisseur;
        assert!(!f.en_direct());
        let c = Consommation::new(|| 0);
        assert!(f.ouvrir_direct("x", c).is_err());
    }

    #[test]
    fn une_reprise_ou_un_reechantillonnage_retire_le_bit_perfect() {
        let mut c = Compensation {
            methode: "tampon_avec_reprise",
            reechantillonne: false,
            reprises: 0,
            derive_ppm: Some(3.0),
        };
        assert!(c.bit_perfect());
        c.reprises = 1;
        assert!(!c.bit_perfect());
        c.reprises = 0;
        c.reechantillonne = true;
        assert!(!c.bit_perfect());
    }
}
