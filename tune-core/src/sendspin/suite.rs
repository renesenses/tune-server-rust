//! Les deux suites cryptographiques, et l'obligation qui pèse sur le serveur.
//!
//! La spécification définit deux suites et répartit l'obligation de façon
//! **asymétrique** : un client doit en supporter au moins une, **un serveur
//! doit supporter les deux**. Il n'y a aucune négociation — le client annonce
//! son choix dans `client/init`, le serveur s'y plie ou ferme la connexion.
//!
//! Tune étant le serveur, l'énumération ci-dessous doit rester exhaustive, et
//! le témoin `un_serveur_supporte_les_deux_suites` est là pour qu'on ne puisse
//! pas en retirer une sans faire rougir la porte.

use super::ErreurSendspin;

/// Une suite Noise admise par Sendspin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Suite {
    /// `25519_ChaChaPoly_SHA256`.
    ChaChaPoly,
    /// `25519_AESGCM_SHA256`.
    AesGcm,
}

impl Suite {
    /// Le nom tel qu'il circule dans le champ `suite` de `client/init`.
    pub const fn nom(self) -> &'static str {
        match self {
            Self::ChaChaPoly => "25519_ChaChaPoly_SHA256",
            Self::AesGcm => "25519_AESGCM_SHA256",
        }
    }

    /// Les deux suites, dans l'ordre où la spécification les présente.
    ///
    /// Rendu par valeur et non par une constante publique : un appelant ne doit
    /// pas pouvoir en retirer une de son côté.
    pub const fn toutes() -> [Self; 2] {
        [Self::ChaChaPoly, Self::AesGcm]
    }

    /// Relit le nom annoncé par le client.
    pub fn depuis_nom(nom: &str) -> Result<Self, ErreurSendspin> {
        Self::toutes()
            .into_iter()
            .find(|s| s.nom() == nom)
            .ok_or_else(|| ErreurSendspin::SuiteInconnue(nom.to_string()))
    }

    /// Le motif Noise complet à passer à la caisse `snow`.
    ///
    /// `KKpsk2` : les deux statiques sont connues d'avance de part et d'autre
    /// (`KK`), et la PSK est mêlée au SECOND message (`psk2`). C'est ce dernier
    /// point qui permet au répondeur de lire le message 1 — et donc d'y trouver
    /// le `psk_id` — avant de savoir quelle PSK employer.
    pub fn motif_noise(self) -> String {
        format!("Noise_KKpsk2_{}", self.nom())
    }

    /// L'indice de mélange de la PSK dans le motif `KKpsk2`.
    pub const fn position_psk() -> u8 {
        2
    }
}

impl std::fmt::Display for Suite {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.nom())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn un_serveur_supporte_les_deux_suites() {
        // La spécification est asymétrique : le client en choisit une, le
        // serveur doit savoir les deux. Retirer une variante ci-dessus fait
        // rougir ici, en nommant la suite disparue.
        let toutes = Suite::toutes();
        assert_eq!(
            toutes.len(),
            2,
            "un serveur Sendspin doit supporter DEUX suites"
        );
        for attendue in ["25519_ChaChaPoly_SHA256", "25519_AESGCM_SHA256"] {
            assert!(
                toutes.iter().any(|s| s.nom() == attendue),
                "la suite {attendue} est imposee au serveur et manque a l'appel"
            );
        }
    }

    #[test]
    fn le_motif_noise_est_kkpsk2_dans_les_deux_suites() {
        for suite in Suite::toutes() {
            let motif = suite.motif_noise();
            assert!(
                motif.starts_with("Noise_KKpsk2_"),
                "Sendspin impose KKpsk2, pas {motif}"
            );
            assert!(motif.ends_with(suite.nom()));
        }
        assert_eq!(
            Suite::position_psk(),
            2,
            "le 2 de KKpsk2 est la position de melange"
        );
    }

    #[test]
    fn une_suite_hors_specification_est_refusee_par_son_nom() {
        let erreur = Suite::depuis_nom("25519_AESGCM_SHA512").expect_err("doit refuser");
        assert_eq!(
            erreur,
            ErreurSendspin::SuiteInconnue("25519_AESGCM_SHA512".into())
        );
    }

    #[test]
    fn un_nom_fait_l_aller_retour() {
        for suite in Suite::toutes() {
            assert_eq!(Suite::depuis_nom(suite.nom()).expect("aller-retour"), suite);
        }
    }
}
