//! L'identité Sendspin : une paire de clés X25519 de longue durée.
//!
//! L'identifiant d'un pair — `client_id` côté enceinte, `server_id` côté Tune —
//! **est** sa clé publique X25519, encodée en base64url sans remplissage, soit
//! 43 caractères. Ce n'est pas un nom choisi : c'est une clé.
//!
//! Conséquence, déjà relevée en phase 1 et confirmée ici : **l'identité d'un
//! appareil n'existe pas avant la connexion.** Aucun TXT mDNS ne la porte, et
//! elle n'est connue qu'une fois la poignée de main faite. Découverte et
//! identité ne se réconcilient donc qu'après coup.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use x25519_dalek::{PublicKey, StaticSecret};

use super::ErreurSendspin;

/// Taille d'une clé X25519, brute.
pub const TAILLE_CLE: usize = 32;

/// Longueur d'un identifiant de pair : 32 octets en base64url sans
/// remplissage.
pub const TAILLE_IDENTIFIANT: usize = 43;

/// Encode en base64url **sans** remplissage — la seule forme que le protocole
/// fait circuler.
pub fn b64url(donnees: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(donnees)
}

/// Décode du base64url en tolérant un remplissage absent.
pub fn depuis_b64url(valeur: &str) -> Result<Vec<u8>, ErreurSendspin> {
    URL_SAFE_NO_PAD
        .decode(valeur.trim_end_matches('='))
        .map_err(|e| ErreurSendspin::IdentifiantInvalide(format!("base64url : {e}")))
}

/// Une identité statique Sendspin.
///
/// `Debug` est écrit à la main : la clé privée ne doit apparaître dans aucun
/// journal, et une dérivation automatique l'y aurait mise le jour où quelqu'un
/// journalise la structure entière.
#[derive(Clone)]
pub struct Identite {
    prive: [u8; TAILLE_CLE],
    public: [u8; TAILLE_CLE],
}

impl std::fmt::Debug for Identite {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // La clé publique EST l'identifiant : elle est publique par
        // construction. La privée n'est pas affichée, même tronquée.
        f.debug_struct("Identite")
            .field("id", &self.id())
            .field("prive", &"<masquee>")
            .finish()
    }
}

impl Identite {
    /// Tire une identité neuve du générateur du système.
    pub fn generer() -> Self {
        let prive = StaticSecret::random_from_rng(&mut rand_core::OsRng);
        Self::depuis_secret(prive)
    }

    /// Reconstruit une identité depuis sa clé privée brute.
    ///
    /// Sert à la persistance — que S2-a n'implémente pas : la clé de longue
    /// durée du serveur est une pièce de S2-b, où elle doit survivre à un
    /// redémarrage. Ici l'identité vit le temps du processus, et le `server_id`
    /// change donc à chaque relance. C'est assumé et écrit.
    pub fn depuis_prive(octets: [u8; TAILLE_CLE]) -> Self {
        Self::depuis_secret(StaticSecret::from(octets))
    }

    fn depuis_secret(prive: StaticSecret) -> Self {
        let public = PublicKey::from(&prive);
        Self {
            prive: prive.to_bytes(),
            public: public.to_bytes(),
        }
    }

    /// L'identifiant du pair : la clé publique en base64url, 43 caractères.
    pub fn id(&self) -> String {
        b64url(&self.public)
    }

    /// La clé privée brute, pour la couche Noise uniquement.
    pub fn prive(&self) -> &[u8; TAILLE_CLE] {
        &self.prive
    }

    /// La clé publique brute.
    pub fn public(&self) -> &[u8; TAILLE_CLE] {
        &self.public
    }
}

/// Relit un identifiant de pair et en ressort la clé publique brute.
///
/// La longueur est vérifiée AVANT le décodage et APRÈS : un base64url de 43
/// caractères peut décoder sur autre chose que 32 octets si l'appelant a laissé
/// passer des caractères hors alphabet, et la couche Noise, elle, se contente
/// de refuser plus tard sans dire pourquoi.
pub fn cle_publique_du_pair(identifiant: &str) -> Result<[u8; TAILLE_CLE], ErreurSendspin> {
    if identifiant.len() != TAILLE_IDENTIFIANT {
        return Err(ErreurSendspin::IdentifiantInvalide(format!(
            "{TAILLE_IDENTIFIANT} caracteres attendus, {} recus",
            identifiant.len()
        )));
    }
    let octets = depuis_b64url(identifiant)?;
    let taille = octets.len();
    octets.try_into().map_err(|_| {
        ErreurSendspin::IdentifiantInvalide(format!("{TAILLE_CLE} octets attendus, {taille} recus"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn un_identifiant_est_la_cle_publique_en_43_caracteres() {
        let ident = Identite::generer();
        let id = ident.id();
        assert_eq!(
            id.len(),
            TAILLE_IDENTIFIANT,
            "un identifiant Sendspin fait 43 caracteres, pas {}",
            id.len()
        );
        assert!(
            !id.contains('='),
            "le base64url du protocole est SANS remplissage : {id}"
        );
        assert_eq!(
            cle_publique_du_pair(&id).expect("relecture"),
            *ident.public(),
            "l'aller-retour identifiant -> cle publique doit rendre la meme cle"
        );
    }

    #[test]
    fn la_cle_privee_ne_fuit_pas_dans_le_journal() {
        let ident = Identite::generer();
        let trace = format!("{ident:?}");
        let prive_b64 = b64url(ident.prive());
        assert!(
            !trace.contains(&prive_b64),
            "le Debug d'une identite ne doit JAMAIS porter la cle privee"
        );
        assert!(
            trace.contains(&ident.id()),
            "il doit en revanche porter l'identifiant public, sinon il n'aide personne"
        );
    }

    #[test]
    fn une_identite_se_reconstruit_a_l_identique_depuis_sa_cle_privee() {
        let ident = Identite::generer();
        let refaite = Identite::depuis_prive(*ident.prive());
        assert_eq!(
            ident.id(),
            refaite.id(),
            "la persistance de S2-b s'appuiera sur cet aller-retour"
        );
    }

    #[test]
    fn un_identifiant_de_mauvaise_taille_est_refuse_avant_noise() {
        // Trop court, trop long, et hors alphabet : les trois doivent etre
        // nommes ici et pas laisses filer jusqu'a un echec Noise muet.
        for mauvais in ["", "AAAA", &"A".repeat(44), &"!".repeat(43)] {
            assert!(
                cle_publique_du_pair(mauvais).is_err(),
                "identifiant invalide accepte : {mauvais:?}"
            );
        }
    }
}
