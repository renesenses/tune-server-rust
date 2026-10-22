//! `p2p-mesure serveur` (cahier § 2.2 et § 5).
//!
//! Refuse toute ALPN autre que `tune-mesure/1` et tout identifiant d'endpoint
//! hors de la liste blanche de la campagne ; vérifie le jeton du testeur dans
//! la poignée de main. Ne sert que des octets synthétiques. S'arrête seul au
//! bout de 24 h au plus.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use iroh::endpoint::{Connection, SendStream};
use iroh::{Endpoint, EndpointId};
use tokio::task::JoinSet;

use crate::protocole::{
    code, debit_cible, ecrire_trame, lire_trame, Bonjour, Commande, InfoServeur, Reponse, ALPN,
    BLOC, CODE_ALPN, CODE_ARRET, CODE_IDENTIFIANT_INCONNU, CODE_REFUS, FLUX_MAX, TAILLE_PING,
    TAMPON_S, VERSION, VRAC_MAX,
};

/// Durée de vie maximale du serveur (cahier § 5 : « s'arrêter seul après 24 h »).
pub const DUREE_DE_VIE_MAX: Duration = Duration::from_secs(24 * 3600);
/// Connexions servies en même temps, au plus.
const CONNEXIONS_MAX: usize = 8;

pub fn duree_de_vie(demandee: Option<Duration>) -> Duration {
    demandee.unwrap_or(DUREE_DE_VIE_MAX).min(DUREE_DE_VIE_MAX)
}

#[derive(Debug, Clone)]
pub struct ConfigServeur {
    pub campagne: String,
    /// testeur → jeton.
    pub jetons: HashMap<String, String>,
    pub autorises: HashSet<EndpointId>,
    pub info: InfoServeur,
}

/// Lignes `testeur jeton` ; `#` commente.
pub fn lire_jetons(texte: &str) -> Result<HashMap<String, String>> {
    let mut jetons = HashMap::new();
    for (n, ligne) in lignes_utiles(texte) {
        let mut champs = ligne.split_whitespace();
        let (Some(testeur), Some(jeton), None) = (champs.next(), champs.next(), champs.next())
        else {
            bail!("jetons, ligne {n} : attendu « testeur jeton »");
        };
        if !crate::journal::identifiant_valide(testeur) {
            bail!("jetons, ligne {n} : pseudonyme invalide (a-z, 0-9, -, _)");
        }
        if jeton.len() < 16 {
            bail!("jetons, ligne {n} : jeton trop court (16 caractères au moins)");
        }
        jetons.insert(testeur.to_string(), jeton.to_string());
    }
    if jetons.is_empty() {
        bail!("aucun jeton : le serveur refuserait tout le monde");
    }
    Ok(jetons)
}

/// Un identifiant d'endpoint par ligne ; `#` commente.
pub fn lire_autorises(texte: &str) -> Result<HashSet<EndpointId>> {
    let mut ids = HashSet::new();
    for (n, ligne) in lignes_utiles(texte) {
        let id = ligne.split_whitespace().next().unwrap_or_default();
        ids.insert(
            id.parse::<EndpointId>()
                .with_context(|| format!("liste blanche, ligne {n} : identifiant illisible"))?,
        );
    }
    if ids.is_empty() {
        bail!("liste blanche vide : le serveur refuserait tout le monde");
    }
    Ok(ids)
}

fn lignes_utiles(texte: &str) -> impl Iterator<Item = (usize, &str)> {
    texte.lines().enumerate().filter_map(|(i, l)| {
        let l = l.split('#').next().unwrap_or("").trim();
        (!l.is_empty()).then_some((i + 1, l))
    })
}

/// Comparaison en temps constant (longueurs mises à part).
fn egal_constant(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0u8, |acc, (x, y)| acc | (x ^ y))
            == 0
}

/// Vérifie la poignée de main. `Err` porte la raison renvoyée au client.
pub fn verifier(cfg: &ConfigServeur, b: &Bonjour) -> Result<(), String> {
    if b.v != VERSION {
        return Err(format!("version {} inconnue (attendu {VERSION})", b.v));
    }
    if b.campagne != cfg.campagne {
        return Err("campagne inconnue".into());
    }
    match cfg.jetons.get(&b.testeur) {
        Some(attendu) if egal_constant(attendu, &b.jeton) => Ok(()),
        _ => Err("jeton refusé".into()),
    }
}

