//! `p2p-mesure client <ticket>` : joue un essai (cahier § 3.1) et en rend la
//! ligne de journal (§ 3.2).

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{bail, Context, Result};
use bytes::Bytes;
use iroh::endpoint::{Connection, ConnectionError};
use iroh::{Endpoint, EndpointAddr};
use tokio::sync::{mpsc, oneshot};

use crate::chemins::{self, DIRECT, INCONNU, RELAIS};
use crate::journal::{caviarder, Essai, Flux, InfoClient, Quantiles, Rtt};
use crate::protocole::{
    code, debit_cible, ecrire_trame, lire_ping, ping, Bonjour, Commande, Reponse, ALPN, BLOC,
    CODE_ALPN, CODE_FIN, CODE_IDENTIFIANT_INCONNU, CODE_REFUS, PERIODE_PING, TAILLE_PING, TAMPON_S,
    VERSION,
};
use crate::stats::{arrondi, gigue, quantile};
use crate::tampon::TamponSimule;

#[derive(Debug, Clone)]
pub struct Scenario {
    /// Phase de pings seuls (essai court : 60 s).
    pub pings: Duration,
    /// Envoi en vrac (essai long : 20 s ; zéro pour l'omettre).
    pub vrac: Duration,
    /// Formats cadencés, joués l'un après l'autre.
    pub formats: Vec<String>,
    pub duree_flux: Duration,
}

impl Scenario {
    pub fn court() -> Self {
        Self {
            pings: Duration::from_secs(60),
            vrac: Duration::ZERO,
            formats: vec![],
            duree_flux: Duration::from_secs(300),
        }
    }

