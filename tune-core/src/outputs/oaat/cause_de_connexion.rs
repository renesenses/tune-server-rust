//! #3727 — pourquoi une connexion à un Tune Endpoint n'aboutit pas.
//!
//! # Le fait mesuré
//!
//! Le 09/09/2026 sur le .42 (`0.9.143`), le Tune Endpoint `192.168.1.44:9740`
//! était déjà tenu par le serveur .18. Sondé depuis une troisième machine :
//!
//! ```text
//! connexion TCP établie
//! AUCUNE réponse après 5.0 s — le port écoute mais l'application ne parle pas
//! ```
//!
//! Il n'a ni refusé, ni fermé, ni dit « occupé ». Le serveur, lui, écrivait
//! 36 fois :
//!
//! ```text
//! oaat: connect timed out, retry device=Tune Endpoint attempt=5 delay_ms=2000
//! ```
//!
//! « Connect timed out » envoie chercher un réseau en panne ou un appareil
//! éteint. L'appareil était allumé, joignable, et simplement **déjà pris** —
//! et rien dans le journal ne permettait de faire la différence.
//!
//! # Ce que ce module fait, et ce qu'il ne fait pas
//!
//! Le délai qui expire dans `output.rs` couvre TOUT `ConnectedEndpoint::connect`
//! (la poignée TCP **et** la poignée applicative). Quand il expire, ce module
//! redemande la seule couche TCP : si elle s'établit, c'est que le port écoute
//! et que c'est l'APPLICATION qui se tait.
//!
//! Il ne prétend pas prouver qu'un autre serveur tient l'endpoint — seul
//! l'endpoint le sait, et le dire franchement est le point 2 de #3727, qui
//! vit dans le dépôt `renesenses/oaat`. Il nomme ce qui est OBSERVABLE d'ici,
//! ce qui est déjà tout ce qui manquait pour ne pas chercher une heure du
//! mauvais côté.

use std::net::SocketAddr;
use std::time::Duration;

/// Ce que la sonde s'autorise à attendre.
///
/// Elle ne mesure QUE la poignée TCP : sur un réseau local c'est une affaire
/// de fractions de milliseconde, et un port fermé rend un RST immédiat. Le
/// seul cas lent est un SYN avalé sans réponse — pare-feu qui jette en
/// silence, hôte absent d'un réseau qui ne signale rien.
///
/// 🔴 **C'est une seconde ajoutée à CHAQUE tentative de la boucle**, qui en
/// tient quinze. Une seconde et pas trois : la boucle de `output.rs` existe
/// justement pour ne pas subir le délai de SYN du système (127 s sous Linux),
/// et ce serait la défaire que de rallonger son budget d'un tiers pour un
/// renseignement de journal. Au pire, l'appareil éteint est déclaré perdu
/// quinze secondes plus tard qu'avant.
pub const BUDGET_DE_SONDE: Duration = Duration::from_secs(1);

/// La cause d'un délai dépassé à la connexion, telle qu'elle est observable
/// depuis le serveur.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CauseDeConnexion {
    /// Le port n'accepte même pas la connexion TCP : appareil éteint, adresse
    /// périmée, pare-feu, câble. C'est ce que « connect timed out » laissait
    /// croire dans TOUS les cas.
    HoteInjoignable,
    /// Le port accepte la connexion TCP et l'application ne répond rien. Sur
    /// un Tune Endpoint, qui n'accepte qu'un serveur à la fois, c'est la
    /// signature d'un endpoint **déjà tenu par un autre serveur Tune**.
    EndpointMuet,
}

impl CauseDeConnexion {
    /// Le nom d'évènement du journal. Stable, fait pour être cherché.
    pub fn evenement(self) -> &'static str {
        match self {
            Self::HoteInjoignable => "oaat_endpoint_injoignable",
            Self::EndpointMuet => "oaat_endpoint_muet_probablement_deja_tenu",
        }
    }

    /// Une phrase en clair, pour un journal lu par un humain — et, le jour où
    /// quelqu'un branchera ce chemin sur l'écran, pour l'écran.
    pub fn message(self) -> &'static str {
        match self {
            Self::HoteInjoignable => {
                "la connexion TCP n'est pas acceptée : l'appareil est éteint, \
                 son adresse a changé, ou un pare-feu ferme le port"
            }
            Self::EndpointMuet => {
                "la connexion TCP est acceptée mais l'endpoint ne répond \
                 rien : il est probablement déjà tenu par un autre serveur \
                 Tune du réseau — un Tune Endpoint n'accepte qu'un serveur à \
                 la fois"
            }
        }
    }
}

