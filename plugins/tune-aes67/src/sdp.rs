//! La description de session (SDP, RFC 4566) d'un flux AES67.
//!
//! Émettre un SDP figé ne demande pas d'analyseur (§ 3.1 de l'étude) : un
//! `format!` ligne par ligne suffit. L'analyseur minimal plus bas ne sert
//! qu'au récepteur de test, pour vérifier que ce que l'émetteur annonce est
//! bien ce qu'il envoie.

use std::net::Ipv4Addr;

use crate::horloge::ReferenceHorloge;

/// Ce qu'il faut pour écrire le SDP d'un flux.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DescriptionSession {
    /// `o=` : identifiant de session, stable pour la vie du flux.
    pub id_session: u64,
    /// `o=` : version, à incrémenter quand le SDP change.
    pub version: u64,
    /// Adresse IPv4 de l'émetteur (`o=`).
    pub origine: Ipv4Addr,
    /// `s=` : le nom que le récepteur affiche (« Tune - Salon »).
    pub nom: String,
    pub destination: Ipv4Addr,
    pub port: u16,
    /// TTL multicast, écrit après l'adresse dans `c=` (ignoré en unicast).
    pub ttl: u8,
    pub type_charge: u8,
    pub cadence: u32,
    pub canaux: u16,
    /// Durée de paquet, en microsecondes (1000 = 1 ms).
    pub duree_paquet_us: u32,
    pub reference: ReferenceHorloge,
}

/// Retire ce qui casserait une ligne SDP (fin de ligne, caractères de
/// contrôle) d'un texte saisi par l'utilisateur.
pub fn nettoyer_texte(texte: &str) -> String {
    texte
        .chars()
        .filter(|c| !c.is_control())
        .collect::<String>()
        .trim()
        .to_string()
}

/// `1000` → `1`, `125` → `0.125`, `250` → `0.25`.
fn ptime(duree_paquet_us: u32) -> String {
    let ms = duree_paquet_us / 1000;
    let reste = duree_paquet_us % 1000;
    if reste == 0 {
        return ms.to_string();
    }
    let fraction = format!("{reste:03}");
    format!("{ms}.{}", fraction.trim_end_matches('0'))
}

impl DescriptionSession {
    /// Le SDP, lignes terminées par CRLF comme l'exige la RFC 4566.
    pub fn ecrire(&self) -> String {
        let connexion = if self.destination.is_multicast() {
            format!("{}/{}", self.destination, self.ttl)
        } else {
            self.destination.to_string()
        };
        let lignes = [
            "v=0".to_string(),
            format!(
                "o=- {} {} IN IP4 {}",
                self.id_session, self.version, self.origine
            ),
            format!("s={}", nettoyer_texte(&self.nom)),
            format!("c=IN IP4 {connexion}"),
            "t=0 0".to_string(),
            format!("m=audio {} RTP/AVP {}", self.port, self.type_charge),
            format!(
                "a=rtpmap:{} L24/{}/{}",
                self.type_charge, self.cadence, self.canaux
            ),
            format!("a=ptime:{}", ptime(self.duree_paquet_us)),
            format!("a=ts-refclk:{}", self.reference.attribut_sdp()),
            "a=mediaclk:direct=0".to_string(),
            "a=recvonly".to_string(),
        ];
        let mut sdp = lignes.join("\r\n");
        sdp.push_str("\r\n");
        sdp
    }
}

/// Ce que le récepteur de test relit d'un SDP.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SdpLu {
    pub nom: String,
    pub destination: Option<Ipv4Addr>,
    pub port: u16,
    pub type_charge: u8,
    pub codage: String,
    pub cadence: u32,
    pub canaux: u16,
    pub ptime: String,
    pub ts_refclk: String,
    pub mediaclk: String,
}

