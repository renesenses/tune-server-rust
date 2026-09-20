//! Le LIEN qu'on colle dans une conversation — « Partager l'écoute ».
//!
//! # Le défaut
//!
//! Xavier Joly, 20/09/2026, collant un partage à Bertrand :
//!
//! ```text
//! Noche En L'alhambra — KOUROU FIA (Cokora)
//! http://localhost:8888/shared/000000000000000ba6c8f02d3db0e2bb
//! ```
//!
//! La route rend un chemin RELATIF (`/shared/<jeton>`) et le client le colle
//! derrière `location.origin`, c'est-à-dire l'adresse tapée dans SA barre
//! d'adresse. Xavier ouvre Tune sur `localhost` : le lien ne mène donc nulle
//! part, sauf sur sa propre machine. Un partage qui ne s'ouvre que chez celui
//! qui l'envoie n'est pas un partage.
//!
//! Le serveur, lui, CONNAÎT son adresse joignable : c'est celle qu'il écrit
//! déjà dans les URL de flux servies aux lecteurs réseau
//! (`dlna_set_uri_ok url="http://192.168.1.79:8888/stream/…"`).
//!
//! # 🔴 Ce qu'on ne fait pas
//!
//! On ne fabrique pas une adresse publique : une boucle locale
//! (`127.0.0.1`, `localhost`, `::1`) ou une adresse vide rendent `None`, et le
//! client garde son comportement d'avant. Annoncer `http://127.0.0.1:8888` à
//! un destinataire serait remplacer un lien faux par un autre lien faux.

/// La base absolue d'un lien de partage, ou `None` quand le serveur n'a
/// qu'une adresse de boucle locale à offrir.
pub fn base_de_partage(ip: &str, port: u16) -> Option<String> {
    let ip = ip.trim();
    if ip.is_empty() || port == 0 {
        return None;
    }
    let sans_crochets = ip.trim_start_matches('[').trim_end_matches(']');
    let boucle_locale = sans_crochets.eq_ignore_ascii_case("localhost")
        || sans_crochets == "::1"
        || sans_crochets
            .parse::<std::net::IpAddr>()
            .is_ok_and(|a| a.is_loopback());
    if boucle_locale {
        return None;
    }
    // Une IPv6 se met entre crochets dans une URL ; une IPv4 ou un nom, non.
    let hote = if sans_crochets.contains(':') {
        format!("[{sans_crochets}]")
    } else {
        sans_crochets.to_string()
    };
    Some(format!("http://{hote}:{port}"))
}

/// Le lien complet à coller, ou `None` : `base_de_partage` + le chemin rendu
/// par la route (`/shared/<jeton>`).
pub fn lien_de_partage(ip: &str, port: u16, chemin: &str) -> Option<String> {
    let base = base_de_partage(ip, port)?;
    let chemin = chemin.trim();
    if chemin.is_empty() {
        return None;
    }
    Some(if chemin.starts_with('/') {
        format!("{base}{chemin}")
    } else {
        format!("{base}/{chemin}")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Le cas de Xavier : le serveur est joignable, le lien doit porter SON
    /// adresse, pas celle de la barre d'adresse du navigateur.
    #[test]
    fn un_serveur_joignable_donne_un_lien_collable() {
        assert_eq!(
            lien_de_partage("192.168.1.79", 8888, "/shared/abc").as_deref(),
            Some("http://192.168.1.79:8888/shared/abc")
        );
    }

    /// 🔴 La contre-épreuve : une boucle locale ne vaut pas mieux que rien.
    /// On ne remplace pas un lien faux par un autre lien faux.
    #[test]
    fn une_boucle_locale_ne_donne_aucun_lien() {
        for ip in [
            "127.0.0.1",
            "localhost",
            "LOCALHOST",
            "::1",
            "[::1]",
            "127.0.1.5",
        ] {
            assert_eq!(base_de_partage(ip, 8888), None, "{ip}");
            assert_eq!(lien_de_partage(ip, 8888, "/shared/abc"), None, "{ip}");
        }
    }

    #[test]
    fn une_adresse_vide_ou_un_port_nul_ne_donnent_rien() {
        assert_eq!(base_de_partage("", 8888), None);
        assert_eq!(base_de_partage("   ", 8888), None);
        assert_eq!(base_de_partage("192.168.1.79", 0), None);
    }

    #[test]
    fn une_ipv6_est_mise_entre_crochets() {
        assert_eq!(
            lien_de_partage("fd00::1", 8888, "/shared/abc").as_deref(),
            Some("http://[fd00::1]:8888/shared/abc")
        );
        assert_eq!(
            lien_de_partage("[fd00::1]", 8888, "shared/abc").as_deref(),
            Some("http://[fd00::1]:8888/shared/abc")
        );
    }

    /// Un nom annoncé à la main (`advertised_ip`) passe tel quel : c'est le
    /// choix de l'utilisateur, et il peut valoir mieux qu'une IP.
    #[test]
    fn un_nom_dhote_annonce_est_respecte() {
        assert_eq!(
            base_de_partage("tune.maison.lan", 8888).as_deref(),
            Some("http://tune.maison.lan:8888")
        );
    }

    #[test]
    fn un_chemin_vide_ne_fabrique_pas_de_lien() {
        assert_eq!(lien_de_partage("192.168.1.79", 8888, ""), None);
    }
}
