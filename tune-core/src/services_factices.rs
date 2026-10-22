//! #6336 — remplacer, EN TEST SEULEMENT, les programmes externes qu'appellent
//! les routes de matériel : le démon AirPlay 2 (`airplay-daemon`) et le
//! montage SMB (`mount.cifs`, ou l'assistant par `sudo`).
//!
//! Ces routes lançaient un vrai programme système, introuvable ou dangereux
//! dans un banc d'essai : on ne pouvait les éprouver qu'en modifiant le code de
//! production. Ici, un test déclare « quand le serveur lance `mount.cifs`,
//! lance plutôt ce faux », puis passe par le VRAI routeur.
//!
//! La feature `services-factices` n'est allumée que par les
//! `[dev-dependencies]` de `tune-server` (resolver 2), comme
//! `journal-de-test` : dans tout binaire publié, [`substitut`] rend toujours
//! `None`, sans registre ni verrou, et le chemin de production est celui
//! d'avant.
//!
//! Les services réseau (Qobuz, TIDAL, Deezer) ne passent PAS par ici : leurs
//! adresses de base se donnent à la construction du service
//! (`avec_bases_de_test`), service par service.
//!
//! Une exception, pour le même motif que les programmes : deux appels de la
//! licence et du SSO vers mozaiklabs.fr n'ont ni service à construire ni
//! réglage qui les redirige (`mozaik_base_url` ne les concerne pas) — le
//! battement horaire (`POST /api/v1/heartbeat`), qui porte le verdict de la
//! licence, et la liaison du serveur au compte (`POST
//! /api/v1/cloud-library/{server_id}/link`), faite à la connexion SSO et au
//! battement de compte. [`substituer_base`] / [`base_substituee`] leur donnent
//! une racine d'essai sous le nom [`MOZAIKLABS`], avec la même feature et le
//! même `None` hors feature.

/// Le nom de la racine mozaiklabs.fr pour [`substituer_base`].
pub const MOZAIKLABS: &str = "mozaiklabs";

#[cfg(any(test, feature = "services-factices"))]
mod registre {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};

    fn table() -> &'static Mutex<HashMap<String, String>> {
        static TABLE: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
        TABLE.get_or_init(|| Mutex::new(HashMap::new()))
    }

    pub fn substituer_programme(nom: &str, remplacant: &str) {
        table()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(nom.to_string(), remplacant.to_string());
    }

    pub fn substitut(nom: &str) -> Option<String> {
        table()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(nom)
            .cloned()
    }

    /// Les adresses de base : une table à part, pour qu'un nom de service ne
    /// puisse jamais être pris pour un nom de programme.
    fn bases() -> &'static Mutex<HashMap<String, String>> {
        static BASES: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
        BASES.get_or_init(|| Mutex::new(HashMap::new()))
    }

    pub fn substituer_base(service: &str, base: &str) {
        bases()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(service.to_string(), base.trim_end_matches('/').to_string());
    }

    pub fn base_substituee(service: &str) -> Option<String> {
        bases()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(service)
            .cloned()
    }
}

/// Fait lancer `remplacant` partout où le serveur lancerait `nom`.
///
/// Le registre vit le temps du PROCESSUS : un banc qui s'en sert tourne dans
/// son propre binaire de test, et chaque substitution y est la même pour tous
/// ses tests (le faux programme lit ses arguments pour décider de sa réponse).
#[cfg(any(test, feature = "services-factices"))]
pub fn substituer_programme(nom: &str, remplacant: &str) {
    registre::substituer_programme(nom, remplacant);
}

/// Le programme à lancer à la place de `nom`, s'il en a été déclaré un.
#[cfg(any(test, feature = "services-factices"))]
pub fn substitut(nom: &str) -> Option<String> {
    registre::substitut(nom)
}

/// Hors construction de tests : jamais de substitut.
#[cfg(not(any(test, feature = "services-factices")))]
#[inline(always)]
pub fn substitut(_nom: &str) -> Option<String> {
    None
}

/// Fait viser `base` (sans `/` final) par le service `service` au lieu de son
/// adresse de production. Même portée que [`substituer_programme`] : le
/// processus.
#[cfg(any(test, feature = "services-factices"))]
pub fn substituer_base(service: &str, base: &str) {
    registre::substituer_base(service, base);
}

/// L'adresse de base d'essai du service `service`, s'il en a été déclaré une.
#[cfg(any(test, feature = "services-factices"))]
pub fn base_substituee(service: &str) -> Option<String> {
    registre::base_substituee(service)
}

/// Hors construction de tests : jamais d'adresse d'essai.
#[cfg(not(any(test, feature = "services-factices")))]
#[inline(always)]
pub fn base_substituee(_service: &str) -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn un_programme_non_declare_n_a_pas_de_substitut() {
        assert_eq!(substitut("programme-jamais-declare-6336"), None);
    }

    #[test]
    fn un_programme_declare_rend_son_remplacant() {
        substituer_programme("programme-declare-6336", "/chemin/du/faux");
        assert_eq!(
            substitut("programme-declare-6336").as_deref(),
            Some("/chemin/du/faux")
        );
    }

    #[test]
    fn une_base_declaree_est_rendue_sans_barre_finale_et_a_part_des_programmes() {
        assert_eq!(base_substituee("service-jamais-declare-6336"), None);
        substituer_base("service-declare-6336", "http://127.0.0.1:9/");
        assert_eq!(
            base_substituee("service-declare-6336").as_deref(),
            Some("http://127.0.0.1:9")
        );
        assert_eq!(
            substitut("service-declare-6336"),
            None,
            "une base n'est pas un programme"
        );
    }
}
