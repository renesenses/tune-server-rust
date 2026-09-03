//! Découverte des Squeezebox — le volet UDP du port 3483.
//!
//! Le protocole slimproto a DEUX volets sur le même numéro de port : la
//! connexion de contrôle en TCP (le module parent), et la découverte en UDP —
//! un lecteur qui démarre diffuse un datagramme en broadcast, et chaque
//! serveur du réseau répond avec son nom et son adresse.
//!
//! Tune n'implémentait que le TCP. Conséquence : une vraie Squeezebox, ou un
//! `squeezelite` en découverte automatique, ne trouvait jamais Tune tout
//! seul — il fallait lui donner l'adresse à la main (`squeezelite -s <ip>`).
//! Pour qui vient de LMS, où la découverte marche depuis vingt ans, c'était
//! une panne.
//!
//! ## Deux dialectes, les deux servis
//!
//! - **l'ancien** (SliMP3, SB1/SB2) : la requête commence par `d` ; la réponse
//!   est `D` suivie du nom du serveur sur EXACTEMENT 17 octets, complétés de
//!   zéros — c'est le format historique, les vieux firmwares comptent dessus ;
//! - **le récent, en TLV** (SB Radio/Touch, squeezelite, contrôleurs) : la
//!   requête commence par `e`, suivie d'étiquettes de 4 octets dont le champ
//!   longueur vaut 0 — « remplis-moi ». La réponse commence par `E` et rend
//!   chaque étiquette CONNUE avec sa valeur, dans l'ordre demandé.
//!
//! La décision de réponse est une fonction pure ([`repondre`]) : tout le
//! protocole se teste sans réseau. La boucle réseau ne fait que transporter.

use std::sync::Arc;
use tokio::net::UdpSocket;
use tracing::{debug, info, warn};

/// Le nom historique tient sur 17 octets, ni plus ni moins.
const LONGUEUR_NOM_ANCIEN: usize = 17;

/// Ce que le serveur sait dire de lui-même.
///
/// Tout sauf l'adresse : elle dépend de l'interface qui fait face au lecteur,
/// et se calcule donc PAR datagramme (cf. la boucle). Répondre une adresse
/// unique referait le défaut du DLNA multi-interfaces (#2202).
pub struct IdentiteServeur {
    pub nom: String,
    pub port_http: u16,
    pub port_cli: u16,
    pub version: String,
}

/// La réponse à un datagramme de découverte, s'il en mérite une.
///
/// `None` = silence : un datagramme qu'on ne comprend pas n'appelle pas de
/// réponse. Sur un port broadcast, répondre au hasard c'est parler à tout le
/// réseau.
pub fn repondre(paquet: &[u8], identite: &IdentiteServeur, ip: &str) -> Option<Vec<u8>> {
    match paquet.first()? {
        // ── L'ancien dialecte : `d` → `D` + nom sur 17 octets ──
        b'd' => {
            let mut reponse = Vec::with_capacity(1 + LONGUEUR_NOM_ANCIEN);
            reponse.push(b'D');
            let nom = identite.nom.as_bytes();
            let coupe = &nom[..nom.len().min(LONGUEUR_NOM_ANCIEN)];
            reponse.extend_from_slice(coupe);
            reponse.resize(1 + LONGUEUR_NOM_ANCIEN, 0);
            Some(reponse)
        }

        // ── Le dialecte TLV : `e` + étiquettes demandées ──
        b'e' => {
            let mut reponse = vec![b'E'];
            let mut servies = 0u8;
            let mut i = 1usize;
            // Chaque entrée : 4 octets d'étiquette + 1 octet de longueur +
            // `longueur` octets de données. Une requête met la longueur à 0.
            while i + 5 <= paquet.len() {
                let etiquette = &paquet[i..i + 4];
                let longueur = paquet[i + 4] as usize;
                i += 5;
                if i + longueur > paquet.len() {
                    // En-tête valide mais données tronquées : on sert ce qu'on
                    // a déjà lu et on s'arrête là, sans paniquer.
                    break;
                }
                i += longueur;

                let valeur: Option<String> = match etiquette {
                    b"NAME" => Some(identite.nom.clone()),
                    b"IPAD" => Some(ip.to_string()),
                    b"JSON" => Some(identite.port_http.to_string()),
                    b"CLIP" => Some(identite.port_cli.to_string()),
                    b"VERS" => Some(identite.version.clone()),
                    // Étiquette inconnue (UUID, JVID…) : on ne l'invente pas.
                    // LMS fait pareil — on ne répond que ce qu'on sait.
                    _ => None,
                };
                if let Some(v) = valeur {
                    let octets = v.as_bytes();
                    // Le champ longueur tient sur un octet : une valeur plus
                    // longue serait un mensonge de protocole.
                    let coupe = &octets[..octets.len().min(255)];
                    reponse.extend_from_slice(etiquette);
                    reponse.push(coupe.len() as u8);
                    reponse.extend_from_slice(coupe);
                    servies += 1;
                }
            }
            // Une requête `e` sans la moindre étiquette connue n'appelle pas
            // de réponse : `E` nu ne renseignerait personne.
            (servies > 0).then_some(reponse)
        }

        _ => None,
    }
}