/// Combien de verdicts « muet » CONSÉCUTIFS la boucle de connexion tolère
/// avant de cesser d'insister.
///
/// Le second volet de #3727 : nommer la cause ne suffisait pas, la boucle
/// continuait ses quinze tentatives, la zone restait `playing` tout du long,
/// et le superviseur de stall relançait la même boucle toutes les quarante
/// secondes — 36 délais expirés et 4 relances en trois minutes, mesurés sur
/// le .42.
///
/// **Trois, et pourquoi.** Un Tune Endpoint LIBRE répond sa poignée
/// applicative en quelques millisecondes ; un endpoint qui redémarre son
/// écouteur après un arrêt de piste REFUSE la connexion (RST) ou l'ignore
/// (SYN sans réponse), il ne l'accepte pas pour se taire — ces deux cas-là
/// gardent leurs quinze tentatives. Trois verdicts « accepté puis muet »
/// d'affilée, c'est au moins neuf secondes de silence applicatif sur un port
/// qui écoute : plus long que tout ce qu'un endpoint libre a jamais mis à
/// répondre, et assez court pour que la zone soit arrêtée — et le motif
/// affiché — AVANT que le superviseur de stall (30 s) ne vienne relancer la
/// même boucle en boucle.
pub const SEUIL_DE_VERDICTS_MUETS: u32 = 3;

/// Les verdicts accumulés par UNE boucle de connexion.
///
/// Elle ne compte que les « muet » consécutifs : un port qui refuse ou qui
/// ne répond pas remet le compte à zéro, parce que ce n'est plus la même
/// panne — et qu'un appareil qui s'éteint au milieu de la série ne doit pas
/// être accusé d'être tenu par un voisin.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct VerdictsDeConnexion {
    muets_consecutifs: u32,
}

impl VerdictsDeConnexion {
    /// Enregistre le verdict d'une tentative expirée. Rend `true` quand la
    /// boucle doit s'arrêter là : la cause est connue, insister ne changera
    /// rien, et chaque tentative de plus est une tentative pendant laquelle
    /// la zone prétend jouer.
    pub fn abandonner_apres(&mut self, cause: CauseDeConnexion) -> bool {
        match cause {
            CauseDeConnexion::EndpointMuet => {
                self.muets_consecutifs += 1;
                self.muets_consecutifs >= SEUIL_DE_VERDICTS_MUETS
            }
            CauseDeConnexion::HoteInjoignable => {
                self.muets_consecutifs = 0;
                false
            }
        }
    }

    /// Une tentative qui n'a pas expiré mais a été REFUSÉE (RST, erreur de
    /// poignée) n'est pas un silence : elle casse la série.
    pub fn connexion_refusee(&mut self) {
        self.muets_consecutifs = 0;
    }

    /// Le nombre de verdicts « muet » d'affilée, pour le journal et le motif.
    pub fn muets_consecutifs(self) -> u32 {
        self.muets_consecutifs
    }
}

/// Le motif remis au sondeur quand la boucle renonce — celui qui devient
/// `zone.playback_error` `fatal: true`, donc une phrase à l'écran au lieu
/// d'une zone qui prétend jouer (#3727, point 4 : « cet endpoint est utilisé
/// par un autre serveur Tune » est une phrase que l'écran peut afficher).
///
/// Il commence par le nom d'évènement, stable, pour qu'un journal client et
/// un journal serveur se retrouvent sur le même mot.
pub fn motif_de_panne(
    cause: CauseDeConnexion,
    nom_de_sortie: &str,
    adresse: SocketAddr,
    tentatives: u32,
) -> String {
    format!(
        "{} — sortie « {nom_de_sortie} » ({adresse}), {tentatives} tentative(s) : {}",
        cause.evenement(),
        cause.message()
    )
}

/// La règle, isolée de tout réseau pour qu'elle soit vérifiable telle quelle.
///
/// `tcp_accepte` : la seule couche TCP s'est-elle établie, une fois la poignée
/// complète abandonnée ?
pub fn cause_si_delai_depasse(tcp_accepte: bool) -> CauseDeConnexion {
    if tcp_accepte {
        CauseDeConnexion::EndpointMuet
    } else {
        CauseDeConnexion::HoteInjoignable
    }
}

