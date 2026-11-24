//! Le logo de Tune pour les points de contrôle UPnP (#9483).
//!
//! JPlay, BubbleUPnP & co affichent l'icône que la description d'un appareil
//! annonce dans `<iconList>`. Le MediaServer annonçait `/icon.png` sans
//! qu'aucune route ne le serve (404), et le MediaRenderer n'annonçait rien.
//!
//! Les icônes sont tirées du logo de l'application (`icon-512.png` du client
//! web), réduites en 48×48 et 120×120, en PNG et en JPEG : les deux tailles
//! et les deux formats que l'usage DLNA attend. Elles sont embarquées dans le
//! binaire : rien à installer, rien à chercher sur le disque.

/// Préfixe sous lequel les icônes sont servies, à la racine du serveur HTTP.
pub const PREFIXE_ICONES: &str = "/upnp/icons";

/// L'ancienne adresse annoncée par le MediaServer (`<url>/icon.png</url>`).
/// Un point de contrôle qui a gardé l'ancienne description en cache la
/// demande encore : elle sert l'icône PNG 120×120.
pub const ANCIENNE_ICONE: &str = "/icon.png";

/// Une icône annoncée et servie.
#[derive(Debug, Clone, Copy)]
pub struct Icone {
    /// Nom du fichier sous [`PREFIXE_ICONES`].
    pub nom: &'static str,
    pub mime: &'static str,
    pub taille: u32,
    pub octets: &'static [u8],
}

/// Les icônes, dans l'ordre où la description les annonce : le PNG d'abord
/// (le plus répandu), la grande taille en tête de chaque format.
pub const ICONES: [Icone; 4] = [
    Icone {
        nom: "tune-120.png",
        mime: "image/png",
        taille: 120,
        octets: include_bytes!("../assets/upnp/tune-120.png"),
    },
    Icone {
        nom: "tune-48.png",
        mime: "image/png",
        taille: 48,
        octets: include_bytes!("../assets/upnp/tune-48.png"),
    },
    Icone {
        nom: "tune-120.jpg",
        mime: "image/jpeg",
        taille: 120,
        octets: include_bytes!("../assets/upnp/tune-120.jpg"),
    },
    Icone {
        nom: "tune-48.jpg",
        mime: "image/jpeg",
        taille: 48,
        octets: include_bytes!("../assets/upnp/tune-48.jpg"),
    },
];

/// L'icône qui porte ce nom de fichier.
pub fn icone(nom: &str) -> Option<&'static Icone> {
    ICONES.iter().find(|i| i.nom == nom)
}

/// L'icône servie à ce chemin HTTP (`/upnp/icons/<nom>` ou l'ancien
/// `/icon.png`).
pub fn icone_au_chemin(chemin: &str) -> Option<&'static Icone> {
    if chemin == ANCIENNE_ICONE {
        return icone("tune-120.png");
    }
    chemin
        .strip_prefix(PREFIXE_ICONES)
        .and_then(|reste| reste.strip_prefix('/'))
        .and_then(icone)
}

/// Le bloc `<iconList>` d'une description d'appareil. Les URL sont absolues
/// (`base` = `http://ip:port`), comme les `controlURL` (#1613/#1719).
pub fn icon_list_xml(base: &str) -> String {
    let mut xml = String::from("<iconList>");
    for i in ICONES.iter() {
        xml.push_str(&format!(
            "\n      <icon>\n        <mimetype>{mime}</mimetype>\n        \
             <width>{t}</width><height>{t}</height><depth>24</depth>\n        \
             <url>{base}{PREFIXE_ICONES}/{nom}</url>\n      </icon>",
            mime = i.mime,
            t = i.taille,
            nom = i.nom,
        ));
    }
    xml.push_str("\n    </iconList>");
    xml
}

/// Les `<url>` des icônes d'une description, dans l'ordre.
pub fn urls_des_icones(description: &str) -> Vec<String> {
    let Some(debut) = description.find("<iconList>") else {
        return Vec::new();
    };
    let Some(fin) = description[debut..].find("</iconList>") else {
        return Vec::new();
    };
    description[debut..debut + fin]
        .split("<url>")
        .skip(1)
        .filter_map(|morceau| morceau.split("</url>").next())
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Largeur et hauteur lues dans l'en-tête de l'image (IHDR pour le PNG,
    /// SOF0 pour le JPEG).
    fn dimensions(octets: &[u8]) -> (u32, u32) {
        if octets.starts_with(b"\x89PNG\r\n\x1a\n") {
            let l = u32::from_be_bytes(octets[16..20].try_into().unwrap());
            let h = u32::from_be_bytes(octets[20..24].try_into().unwrap());
            return (l, h);
        }
        assert!(octets.starts_with(&[0xFF, 0xD8]), "ni PNG ni JPEG");
        let mut i = 2;
        while i + 9 < octets.len() {
            assert_eq!(octets[i], 0xFF);
            let marqueur = octets[i + 1];
            let long = u16::from_be_bytes([octets[i + 2], octets[i + 3]]) as usize;
            if (0xC0..=0xC2).contains(&marqueur) {
                let h = u16::from_be_bytes([octets[i + 5], octets[i + 6]]) as u32;
                let l = u16::from_be_bytes([octets[i + 7], octets[i + 8]]) as u32;
                return (l, h);
            }
            i += 2 + long;
        }
        panic!("JPEG sans SOF");
    }

    #[test]
    fn chaque_icone_a_la_taille_et_le_format_annonces() {
        for i in ICONES.iter() {
            assert_eq!(dimensions(i.octets), (i.taille, i.taille), "{}", i.nom);
            let png = i.octets.starts_with(b"\x89PNG");
            assert_eq!(png, i.mime == "image/png", "{}", i.nom);
        }
    }

    #[test]
    fn chaque_url_annoncee_designe_une_icone_servie() {
        let xml = icon_list_xml("http://10.0.0.2:8888");
        let urls = urls_des_icones(&xml);
        assert_eq!(urls.len(), ICONES.len());
        for url in urls {
            let chemin = url.strip_prefix("http://10.0.0.2:8888").unwrap();
            assert!(icone_au_chemin(chemin).is_some(), "{url}");
        }
        assert_eq!(icone_au_chemin("/icon.png").unwrap().mime, "image/png");
        assert!(icone_au_chemin("/upnp/icons/../secret").is_none());
    }
}