/// L'adresse locale qui fait face à ce correspondant.
///
/// L'astuce du `connect` UDP : aucune trame ne part, mais le noyau choisit
/// l'interface de sortie et nous la donne. C'est la leçon du DLNA
/// multi-interfaces (#2202) : sur une machine à plusieurs pattes — Ethernet,
/// Wi-Fi, VPN — « notre adresse » n'existe pas, seule existe « notre adresse
/// vue de lui ».
async fn adresse_face_a(correspondant: std::net::SocketAddr) -> Option<String> {
    let sonde = UdpSocket::bind("0.0.0.0:0").await.ok()?;
    sonde.connect(correspondant).await.ok()?;
    Some(sonde.local_addr().ok()?.ip().to_string())
}

/// Arme le répondeur de découverte. Tourne pour toujours.
pub fn spawn(identite: IdentiteServeur) {
    tokio::spawn(async move {
        let port: u16 = std::env::var("TUNE_SLIMPROTO_PORT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(super::DEFAULT_PORT);
        let socket = match UdpSocket::bind(("0.0.0.0", port)).await {
            Ok(s) => Arc::new(s),
            Err(e) => {
                // Un LMS déjà installé sur la machine tient peut-être ce port :
                // le dire, plutôt qu'échouer en silence.
                //
                // ⚠️ Ce message affirmait « le TCP, lui, fonctionne toujours ».
                // Le terrain le contredit : cinq journaux de testeurs, deux
                // systèmes, montrent le bind TCP 3483 qui échoue lui aussi, et
                // deux d'entre eux le montrent tomber en MÊME TEMPS que l'UDP
                // (#2938). Rassurer sur le TCP ici, c'est mentir juste au
                // moment où le canal de lecture est mort. L'état réel du TCP se
                // lit désormais dans `super::etat_ecoute()`.
                warn!(port, error = %e, "slimproto_discovery_bind_failed — la découverte UDP des Squeezebox est désactivée ; les lecteurs devront recevoir l'adresse du serveur à la main. L'état du canal TCP est à vérifier séparément (/system/diagnostics/network, champ « slimproto »)");
                return;
            }
        };
        info!(port, "slimproto_discovery_started");

        let mut tampon = [0u8; 512];
        loop {
            let (n, qui) = match socket.recv_from(&mut tampon).await {
                Ok(x) => x,
                Err(e) => {
                    debug!(error = %e, "slimproto_discovery_recv_error");
                    continue;
                }
            };
            let ip = match adresse_face_a(qui).await {
                Some(ip) => ip,
                None => continue,
            };
            if let Some(reponse) = repondre(&tampon[..n], &identite, &ip) {
                debug!(peer = %qui, octets = reponse.len(), "slimproto_discovery_reply");
                let _ = socket.send_to(&reponse, qui).await;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identite() -> IdentiteServeur {
        IdentiteServeur {
            nom: "Tune".into(),
            port_http: 8888,
            port_cli: 9090,
            version: "0.9.104".into(),
        }
    }

    /// La requête exacte de squeezelite : quatre étiquettes vides.
    #[test]
    fn squeezelite_recoit_nom_adresse_et_ports() {
        let requete = b"eNAME\0IPAD\0JSON\0VERS\0";
        let r = repondre(requete, &identite(), "192.168.1.18").expect("une réponse");
        assert_eq!(r[0], b'E');
        let attendu: Vec<u8> = {
            let mut v = vec![b'E'];
            for (tag, val) in [
                (&b"NAME"[..], "Tune"),
                (b"IPAD", "192.168.1.18"),
                (b"JSON", "8888"),
                (b"VERS", "0.9.104"),
            ] {
                v.extend_from_slice(tag);
                v.push(val.len() as u8);
                v.extend_from_slice(val.as_bytes());
            }
            v
        };
        // L'ordre de la réponse est celui de la demande : c'est ainsi que les
        // lecteurs la relisent.
        assert_eq!(r, attendu);
    }

    #[test]
    fn une_etiquette_inconnue_est_passee_sous_silence() {
        let requete = b"eUUID\0NAME\0";
        let r = repondre(requete, &identite(), "10.0.0.2").expect("une réponse");
        // UUID sauté, NAME servi.
        assert_eq!(&r[1..5], b"NAME");
        assert!(!r.windows(4).any(|w| w == b"UUID"));
    }

    /// `E` nu ne renseigne personne : pas d'étiquette connue, pas de réponse.
    #[test]
    fn une_requete_sans_etiquette_connue_reste_sans_reponse() {
        assert!(repondre(b"eXXXX\0", &identite(), "10.0.0.2").is_none());
        assert!(repondre(b"e", &identite(), "10.0.0.2").is_none());
    }

    /// L'ancien dialecte : `D` + nom sur EXACTEMENT 17 octets.
    #[test]
    fn lancien_dialecte_recoit_le_nom_sur_dix_sept_octets() {
        let r = repondre(
            b"d\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0",
            &identite(),
            "10.0.0.2",
        )
        .expect("une réponse");
        assert_eq!(r.len(), 1 + LONGUEUR_NOM_ANCIEN);
        assert_eq!(r[0], b'D');
        assert_eq!(&r[1..5], b"Tune");
        assert!(
            r[5..].iter().all(|&b| b == 0),
            "le reste est complété de zéros"
        );
    }

    /// Un nom plus long que 17 octets est coupé, pas débordé.
    #[test]
    fn un_nom_trop_long_est_coupe_a_dix_sept() {
        let longue = IdentiteServeur {
            nom: "Un nom de serveur vraiment interminable".into(),
            ..identite()
        };
        let r = repondre(b"d", &longue, "10.0.0.2").expect("une réponse");
        assert_eq!(r.len(), 1 + LONGUEUR_NOM_ANCIEN);
    }

    /// Du bruit sur un port broadcast n'appelle AUCUNE réponse.
    #[test]
    fn le_bruit_reste_sans_reponse() {
        assert!(repondre(b"", &identite(), "10.0.0.2").is_none());
        assert!(repondre(b"x salut", &identite(), "10.0.0.2").is_none());
        assert!(repondre(b"\xff\xfe\xfd", &identite(), "10.0.0.2").is_none());
    }

    /// Un TLV tronqué en pleines données sert ce qui précède et s'arrête.
    #[test]
    fn un_tlv_tronque_ne_panique_pas() {
        // NAME demandé proprement, puis une étiquette qui annonce 200 octets
        // de données absentes.
        let mut requete = b"eNAME\0IPAD".to_vec();
        requete.push(200);
        let r = repondre(&requete, &identite(), "10.0.0.2").expect("une réponse");
        assert_eq!(&r[1..5], b"NAME");
        assert!(
            !r.windows(4).any(|w| w == b"IPAD"),
            "l'étiquette tronquée n'est pas servie"
        );
    }
}