    pub fn long() -> Self {
        Self {
            pings: Duration::ZERO,
            vrac: Duration::from_secs(20),
            formats: crate::protocole::FORMATS
                .iter()
                .map(|(f, _)| f.to_string())
                .collect(),
            duree_flux: Duration::from_secs(300),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ParamsClient {
    pub campagne: String,
    pub testeur: String,
    pub jeton: String,
    pub client: InfoClient,
    pub scenario: Scenario,
}

/// Étiquette du relais d'après le ticket, sans l'URL.
pub fn etiquette_relais(addr: &EndpointAddr) -> String {
    let Some(url) = addr.relay_urls().next() else {
        return "aucun".into();
    };
    let hote = url.host_str().unwrap_or("").trim_end_matches('.');
    if hote.ends_with("mozaiklabs.fr") {
        "mozaiklabs"
    } else if hote.ends_with("iroh.link") || hote.ends_with("n0.computer") {
        "n0"
    } else {
        "autre"
    }
    .into()
}

fn identifiant_d_essai() -> String {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos(),
    );
    h.write_u32(std::process::id());
    format!("{:016x}", h.finish())
}

fn ms(d: Duration) -> u64 {
    d.as_millis() as u64
}

/// Joue l'essai. Ne renvoie jamais d'erreur : un échec entre dans `erreur`,
/// avec ce qui a pu être mesuré avant.
pub async fn jouer(endpoint: &Endpoint, cible: EndpointAddr, p: &ParamsClient) -> Essai {
    let mut essai = Essai {
        v: 1,
        campagne: p.campagne.clone(),
        testeur: p.testeur.clone(),
        essai: identifiant_d_essai(),
        debut: humantime::format_rfc3339_seconds(SystemTime::now()).to_string(),
        client: p.client.clone(),
        serveur: None,
        relais: etiquette_relais(&cible),
        chemin_final: None,
        famille_direct: None,
        t_connecte_ms: None,
        t_premier_octet_ms: None,
        t_direct_ms: None,
        debit_max_mbps: None,
        flux: vec![],
        rtt: None,
        batterie: None,
        erreur: None,
    };
    let t0 = Instant::now();
    let conn =
        match tokio::time::timeout(Duration::from_secs(30), endpoint.connect(cible, ALPN)).await {
            Ok(Ok(c)) => c,
            Ok(Err(e)) => {
                essai.erreur = Some(caviarder(&format!("connexion : {e:#}")));
                return essai;
            }
            Err(_) => {
                essai.erreur = Some("connexion : délai de 30 s dépassé".into());
                return essai;
            }
        };
    essai.t_connecte_ms = Some(ms(t0.elapsed()));

    let courant = Arc::new(AtomicU8::new(INCONNU));
    let (stop_chemins, arret) = oneshot::channel();
    let suivi = tokio::spawn(chemins::suivre(conn.clone(), t0, courant.clone(), arret));

    let mut accepte = false;
    let resultat = derouler(&conn, t0, p, &courant, &mut essai, &mut accepte).await;
    let _ = stop_chemins.send(());
    let suivi = suivi.await.unwrap_or_default();

    if accepte {
        essai.chemin_final = chemins::chemin_final(&suivi.sequence).map(str::to_string);
        essai.famille_direct = Some(suivi.famille_direct.unwrap_or("aucune").to_string());
        essai.t_direct_ms = suivi.t_direct.map(ms);
    }
    if let Err(e) = resultat {
        let raison = match conn.close_reason() {
            Some(ConnectionError::ApplicationClosed(fermeture)) => {
                let motif = String::from_utf8_lossy(&fermeture.reason).into_owned();
                if [CODE_IDENTIFIANT_INCONNU, CODE_REFUS, CODE_ALPN]
                    .map(code)
                    .contains(&fermeture.error_code)
                {
                    format!("refusé par le serveur : {motif}")
                } else {
                    format!("connexion fermée par le serveur ({motif}) : {e:#}")
                }
            }
            _ => format!("{e:#}"),
        };
        essai.erreur = Some(caviarder(&raison));
    }
    conn.close(code(CODE_FIN), b"fin");
    essai
}

async fn derouler(
    conn: &Connection,
    t0: Instant,
    p: &ParamsClient,
    courant: &Arc<AtomicU8>,
    essai: &mut Essai,
    accepte: &mut bool,
) -> Result<()> {
    let (mut send, mut recv) = conn.open_bi().await?;
    ecrire_trame(
        &mut send,
        &Bonjour {
            v: VERSION,
            campagne: p.campagne.clone(),
            testeur: p.testeur.clone(),
            jeton: p.jeton.clone(),
        },
    )
    .await?;
    let mut longueur = [0u8; 4];
    recv.read_exact(&mut longueur).await?;
    essai.t_premier_octet_ms = Some(ms(t0.elapsed()));
    let reponse: Reponse = {
        let n = u32::from_be_bytes(longueur) as usize;
        if n > 16 * 1024 {
            bail!("réponse trop longue");
        }
        let mut octets = vec![0u8; n];
        recv.read_exact(&mut octets).await?;
        serde_json::from_slice(&octets)?
    };
    if !reponse.ok {
        bail!(
            "refusé par le serveur : {}",
            reponse.raison.unwrap_or_default()
        );
    }
    *accepte = true;
    essai.serveur = reponse.serveur;

    let (stop_pings, arret) = oneshot::channel();
    let pings = tokio::spawn(pinger(conn.clone(), t0, courant.clone(), arret));

    let deroule = async {
        tokio::time::sleep(p.scenario.pings).await;
        if !p.scenario.vrac.is_zero() {
            ecrire_trame(
                &mut send,
                &Commande::Vrac {
                    duree_ms: ms(p.scenario.vrac),
                },
            )
            .await?;
            let mut uni = conn.accept_uni().await?;
            let mut tampon = vec![0u8; BLOC];
            let mut total = 0u64;
            let mut premier = None;
            while let Some(n) = uni.read(&mut tampon).await? {
                premier.get_or_insert_with(Instant::now);
                total += n as u64;
            }
            let duree = premier.map(|t| t.elapsed()).unwrap_or_default();
            if !duree.is_zero() {
                essai.debit_max_mbps =
                    Some(arrondi(total as f64 * 8.0 / duree.as_secs_f64() / 1e6, 1));
            }
        }
        for format in &p.scenario.formats {
            let debit =
                debit_cible(format).with_context(|| format!("format inconnu : {format}"))?;
            ecrire_trame(
                &mut send,
                &Commande::Flux {
                    format: format.clone(),
                    duree_ms: ms(p.scenario.duree_flux),
                },
            )
            .await?;
            let mut uni = conn.accept_uni().await?;
            let mut lecteur = TamponSimule::new(debit, TAMPON_S);
            let mut tampon = vec![0u8; BLOC];
            let mut total = 0u64;
            let debut = Instant::now();
            while let Some(n) = uni.read(&mut tampon).await? {
                lecteur.recevoir(debut.elapsed().as_secs_f64(), n);
                total += n as u64;
            }
            let duree = debut.elapsed().as_secs_f64().max(1e-3);
            essai.flux.push(Flux {
                format: format.clone(),
                duree_s: p.scenario.duree_flux.as_secs(),
                debit_moyen_mbps: arrondi(total as f64 * 8.0 / duree / 1e6, 2),
                sous_debits: lecteur.sous_debits(),
            });
        }
        ecrire_trame(&mut send, &Commande::Fin).await?;
        let _ = send.finish();
        anyhow::Ok(())
    };
    let resultat = deroule.await;
    let _ = stop_pings.send(());
    if let Ok(mesure) = pings.await {
        essai.rtt = Some(mesure);
    }
    resultat
}

/// Pings toutes les 200 ms, horodatés à l'émission ; RTT rangés selon le
/// chemin courant à la réception.
async fn pinger(
    conn: Connection,
    t0: Instant,
    courant: Arc<AtomicU8>,
    mut arret: oneshot::Receiver<()>,
) -> Rtt {
    let (tx, mut rx) = mpsc::unbounded_channel::<(u64, u64)>();
    let datagrammes = conn.max_datagram_size().is_some_and(|m| m >= TAILLE_PING);
    let mut flux_ping = None;
    let lecteur = if datagrammes {
        let conn = conn.clone();
        tokio::spawn(async move {
            while let Ok(d) = conn.read_datagram().await {
                if let Some(p) = lire_ping(&d) {
                    if tx.send(p).is_err() {
                        break;
                    }
                }
            }
        })
    } else {
        match conn.open_bi().await {
            Ok((s, mut r)) => {
                flux_ping = Some(s);
                tokio::spawn(async move {
                    let mut p = [0u8; TAILLE_PING];
                    while r.read_exact(&mut p).await.is_ok() {
                        if let Some(p) = lire_ping(&p) {
                            if tx.send(p).is_err() {
                                break;
                            }
                        }
                    }
                })
            }
            Err(_) => tokio::spawn(async {}),
        }
    };

    let mut envoyes = 0u64;
    // seq → (rtt en ms, chemin à la réception)
    let mut recus: BTreeMap<u64, (f64, u8)> = BTreeMap::new();
    let mut tic = tokio::time::interval(PERIODE_PING);
    let mut fin: Option<tokio::time::Instant> = None;
    loop {
        let delai = async {
            match fin {
                Some(f) => tokio::time::sleep_until(f).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            _ = &mut arret, if fin.is_none() => {
                // Une seconde de grâce pour les derniers échos.
                fin = Some(tokio::time::Instant::now() + Duration::from_secs(1));
            }
            _ = delai => break,
            _ = tic.tick(), if fin.is_none() => {
                let t_us = t0.elapsed().as_micros() as u64;
                let trame = ping(envoyes, t_us);
                let ok = match flux_ping.as_mut() {
                    Some(s) => s.write_all(&trame).await.is_ok(),
                    None => conn.send_datagram(Bytes::copy_from_slice(&trame)).is_ok(),
                };
                if ok {
                    envoyes += 1;
                }
            }
            Some((seq, t_us)) = rx.recv() => {
                if seq < envoyes {
                    let rtt = t0.elapsed().as_micros().saturating_sub(t_us as u128) as f64 / 1000.0;
                    recus.entry(seq).or_insert((rtt, courant.load(Ordering::Relaxed)));
                }
            }
        }
    }
    lecteur.abort();

    let par_chemin = |c: u8| -> Option<Quantiles> {
        let v: Vec<f64> = recus
            .values()
            .filter(|(_, ch)| *ch == c)
            .map(|(r, _)| *r)
            .collect();
        Some(Quantiles {
            p50: arrondi(quantile(&v, 0.50)?, 1),
            p95: arrondi(quantile(&v, 0.95)?, 1),
            p99: Some(arrondi(quantile(&v, 0.99)?, 1)),
        })
    };
    let dans_l_ordre: Vec<f64> = recus.values().map(|(r, _)| *r).collect();
    Rtt {
        relais: par_chemin(RELAIS),
        direct: par_chemin(DIRECT),
        gigue_ms: gigue(&dans_l_ordre).map(|g| arrondi(g, 1)),
        pertes_ping: if envoyes == 0 {
            0.0
        } else {
            arrondi(1.0 - recus.len() as f64 / envoyes as f64, 4)
        },
    }
}
