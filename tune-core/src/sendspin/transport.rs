//! Le tuyau chiffré, une fois la poignée de main faite.
//!
//! À partir de là, plus une seule trame texte : tout passe en trames WebSocket
//! **binaires**, chacune portant un message Noise chiffré. Le premier octet du
//! déchiffré est le **type** ; `0` désigne un corps JSON en UTF-8.
//!
//! Les messages applicatifs passent par recevoir_message/chiffrer_message :
//! le cadrage Sendspin (type 1, drapeaux first/last) est distinct des trames
//! WebSocket et de la limite Noise. Voir Sendspin/spec@cd9330ef, messaging.md.
//! Le reassemblage est borne a 1 Mio ; une erreur invalide ce recepteur.

mod fragmentation;
pub use fragmentation::MAX_CORPS_MESSAGE;
use fragmentation::Reassemblage;

use snow::TransportState;

use super::ErreurSendspin;

/// Plafond d'un message Noise, tag AEAD compris.
pub const MAX_MESSAGE_NOISE: usize = 65535;

/// Taille du tag AEAD ajouté par Noise.
pub const TAILLE_TAG: usize = 16;

/// Charge utile maximale d'une trame, octet de type compris.
pub const MAX_CLAIR: usize = MAX_MESSAGE_NOISE - TAILLE_TAG;

/// Type binaire d'un corps JSON.
pub const TYPE_CORPS_JSON: u8 = 0;

/// Le transport Noise établi, côté serveur.
pub struct TransportNoise {
    etat: TransportState,
    reassemblage: Reassemblage,
}

impl std::fmt::Debug for TransportNoise {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Rien du materiel de clef ne doit passer par le journal.
        f.write_str("TransportNoise { <chiffre> }")
    }
}

impl TransportNoise {
    pub(super) fn nouveau(etat: TransportState) -> Self {
        Self {
            etat,
            reassemblage: Reassemblage::default(),
        }
    }

    /// Chiffre une charge utile déjà préfixée de son octet de type.
    pub fn chiffrer(&mut self, clair: &[u8]) -> Result<Vec<u8>, ErreurSendspin> {
        if clair.is_empty() {
            return Err(ErreurSendspin::EtatInattendu(
                "une trame binaire porte au moins son octet de type",
            ));
        }
        if clair.len() > MAX_CLAIR {
            // Primitive d'une seule trame. chiffrer_message fragmente les
            // messages applicatifs plus grands avant cet appel.
            return Err(ErreurSendspin::EtatInattendu(
                "charge utile au-dela d'une trame : utiliser chiffrer_message",
            ));
        }
        let mut sortie = vec![0u8; clair.len() + TAILLE_TAG];
        let n = self
            .etat
            .write_message(clair, &mut sortie)
            .map_err(|e| ErreurSendspin::Noise(format!("chiffrement : {e}")))?;
        sortie.truncate(n);
        Ok(sortie)
    }

    /// Déchiffre une trame et rend `(type, corps)`.
    pub fn dechiffrer(&mut self, chiffre: &[u8]) -> Result<(u8, Vec<u8>), ErreurSendspin> {
        let mut sortie = vec![0u8; MAX_MESSAGE_NOISE];
        let n = self
            .etat
            .read_message(chiffre, &mut sortie)
            .map_err(|e| ErreurSendspin::Noise(format!("dechiffrement : {e}")))?;
        sortie.truncate(n);
        let Some((type_message, corps)) = sortie.split_first() else {
            return Err(ErreurSendspin::MessageIllisible(
                "trame binaire vide apres dechiffrement".into(),
            ));
        };
        Ok((*type_message, corps.to_vec()))
    }

    /// Chiffre un message applicatif entier, sans entrelacer ses fragments.
    pub fn chiffrer_message(
        &mut self,
        typ: u8,
        corps: &[u8],
    ) -> Result<Vec<Vec<u8>>, ErreurSendspin> {
        fragmentation::decouper(typ, corps)?
            .into_iter()
            .map(|clair| self.chiffrer(&clair))
            .collect()
    }

    /// Authentifie chaque trame avant de reassembler. Aucun corps partiel
    /// n'est livre. Apres une erreur, l'appelant doit fermer la connexion.
    pub fn recevoir_message(
        &mut self,
        chiffre: &[u8],
    ) -> Result<Option<(u8, Vec<u8>)>, ErreurSendspin> {
        let trame = self.dechiffrer(chiffre);
        match trame {
            Ok((typ, corps)) => self.reassemblage.accepter(typ, &corps),
            Err(e) => {
                self.reassemblage.invalider();
                Err(e)
            }
        }
    }

    pub fn recevoir_json(&mut self, chiffre: &[u8]) -> Result<Option<String>, ErreurSendspin> {
        let Some((typ, corps)) = self.recevoir_message(chiffre)? else {
            return Ok(None);
        };
        if typ != TYPE_CORPS_JSON {
            self.reassemblage.invalider();
            return Err(ErreurSendspin::MessageIllisible(format!(
                "type binaire {typ} au lieu de JSON"
            )));
        }
        String::from_utf8(corps).map(Some).map_err(|e| {
            self.reassemblage.invalider();
            ErreurSendspin::MessageIllisible(format!("corps non UTF-8 : {e}"))
        })
    }

    /// Chiffre un corps JSON (type `0`).
    pub fn chiffrer_json(&mut self, texte: &str) -> Result<Vec<u8>, ErreurSendspin> {
        let mut clair = Vec::with_capacity(texte.len() + 1);
        clair.push(TYPE_CORPS_JSON);
        clair.extend_from_slice(texte.as_bytes());
        self.chiffrer(&clair)
    }

    /// Déchiffre une trame dont on attend un corps JSON.
    ///
    /// Un type non nul est refusé **en le nommant** : à ce stade de la
    /// séquence, une trame de rôle (`4` = audio) serait le signe d'un pair qui
    /// parle plus vite que nous ne savons écouter, et l'avaler comme du JSON
    /// donnerait une erreur d'UTF-8 sans rapport avec la cause.
    pub fn dechiffrer_json(&mut self, chiffre: &[u8]) -> Result<String, ErreurSendspin> {
        let (type_message, corps) = self.dechiffrer(chiffre)?;
        if type_message != TYPE_CORPS_JSON {
            return Err(ErreurSendspin::MessageIllisible(format!(
                "type binaire {type_message} recu la ou un corps JSON (0) etait attendu"
            )));
        }
        String::from_utf8(corps)
            .map_err(|e| ErreurSendspin::MessageIllisible(format!("corps non UTF-8 : {e}")))
    }
}
