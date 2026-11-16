//! Le nom sous lequel ce serveur s'annonce sur le réseau (#8189).
//!
//! Avant : le MediaServer UPnP s'appelait « Tune Server » sur toutes les
//! machines, et l'instance mDNS `Tune (<nom d'hôte>)`. Sur un téléphone, le
//! nom d'hôte ne dit rien (`localhost` sur Android, souvent générique sur
//! iOS) : quatre Tune d'un même réseau s'affichaient sous le même nom.
//!
//! Ordre retenu, le même pour UPnP et mDNS :
//! 1. le nom choisi dans les réglages (`server_name`) ;
//! 2. le nom de l'appareil donné par l'application hôte
//!    (`definir_nom_de_l_appareil`, appelé par `tune-ffi`) ;
//! 3. le nom d'hôte du système.
//!
//! Un nom générique (« iPhone », « localhost »…) reçoit un suffixe court et
//! stable tiré de l'UDN, pour que deux téléphones restent distinguables.

use std::sync::RwLock;

static NOM_DE_L_APPAREIL: RwLock<Option<String>> = RwLock::new(None);

/// Le nom de l'appareil, donné par l'application hôte (iOS :
/// `UIDevice.current.name`). `None` ou vide : on revient au nom d'hôte.
pub fn definir_nom_de_l_appareil(nom: Option<&str>) {
    let nom = nom
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .map(|n| n.chars().take(63).collect::<String>());
    if let Ok(mut g) = NOM_DE_L_APPAREIL.write() {
        *g = nom;
    }
}

/// Le nom de l'appareil donné par l'hôte, sinon le nom d'hôte du système.
pub fn nom_de_l_appareil() -> String {
    NOM_DE_L_APPAREIL
        .read()
        .ok()
        .and_then(|g| g.clone())
        .unwrap_or_else(super::system_hostname)
}

/// Un nom qui ne distingue pas deux appareils.
pub fn est_generique(nom: &str) -> bool {
    let n = nom.trim().trim_end_matches(".local").to_ascii_lowercase();
    matches!(
        n.as_str(),
        "" | "localhost"
            | "tune-server"
            | "tune server"
            | "iphone"
            | "ipad"
            | "ipod touch"
            | "android"
            | "phone"
            | "tablet"
    )
}

/// Quatre caractères hexadécimaux stables tirés de l'UDN
/// (`uuid:3f2a…` → `3F2A`).
fn suffixe(udn: &str) -> String {
    let hex: String = udn
        .trim_start_matches("uuid:")
        .chars()
        .filter(|c| c.is_ascii_hexdigit())
        .take(4)
        .collect::<String>()
        .to_ascii_uppercase();
    if hex.is_empty() { "0000".into() } else { hex }
}

/// Le nom annoncé, sans la marque : `server_name` réglé, sinon le nom de
/// l'appareil, avec un suffixe s'il est générique.
pub fn nom_annonce(server_name: Option<&str>, udn: &str) -> String {
    if let Some(n) = server_name.map(str::trim).filter(|n| !n.is_empty()) {
        return n.to_string();
    }
    let appareil = nom_de_l_appareil();
    let appareil = appareil.trim().trim_end_matches(".local").to_string();
    if est_generique(&appareil) {
        let base = if appareil.is_empty() || appareil.eq_ignore_ascii_case("localhost") {
            "Tune"
        } else {
            appareil.as_str()
        };
        format!("{base} {}", suffixe(udn))
    } else {
        appareil
    }
}

/// Le nom d'instance publié : `Tune (<nom annoncé>)`.
pub fn nom_de_service(nom: &str) -> String {
    format!("Tune ({nom})")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn le_reglage_prime_sur_tout() {
        assert_eq!(nom_annonce(Some("  Salon "), "uuid:abcd"), "Salon");
    }

    #[test]
    fn un_nom_generique_recoit_un_suffixe_stable_et_distinct() {
        // Ce test pose le nom de l'appareil du processus : il le rend ensuite.
        definir_nom_de_l_appareil(Some("iPhone"));
        let a = nom_annonce(None, "uuid:3f2a9c00-0000-4000-8000-000000000001");
        let b = nom_annonce(None, "uuid:b71e0000-0000-4000-8000-000000000002");
        assert_eq!(a, "iPhone 3F2A");
        assert_eq!(b, "iPhone B71E");
        assert_ne!(a, b, "deux iPhone doivent rester distinguables");
        assert_eq!(nom_annonce(None, "uuid:3f2a9c00"), a, "suffixe stable");

        definir_nom_de_l_appareil(Some("localhost"));
        assert_eq!(nom_annonce(None, "uuid:c0de"), "Tune C0DE");

        definir_nom_de_l_appareil(Some("iPhone de Bertrand"));
        assert_eq!(nom_annonce(None, "uuid:c0de"), "iPhone de Bertrand");
        assert_eq!(nom_annonce(Some("Cuisine"), "uuid:c0de"), "Cuisine");

        definir_nom_de_l_appareil(None);
        assert_eq!(nom_de_l_appareil(), super::super::system_hostname());
    }

    #[test]
    fn generique() {
        for n in [
            "iPhone",
            "IPAD",
            "localhost",
            "localhost.local",
            "",
            "tune-server",
        ] {
            assert!(est_generique(n), "{n:?}");
        }
        for n in ["iPhone de Bertrand", "Mac-Studio-6", "pixel-8"] {
            assert!(!est_generique(n), "{n:?}");
        }
        assert_eq!(nom_de_service("Salon"), "Tune (Salon)");
    }
}