/// Famille(s) des adresses du serveur, sans les adresses.
pub fn familles(endpoint: &Endpoint) -> String {
    let mut v4 = false;
    let mut v6 = false;
    for sa in endpoint.addr().ip_addrs() {
        match crate::protocole::famille(sa.ip()) {
            "ipv4" => v4 = true,
            _ => v6 = true,
        }
    }
    match (v4, v6) {
        (true, true) => "ipv4+ipv6",
        (true, false) => "ipv4",
        (false, true) => "ipv6",
        (false, false) => "aucune",
    }
    .to_string()
}

/// Boucle d'accueil, jusqu'à `duree` ou Ctrl-C. Ferme l'endpoint en sortant.
pub async fn servir(endpoint: Endpoint, cfg: ConfigServeur, duree: Duration) -> Result<()> {
    let cfg = Arc::new(cfg);
    let fin = tokio::time::sleep(duree_de_vie(Some(duree)));
    tokio::pin!(fin);
    let mut taches = JoinSet::new();
    loop {
        tokio::select! {
            _ = &mut fin => {
                eprintln!("arrêt : durée de vie atteinte ({} s)", duree.as_secs());
                break;
            }
            _ = tokio::signal::ctrl_c() => {
                eprintln!("arrêt demandé");
                break;
            }
            Some(_) = taches.join_next(), if !taches.is_empty() => {}
            entrant = endpoint.accept() => {
                let Some(entrant) = entrant else { break };
                if taches.len() >= CONNEXIONS_MAX {
                    entrant.refuse();
                    continue;
                }
                let cfg = cfg.clone();
                taches.spawn(async move {
                    let conn = match entrant.await {
                        Ok(c) => c,
                        Err(e) => {
                            eprintln!("connexion avortée : {}", crate::journal::caviarder(&e.to_string()));
                            return;
                        }
                    };
                    let qui = conn.remote_id().fmt_short().to_string();
                    match servir_connexion(conn, &cfg).await {
                        Ok(()) => eprintln!("[{qui}] essai terminé"),
                        Err(e) => eprintln!("[{qui}] {}", crate::journal::caviarder(&format!("{e:#}"))),
                    }
                });
            }
        }
    }
    taches.abort_all();
    endpoint.close().await;
    Ok(())
}

async fn servir_connexion(conn: Connection, cfg: &ConfigServeur) -> Result<()> {
    if conn.alpn() != ALPN {
        conn.close(code(CODE_ALPN), b"ALPN inconnue");
        bail!("ALPN inconnue, refusée");
    }
    if !cfg.autorises.contains(&conn.remote_id()) {
        conn.close(code(CODE_IDENTIFIANT_INCONNU), b"identifiant inconnu");
        bail!("identifiant inconnu, refusé");
    }
    let (mut send, mut recv) = tokio::time::timeout(Duration::from_secs(10), conn.accept_bi())
        .await
        .context("pas de poignée de main en 10 s")??;
    let bonjour: Bonjour = lire_trame(&mut recv).await?;
    if let Err(raison) = verifier(cfg, &bonjour) {
        let _ = ecrire_trame(
            &mut send,
            &Reponse {
                ok: false,
                raison: Some(raison.clone()),
                serveur: None,
            },
        )
        .await;
        let _ = send.finish();
        // Laisse au client le temps de lire la raison avant la fermeture.
        let _ = tokio::time::timeout(Duration::from_secs(2), conn.closed()).await;
        conn.close(code(CODE_REFUS), raison.as_bytes());
        bail!("{raison} (testeur {})", bonjour.testeur);
    }
    ecrire_trame(
        &mut send,
        &Reponse {
            ok: true,
            raison: None,
            serveur: Some(cfg.info.clone()),
        },
    )
    .await?;

    let mut echos = JoinSet::new();
    // Pings en datagrammes : renvoyés tels quels.
    {
        let conn = conn.clone();
        echos.spawn(async move {
            while let Ok(d) = conn.read_datagram().await {
                if d.len() == TAILLE_PING {
                    let _ = conn.send_datagram(d);
                }
            }
        });
    }
    // Pings sur flux, si le client n'a pas de datagrammes.
    {
        let conn = conn.clone();
        echos.spawn(async move {
            while let Ok((mut s, mut r)) = conn.accept_bi().await {
                tokio::spawn(async move {
                    let mut p = [0u8; TAILLE_PING];
                    while r.read_exact(&mut p).await.is_ok() {
                        if s.write_all(&p).await.is_err() {
                            break;
                        }
                    }
                });
            }
        });
    }

    loop {
        let commande: Commande = match lire_trame(&mut recv).await {
            Ok(c) => c,
            Err(_) => break,
        };
        match commande {
            Commande::Vrac { duree_ms } => {
                let mut uni = conn.open_uni().await?;
                envoyer_vrac(&mut uni, Duration::from_millis(duree_ms).min(VRAC_MAX)).await?;
                uni.finish()?;
            }
            Commande::Flux { format, duree_ms } => {
                let debit = debit_cible(&format).context("format inconnu")?;
                let mut uni = conn.open_uni().await?;
                envoyer_cadence(
                    &mut uni,
                    debit,
                    Duration::from_millis(duree_ms).min(FLUX_MAX),
                )
                .await?;
                uni.finish()?;
            }
            Commande::Fin => break,
        }
    }
    let _ = tokio::time::timeout(Duration::from_secs(5), conn.closed()).await;
    conn.close(code(CODE_ARRET), b"fin");
    echos.abort_all();
    Ok(())
}

