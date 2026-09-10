//! Le tuyau chiffré, une fois la poignée de main faite.
//!
//! À partir de là, plus une seule trame texte : tout passe en trames WebSocket
//! **binaires**, chacune portant un message Noise chiffré. Le premier octet du
//! déchiffré est le **type** ; `0` désigne un corps JSON en UTF-8.
//!
//! ## Ce que ce module ne fait pas
//!
//! La **fragmentation** au-delà d'une trame n'est pas ici. Elle n'a de raison
//! d'être que pour l'audio et les pochettes, c'est-à-dire S2-c ; et le codage
//! des types de fragment a changé sous nous entre la note de lecture de la
//! phase 1 et l'état courant des implémentations de référence (voir le rapport
//! de S2-a). L'écrire maintenant serait l'écrire contre une cible mouvante,
//! sans rien pour l'exercer. [`TransportNoise::chiffrer`] refuse donc net ce
//! qui dépasse une trame, plutôt que de tronquer en silence.

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
}

impl std::fmt::Debug for TransportNoise {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Rien du materiel de clef ne doit passer par le journal.
        f.write_str("TransportNoise { <chiffre> }")
    }
}

impl TransportNoise {
    pub(super) fn nouveau(etat: TransportState) -> Self {
        Self { etat }
    }

    /// Chiffre une charge utile déjà préfixée de son octet de type.
    pub fn chiffrer(&mut self, clair: &[u8]) -> Result<Vec<u8>, ErreurSendspin> {
        if clair.is_empty() {
            return Err(ErreurSendspin::EtatInattendu(
                "une trame binaire porte au moins son octet de type",
            ));
        }
        if clair.len() > MAX_CLAIR {
            // Refus explicite plutot que troncature : la fragmentation est le
            // sujet de S2-c, et une trame coupee en silence produirait un flux
            // corrompu que rien ne nommerait.
            return Err(ErreurSendspin::EtatInattendu(
                "charge utile au-dela d'une trame : la fragmentation est S2-c",
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
