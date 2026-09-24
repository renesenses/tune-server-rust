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

use std::collections::HashMap;
use std::io::Read;
use std::sync::{Arc, RwLock};

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
}