/// Octets synthétiques : jamais un fichier.
static SYNTHETIQUE: [u8; BLOC] = [0x5a; BLOC];

async fn envoyer_vrac(uni: &mut SendStream, duree: Duration) -> Result<()> {
    let debut = Instant::now();
    while debut.elapsed() < duree {
        uni.write_all(&SYNTHETIQUE).await?;
    }
    Ok(())
}

/// Rafale initiale de `TAMPON_S` secondes, puis un bloc de 64 Kio à chaque
/// échéance du débit cible, comme un lecteur qui remplit son tampon.
async fn envoyer_cadence(uni: &mut SendStream, debit_bps: u64, duree: Duration) -> Result<()> {
    let octets_s = debit_bps as f64 / 8.0;
    let rafale = (TAMPON_S * octets_s / BLOC as f64).ceil() as u64;
    let debut = tokio::time::Instant::now();
    let mut envoyes: u64 = 0;
    loop {
        if envoyes >= rafale {
            let echeance =
                Duration::from_secs_f64((envoyes - rafale) as f64 * BLOC as f64 / octets_s);
            if echeance >= duree {
                break;
            }
            tokio::time::sleep_until(debut + echeance).await;
        }
        uni.write_all(&SYNTHETIQUE).await?;
        envoyes += 1;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use iroh::SecretKey;

    fn config() -> ConfigServeur {
        ConfigServeur {
            campagne: "c1".into(),
            jetons: lire_jetons("t03 0123456789abcdef0123  # Yves\n").unwrap(),
            autorises: HashSet::new(),
            info: InfoServeur {
                site: "domicile".into(),
                box_: "bbox".into(),
                famille: "ipv4".into(),
            },
        }
    }

    fn bonjour(testeur: &str, jeton: &str) -> Bonjour {
        Bonjour {
            v: VERSION,
            campagne: "c1".into(),
            testeur: testeur.into(),
            jeton: jeton.into(),
        }
    }

    #[test]
    fn la_poignee_de_main_verifie_le_jeton_du_testeur() {
        let cfg = config();
        assert_eq!(
            verifier(&cfg, &bonjour("t03", "0123456789abcdef0123")),
            Ok(())
        );
        assert_eq!(
            verifier(&cfg, &bonjour("t03", "0123456789abcdef0124")),
            Err("jeton refusé".into())
        );
        // Le jeton d'un autre testeur ne vaut pas pour soi.
        assert_eq!(
            verifier(&cfg, &bonjour("t04", "0123456789abcdef0123")),
            Err("jeton refusé".into())
        );
        let mut b = bonjour("t03", "0123456789abcdef0123");
        b.campagne = "c2".into();
        assert!(verifier(&cfg, &b).is_err());
        b.campagne = "c1".into();
        b.v = 2;
        assert!(verifier(&cfg, &b).is_err());
    }

    #[test]
    fn fichiers_de_campagne() {
        assert!(lire_jetons("# rien\n").is_err());
        assert!(lire_jetons("t03 court").is_err());
        assert!(lire_jetons("Jean 0123456789abcdef").is_err());
        let id = SecretKey::generate().public();
        let ids = lire_autorises(&format!("# campagne c1\n{id}  # t03\n")).unwrap();
        assert!(ids.contains(&id));
        assert!(lire_autorises("pas-un-identifiant").is_err());
        assert!(lire_autorises("").is_err());
    }

    #[test]
    fn la_duree_de_vie_ne_depasse_jamais_24_h() {
        assert_eq!(duree_de_vie(None), DUREE_DE_VIE_MAX);
        assert_eq!(
            duree_de_vie(Some(Duration::from_secs(48 * 3600))),
            DUREE_DE_VIE_MAX
        );
        assert_eq!(
            duree_de_vie(Some(Duration::from_secs(60))),
            Duration::from_secs(60)
        );
    }
}