/// Analyse minimale : `s=`, `c=`, `m=`, `a=rtpmap`, `a=ptime`,
/// `a=ts-refclk`, `a=mediaclk`. Refuse un SDP sans `v=0` ni `m=audio`.
pub fn lire(sdp: &str) -> Result<SdpLu, String> {
    let mut lu = SdpLu::default();
    let mut version = false;
    let mut media = false;
    for ligne in sdp.lines() {
        let ligne = ligne.trim_end_matches('\r');
        if ligne == "v=0" {
            version = true;
        } else if let Some(s) = ligne.strip_prefix("s=") {
            lu.nom = s.to_string();
        } else if let Some(c) = ligne.strip_prefix("c=IN IP4 ") {
            let adresse = c.split('/').next().unwrap_or(c);
            lu.destination = Some(
                adresse
                    .parse()
                    .map_err(|e| format!("c= illisible ({c}) : {e}"))?,
            );
        } else if let Some(m) = ligne.strip_prefix("m=audio ") {
            let champs: Vec<&str> = m.split_whitespace().collect();
            if champs.len() < 3 || champs[1] != "RTP/AVP" {
                return Err(format!("m= inattendu : {m}"));
            }
            lu.port = champs[0].parse().map_err(|e| format!("port : {e}"))?;
            lu.type_charge = champs[2].parse().map_err(|e| format!("type : {e}"))?;
            media = true;
        } else if let Some(r) = ligne.strip_prefix("a=rtpmap:") {
            let (_, format) = r.split_once(' ').ok_or("rtpmap sans format")?;
            let mut parties = format.split('/');
            lu.codage = parties.next().unwrap_or_default().to_string();
            lu.cadence = parties
                .next()
                .ok_or("rtpmap sans cadence")?
                .parse()
                .map_err(|e| format!("cadence : {e}"))?;
            lu.canaux = parties
                .next()
                .unwrap_or("1")
                .parse()
                .map_err(|e| format!("canaux : {e}"))?;
        } else if let Some(p) = ligne.strip_prefix("a=ptime:") {
            lu.ptime = p.to_string();
        } else if let Some(r) = ligne.strip_prefix("a=ts-refclk:") {
            lu.ts_refclk = r.to_string();
        } else if let Some(m) = ligne.strip_prefix("a=mediaclk:") {
            lu.mediaclk = m.to_string();
        }
    }
    if !version {
        return Err("SDP sans v=0".into());
    }
    if !media {
        return Err("SDP sans m=audio".into());
    }
    Ok(lu)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn salon() -> DescriptionSession {
        DescriptionSession {
            id_session: 42,
            version: 1,
            origine: Ipv4Addr::new(192, 168, 1, 18),
            nom: "Tune - Salon".into(),
            destination: Ipv4Addr::new(239, 69, 1, 10),
            port: 5004,
            ttl: 16,
            type_charge: 98,
            cadence: 48_000,
            canaux: 2,
            duree_paquet_us: 1000,
            reference: ReferenceHorloge::Locale,
        }
    }

    #[test]
    fn le_sdp_est_celui_du_profil_retenu_par_l_etude() {
        assert_eq!(
            salon().ecrire(),
            "v=0\r\n\
             o=- 42 1 IN IP4 192.168.1.18\r\n\
             s=Tune - Salon\r\n\
             c=IN IP4 239.69.1.10/16\r\n\
             t=0 0\r\n\
             m=audio 5004 RTP/AVP 98\r\n\
             a=rtpmap:98 L24/48000/2\r\n\
             a=ptime:1\r\n\
             a=ts-refclk:local\r\n\
             a=mediaclk:direct=0\r\n\
             a=recvonly\r\n"
        );
    }

    #[test]
    fn la_reference_ptp_prendra_la_place_de_l_horloge_locale() {
        let mut d = salon();
        d.reference = ReferenceHorloge::Ptp {
            identite_maitre: [0, 1, 2, 3, 4, 5, 6, 7],
            domaine: 0,
        };
        assert!(
            d.ecrire()
                .contains("a=ts-refclk:ptp=IEEE1588-2008:00-01-02-03-04-05-06-07:0\r\n")
        );
    }

    #[test]
    fn un_nom_saisi_ne_peut_pas_injecter_de_ligne() {
        let mut d = salon();
        d.nom = "Salon\r\na=inject".into();
        let sdp = d.ecrire();
        assert!(sdp.contains("s=Salona=inject\r\n"));
        assert_eq!(sdp.matches("\r\n").count(), 11);
    }

    #[test]
    fn la_duree_de_paquet_s_ecrit_en_millisecondes() {
        assert_eq!(ptime(1000), "1");
        assert_eq!(ptime(125), "0.125");
        assert_eq!(ptime(250), "0.25");
        assert_eq!(ptime(4000), "4");
    }

    #[test]
    fn l_unicast_n_a_pas_de_ttl_dans_c() {
        let mut d = salon();
        d.destination = Ipv4Addr::LOCALHOST;
        assert!(d.ecrire().contains("c=IN IP4 127.0.0.1\r\n"));
    }

    #[test]
    fn le_sdp_ecrit_se_relit() {
        let lu = lire(&salon().ecrire()).unwrap();
        assert_eq!(lu.nom, "Tune - Salon");
        assert_eq!(lu.destination, Some(Ipv4Addr::new(239, 69, 1, 10)));
        assert_eq!(lu.port, 5004);
        assert_eq!(lu.type_charge, 98);
        assert_eq!(lu.codage, "L24");
        assert_eq!(lu.cadence, 48_000);
        assert_eq!(lu.canaux, 2);
        assert_eq!(lu.ptime, "1");
        assert_eq!(lu.ts_refclk, "local");
        assert_eq!(lu.mediaclk, "direct=0");
        assert!(lire("s=x\r\n").is_err());
    }
}
