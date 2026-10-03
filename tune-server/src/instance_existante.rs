//! #5640 — « Ouvrir l'instance existante » (décision de Bertrand, 02/10).
//!
//! Un double-clic sur le raccourci pendant que Tune tourne déjà trouvait le
//! port HTTP tenu, attendait 20 s, puis sortait en erreur — ou, sous Windows
//! avant la même PR, servait EN DOUBLE (`SO_REUSEADDR`). Désormais, si le port
//! est tenu par un Tune vivant de la même version et que ce lancement vient du
//! lanceur (pas d'un « Redémarrer » ni d'une mise à jour), on ouvre le
//! navigateur sur l'instance existante et on s'arrête proprement.
//!
//! Ce qui distingue un lancement d'une relance : le lanceur pose
//! `TUNE_OPEN_BROWSER=1` ; tous les chemins de relance internes le retirent
//! (`POST /system/restart`, l'`exec` de la mise à jour, `tune-update.bat` le
//! met à `0`) et la relance macOS après mise à jour pose
//! `TUNE_RELANCE_APRES_MAJ=1`. Le « Redémarrer » pose en plus, explicitement,
//! [`MARQUEUR_RELANCE_INTERNE`] : son enfant doit ATTENDRE que l'ancien
//! processus rende le port, jamais ouvrir le navigateur sur celui qui s'éteint.

use std::io::{Read, Write};
use std::time::Duration;

/// Posé par `POST /system/restart` sur le processus qu'il relance.
pub(crate) const MARQUEUR_RELANCE_INTERNE: &str = "TUNE_RELANCE_INTERNE";

/// Que faire quand le port HTTP est déjà pris au démarrage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConduitePortPris {
    /// Un Tune de la même version sert déjà : ouvrir le navigateur dessus,
    /// sortir avec le code 0.
    OuvrirLExistante,
    /// Conduite d'avant : réessayer (reprise du port sous Unix), puis l'erreur
    /// explicite.
    Attendre,
}

/// Ce que dit l'environnement du lancement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Lancement {
    /// `TUNE_OPEN_BROWSER=1` : lancé par un script de lancement.
    pub par_le_lanceur: bool,
    /// [`MARQUEUR_RELANCE_INTERNE`] ou `TUNE_RELANCE_APRES_MAJ` à `1`.
    pub relance_interne: bool,
}

impl Lancement {
    pub(crate) fn depuis_l_environnement() -> Self {
        let vaut_un = |nom: &str| std::env::var(nom).ok().as_deref() == Some("1");
        Self {
            par_le_lanceur: vaut_un("TUNE_OPEN_BROWSER"),
            relance_interne: vaut_un(MARQUEUR_RELANCE_INTERNE) || vaut_un("TUNE_RELANCE_APRES_MAJ"),
        }
    }
}

/// La décision, sans effet de bord.
///
/// `version_en_place` : la version que le détenteur du port a déclarée en se
/// présentant comme Tune (`None` : il ne répond pas comme Tune — autre
/// programme, ou Tune en cours de démarrage/d'arrêt).
pub(crate) fn conduite_port_pris(
    lancement: Lancement,
    version_en_place: Option<&str>,
    notre_version: &str,
) -> ConduitePortPris {
    if lancement.relance_interne || !lancement.par_le_lanceur {
        return ConduitePortPris::Attendre;
    }
    match version_en_place {
        // Une AUTRE version : la conduite d'avant (sous Unix, reprise du port
        // d'une instance périmée, #1158) — on n'ouvre pas une vieille version
        // à la place de celle qu'on vient de lancer.
        Some(v) if v == notre_version => ConduitePortPris::OuvrirLExistante,
        _ => ConduitePortPris::Attendre,
    }
}

/// Lit la réponse brute de `GET /api/v1/system/version` : la version si, et
/// seulement si, c'est Tune qui a répondu (statut 200, JSON
/// `{"version": …, "engine": "rust"}`).
pub(crate) fn version_de_tune_dans_la_reponse(brut: &str) -> Option<String> {
    let (tete, corps) = brut.split_once("\r\n\r\n")?;
    let statut = tete.lines().next()?;
    let mut morceaux = statut.split_whitespace();
    if !morceaux.next()?.starts_with("HTTP/") || morceaux.next()? != "200" {
        return None;
    }
    let json: serde_json::Value = serde_json::from_str(corps.trim()).ok()?;
    if json.get("engine")?.as_str()? != "rust" {
        return None;
    }
    Some(json.get("version")?.as_str()?.to_string())
}