/// Sonde la seule couche TCP et rend la cause. Jamais d'erreur : une sonde qui
/// échoue est exactement le cas « injoignable ».
pub async fn cause_de_delai_depasse(adresse: SocketAddr, delai: Duration) -> CauseDeConnexion {
    let accepte = matches!(
        tokio::time::timeout(delai, tokio::net::TcpStream::connect(adresse)).await,
        Ok(Ok(_))
    );
    cause_si_delai_depasse(accepte)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Le cas du .42, reconstitué : un port qui ACCEPTE et ne dit jamais rien.
    /// C'est la moitié du ticket que le journal ne savait pas nommer.
    #[tokio::test]
    async fn un_port_qui_accepte_et_se_tait_est_nomme_endpoint_muet() {
        let ecouteur = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let adresse = ecouteur.local_addr().unwrap();
        // Accepte, garde la connexion ouverte, n'écrit pas un octet — le
        // comportement mesuré de l'endpoint tenu par le .18.
        tokio::spawn(async move {
            let mut gardees = Vec::new();
            while let Ok((flux, _)) = ecouteur.accept().await {
                gardees.push(flux);
            }
        });
        assert_eq!(
            cause_de_delai_depasse(adresse, BUDGET_DE_SONDE).await,
            CauseDeConnexion::EndpointMuet,
            "le budget de production doit suffire à établir une poignée TCP locale"
        );
    }

    /// L'autre sens, sans lequel « toujours muet » resterait vert : un port
    /// que personne n'écoute reste « injoignable », et le journal ne doit pas
    /// se mettre à accuser un serveur voisin à chaque appareil éteint.
    #[tokio::test]
    async fn un_port_sans_personne_reste_hote_injoignable() {
        let ecouteur = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let adresse = ecouteur.local_addr().unwrap();
        drop(ecouteur);
        assert_eq!(
            cause_de_delai_depasse(adresse, BUDGET_DE_SONDE).await,
            CauseDeConnexion::HoteInjoignable,
        );
    }

    /// Trois « muet » d'affilée, et la boucle renonce — pas avant : un seul
    /// verdict est un indice, trois sont une signature.
    #[test]
    fn trois_verdicts_muets_consecutifs_font_renoncer_la_boucle() {
        let mut verdicts = VerdictsDeConnexion::default();
        for n in 1..SEUIL_DE_VERDICTS_MUETS {
            assert!(
                !verdicts.abandonner_apres(CauseDeConnexion::EndpointMuet),
                "{n} verdict(s) muet(s) ne suffisent pas encore"
            );
        }
        assert!(
            verdicts.abandonner_apres(CauseDeConnexion::EndpointMuet),
            "au seuil, la boucle doit renoncer"
        );
        assert_eq!(verdicts.muets_consecutifs(), SEUIL_DE_VERDICTS_MUETS);
    }

    /// Un appareil qui cesse de répondre au milieu de la série n'est pas un
    /// endpoint tenu : la série repart de zéro, la boucle garde ses quinze
    /// tentatives. Idem pour une connexion refusée.
    #[test]
    fn un_verdict_injoignable_ou_un_refus_casse_la_serie() {
        let mut verdicts = VerdictsDeConnexion::default();
        verdicts.abandonner_apres(CauseDeConnexion::EndpointMuet);
        verdicts.abandonner_apres(CauseDeConnexion::EndpointMuet);
        assert!(!verdicts.abandonner_apres(CauseDeConnexion::HoteInjoignable));
        assert_eq!(verdicts.muets_consecutifs(), 0);
        for _ in 0..SEUIL_DE_VERDICTS_MUETS {
            assert!(!verdicts.abandonner_apres(CauseDeConnexion::HoteInjoignable));
        }

        verdicts.abandonner_apres(CauseDeConnexion::EndpointMuet);
        verdicts.abandonner_apres(CauseDeConnexion::EndpointMuet);
        verdicts.connexion_refusee();
        assert_eq!(verdicts.muets_consecutifs(), 0);
        assert!(!verdicts.abandonner_apres(CauseDeConnexion::EndpointMuet));
    }

    /// Le motif remis à l'écran nomme l'évènement, la sortie, l'adresse et
    /// la phrase en clair — de quoi ne pas chercher une heure du mauvais côté.
    #[test]
    fn le_motif_de_panne_nomme_l_evenement_la_sortie_et_la_cause() {
        let adresse: SocketAddr = "192.168.1.44:9740".parse().unwrap();
        let motif = motif_de_panne(CauseDeConnexion::EndpointMuet, "Tune Endpoint", adresse, 3);
        assert!(motif.starts_with("oaat_endpoint_muet_probablement_deja_tenu"));
        assert!(motif.contains("Tune Endpoint"));
        assert!(motif.contains("192.168.1.44:9740"));
        assert!(motif.contains("3 tentative(s)"));
        assert!(motif.contains("autre serveur"));

        let motif = motif_de_panne(
            CauseDeConnexion::HoteInjoignable,
            "Tune Endpoint",
            adresse,
            15,
        );
        assert!(motif.starts_with("oaat_endpoint_injoignable"));
        assert!(!motif.contains("autre serveur"));
    }

    #[test]
    fn les_deux_causes_ne_portent_ni_le_meme_evenement_ni_le_meme_message() {
        let (a, b) = (
            CauseDeConnexion::HoteInjoignable,
            CauseDeConnexion::EndpointMuet,
        );
        assert_ne!(a.evenement(), b.evenement());
        assert_ne!(a.message(), b.message());
        assert!(b.message().contains("autre serveur"));
    }
}