/// Demande au détenteur du port local s'il est Tune, et sa version.
pub(crate) fn sonder_tune_sur_le_port(port: u16) -> Option<String> {
    let adresse = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let mut flux = std::net::TcpStream::connect_timeout(&adresse, Duration::from_secs(1)).ok()?;
    flux.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    flux.set_write_timeout(Some(Duration::from_secs(1))).ok()?;
    flux.write_all(
        b"GET /api/v1/system/version HTTP/1.0\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n",
    )
    .ok()?;
    let mut brut = Vec::new();
    // Borné : une réponse de version tient en quelques centaines d'octets.
    let _ = flux.take(64 * 1024).read_to_end(&mut brut);
    version_de_tune_dans_la_reponse(&String::from_utf8_lossy(&brut))
}

/// Le même geste que `opening_browser` au démarrage.
pub(crate) fn ouvrir_le_navigateur(url: &str) {
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(url).spawn();
    #[cfg(target_os = "windows")]
    let _ = std::process::Command::new("cmd")
        .args(["/C", "start", "", url])
        .spawn();
    #[cfg(target_os = "linux")]
    let _ = std::process::Command::new("xdg-open").arg(url).spawn();
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    let _ = url;
}

#[cfg(test)]
mod tests {
    use super::*;

    const LANCEUR: Lancement = Lancement {
        par_le_lanceur: true,
        relance_interne: false,
    };

    #[test]
    fn le_lanceur_qui_trouve_le_meme_tune_ouvre_l_existante() {
        assert_eq!(
            conduite_port_pris(LANCEUR, Some("1.0.0-rc1"), "1.0.0-rc1"),
            ConduitePortPris::OuvrirLExistante
        );
    }

    #[test]
    fn le_redemarrage_attend_toujours_que_l_ancien_rende_le_port() {
        // Le « Redémarrer » Windows : l'ancien répond encore pendant ses
        // derniers instants. Son enfant ne doit PAS s'y fier.
        let relance = Lancement {
            par_le_lanceur: true,
            relance_interne: true,
        };
        assert_eq!(
            conduite_port_pris(relance, Some("1.0.0-rc1"), "1.0.0-rc1"),
            ConduitePortPris::Attendre
        );
        // Et le chemin réel : TUNE_OPEN_BROWSER retiré par la relance.
        let sans_lanceur = Lancement {
            par_le_lanceur: false,
            relance_interne: false,
        };
        assert_eq!(
            conduite_port_pris(sans_lanceur, Some("1.0.0-rc1"), "1.0.0-rc1"),
            ConduitePortPris::Attendre
        );
    }

    #[test]
    fn un_autre_programme_ou_une_autre_version_garde_la_conduite_d_avant() {
        assert_eq!(
            conduite_port_pris(LANCEUR, None, "1.0.0-rc1"),
            ConduitePortPris::Attendre
        );
        assert_eq!(
            conduite_port_pris(LANCEUR, Some("0.9.169"), "1.0.0-rc1"),
            ConduitePortPris::Attendre
        );
    }

    #[test]
    fn seul_tune_est_reconnu_dans_la_reponse() {
        let tune = "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\r\n\
                    {\"version\":\"1.0.0-rc1\",\"engine\":\"rust\"}";
        assert_eq!(
            version_de_tune_dans_la_reponse(tune).as_deref(),
            Some("1.0.0-rc1")
        );
        // Lyrion/LMS ou autre serveur web sur le port : pas Tune.
        let autre = "HTTP/1.1 200 OK\r\n\r\n{\"version\":\"8.5\"}";
        assert_eq!(version_de_tune_dans_la_reponse(autre), None);
        let html = "HTTP/1.1 200 OK\r\n\r\n<html>LMS</html>";
        assert_eq!(version_de_tune_dans_la_reponse(html), None);
        // Tune qui démarre (répondeur de démarrage) ou en erreur : pas d'avis.
        let demarre = "HTTP/1.1 503 Service Unavailable\r\n\r\n\
                       {\"version\":\"1.0.0-rc1\",\"engine\":\"rust\"}";
        assert_eq!(version_de_tune_dans_la_reponse(demarre), None);
        assert_eq!(version_de_tune_dans_la_reponse(""), None);
    }

    /// La sonde réelle, contre un faux Tune et contre un port muet.
    #[test]
    fn la_sonde_reconnait_un_tune_local_et_rien_d_autre() {
        let ecoute = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = ecoute.local_addr().expect("addr").port();
        let fil = std::thread::spawn(move || {
            let (mut c, _) = ecoute.accept().expect("accept");
            let mut tampon = [0u8; 1024];
            let n = c.read(&mut tampon).expect("read");
            assert!(
                String::from_utf8_lossy(&tampon[..n]).starts_with("GET /api/v1/system/version ")
            );
            c.write_all(
                b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\r\n{\"version\":\"9.9.9\",\"engine\":\"rust\"}",
            )
            .expect("write");
        });
        assert_eq!(sonder_tune_sur_le_port(port).as_deref(), Some("9.9.9"));
        fil.join().expect("fil");

        let libre = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port_libre = libre.local_addr().expect("addr").port();
        drop(libre);
        assert_eq!(sonder_tune_sur_le_port(port_libre), None);
    }
}
