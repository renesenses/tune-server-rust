//! La boucle qui tient l'état à jour : instantané `GET /api/v1/zones`, puis
//! le WebSocket `/ws` abonné à `playback.*`, reconnecté de 1 s à 30 s.

use std::collections::HashSet;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::{self, Message};

use crate::client::{ClientTune, JETON_REQUIS, url_du_websocket};
use crate::etat::{Changements, Etat};
use crate::pochette;

pub type EtatPartage = Arc<Mutex<Etat>>;

/// Qui signale les changements : le bus D-Bus en vrai, un faux en test.
// `async_trait` pose un `#[must_use]` sur un futur qui l'est déjà.
#[allow(clippy::double_must_use)]
#[async_trait::async_trait]
pub trait Notifieur: Send + Sync {
    async fn notifier(&self, changements: Changements);
}

const ATTENTE_MIN: Duration = Duration::from_secs(1);
const ATTENTE_MAX: Duration = Duration::from_secs(30);

/// Les délais de la boucle (réglables pour les essais).
#[derive(Debug, Clone, Copy)]
pub struct Delais {
    /// Un ping toutes les `ping`.
    pub ping: Duration,
    /// Sans réponse (pong ou autre trame) dans ce délai après un ping, le
    /// serveur est tenu pour perdu : câble débranché, NAS en veille, rien
    /// qui ferme la connexion. Sans ce délai, seule l'expiration TCP
    /// (~15 min) le révélait, et l'applet restait sur « Playing ».
    pub pong: Duration,
    /// Une connexion restée ouverte au moins ce temps remet l'attente de
    /// reconnexion à 1 s ; une connexion qui retombe aussitôt la fait croître.
    pub tenue: Duration,
}

impl Default for Delais {
    fn default() -> Self {
        Delais {
            ping: Duration::from_secs(30),
            pong: Duration::from_secs(10),
            tenue: Duration::from_secs(10),
        }
    }
}

pub fn attente_suivante(attente: Duration) -> Duration {
    (attente * 2).min(ATTENTE_MAX)
}

enum Rupture {
    /// 401 : le jeton manque ou a expiré. Réessayer n'y changera rien.
    Jeton,
    Autre(String),
}

impl From<String> for Rupture {
    fn from(e: String) -> Self {
        if e == JETON_REQUIS {
            Rupture::Jeton
        } else {
            Rupture::Autre(e)
        }
    }
}

/// Ne rend la main que sur une erreur sans issue (jeton refusé) ; rend son
/// message.
pub async fn suivre(
    client: ClientTune,
    etat: EtatPartage,
    notifieur: Arc<dyn Notifieur>,
) -> String {
    suivre_avec(client, etat, notifieur, Delais::default()).await
}

pub async fn suivre_avec(
    client: ClientTune,
    etat: EtatPartage,
    notifieur: Arc<dyn Notifieur>,
    delais: Delais,
) -> String {
    let mut attente = ATTENTE_MIN;
    loop {
        let debut = Instant::now();
        let issue = une_session(&client, &etat, &notifieur, delais).await;
        let c = etat.lock().expect("etat").deconnecte(Instant::now());
        if !c.rien() {
            notifieur.notifier(c).await;
        }
        match issue {
            Ok(()) => tracing::info!("connexion au serveur fermée, reconnexion"),
            Err(Rupture::Jeton) => return JETON_REQUIS.to_string(),
            Err(Rupture::Autre(e)) => tracing::warn!("serveur {} : {e}", client.serveur),
        }
        if debut.elapsed() >= delais.tenue {
            attente = ATTENTE_MIN;
        }
        tokio::time::sleep(attente).await;
        attente = attente_suivante(attente);
    }
}

async fn une_session(
    client: &ClientTune,
    etat: &EtatPartage,
    notifieur: &Arc<dyn Notifieur>,
    delais: Delais,
) -> Result<(), Rupture> {
    // Le WebSocket d'abord, l'instantané ensuite : un évènement arrivé entre
    // les deux n'est pas perdu.
    let mut requete = url_du_websocket(&client.serveur)
        .into_client_request()
        .map_err(|e| e.to_string())?;
    if let Some(a) = client.autorisation() {
        requete.headers_mut().insert(
            "Authorization",
            a.parse().map_err(|_| "jeton illisible".to_string())?,
        );
    }
    let (mut ws, _) = match tokio_tungstenite::connect_async(requete).await {
        Ok(ws) => ws,
        Err(tungstenite::Error::Http(reponse)) if reponse.status() == 401 => {
            return Err(Rupture::Jeton);
        }
        Err(e) => return Err(Rupture::Autre(format!("WebSocket : {e}"))),
    };
    ws.send(Message::Text(
        json!({ "subscribe": ["playback.*"] }).to_string().into(),
    ))
    .await
    .map_err(|e| e.to_string())?;

    let zone_id = etat.lock().expect("etat").zone_id;
    let zones = client.zones().await?;
    let zone = zones
        .iter()
        .find(|z| z.get("id").and_then(Value::as_i64) == Some(zone_id))
        .ok_or_else(|| format!("la zone {zone_id} n'existe plus sur le serveur"))?;
    let c = etat
        .lock()
        .expect("etat")
        .appliquer_zone(zone, Instant::now());
    publier(client, etat, notifieur, c).await;
    tracing::info!("connecté à {}, zone {zone_id}", client.serveur);

    let mut ping = tokio::time::interval(delais.ping);
    ping.tick().await;
    // Échéance du pong attendu ; toute trame reçue la lève.
    let mut pong_avant: Option<tokio::time::Instant> = None;
    loop {
        let echeance = pong_avant.unwrap_or_else(|| tokio::time::Instant::now() + delais.ping);
        tokio::select! {
            message = ws.next() => {
                pong_avant = None;
                let texte = match message {
                    Some(Ok(Message::Text(t))) => t,
                    Some(Ok(Message::Close(_))) | None => return Ok(()),
                    Some(Ok(_)) => continue,
                    Some(Err(e)) => return Err(Rupture::Autre(e.to_string())),
                };
                let Ok(valeur) = serde_json::from_str::<Value>(&texte) else { continue };
                let c = etat.lock().expect("etat").appliquer_evenement(&valeur, Instant::now());
                if let Some(c) = c {
                    publier(client, etat, notifieur, c).await;
                }
            }
            _ = ping.tick() => {
                ws.send(Message::Ping(Vec::new().into())).await.map_err(|e| e.to_string())?;
                if pong_avant.is_none() {
                    pong_avant = Some(tokio::time::Instant::now() + delais.pong);
                }
            }
            _ = tokio::time::sleep_until(echeance), if pong_avant.is_some() => {
                return Err(Rupture::Autre(format!(
                    "aucune réponse au ping en {:?} : serveur perdu",
                    delais.pong
                )));
            }
        }
    }
}

async fn publier(
    client: &ClientTune,
    etat: &EtatPartage,
    notifieur: &Arc<dyn Notifieur>,
    c: Changements,
) {
    let chercher = c.metadonnees;
    if !c.rien() {
        notifieur.notifier(c).await;
    }
    if chercher {
        chercher_pochette(client.clone(), etat.clone(), notifieur.clone());
    }
}

/// Les pochettes en cours de téléchargement : une même URL n'est jamais
/// demandée deux fois en parallèle.
static EN_COURS: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(Default::default);

/// Télécharge la pochette en tâche de fond, puis republie les métadonnées.
/// Faute de cache ou de téléchargement, l'URL HTTP est publiée telle quelle :
/// elle marche quand l'authentification est coupée.
fn chercher_pochette(client: ClientTune, etat: EtatPartage, notifieur: Arc<dyn Notifieur>) {
    let Some(url) = etat
        .lock()
        .expect("etat")
        .pochette_a_chercher(&client.serveur)
    else {
        return;
    };
    if !EN_COURS.lock().expect("en cours").insert(url.clone()) {
        return;
    }
    tokio::spawn(async move {
        let publiee = obtenir_pochette(&client, &url).await;
        EN_COURS.lock().expect("en cours").remove(&url);
        let a_publier = {
            let mut e = etat.lock().expect("etat");
            // La piste a pu changer pendant le téléchargement.
            if e.pochette_a_chercher(&client.serveur).as_deref() == Some(url.as_str()) {
                e.pochette = Some(publiee);
                true
            } else {
                false
            }
        };
        if a_publier {
            notifieur
                .notifier(Changements {
                    metadonnees: true,
                    ..Default::default()
                })
                .await;
        }
    });
}

async fn obtenir_pochette(client: &ClientTune, url: &str) -> String {
    let Some(dossier) = pochette::dossier_de_cache() else {
        return url.to_string();
    };
    if let Some(chemin) = pochette::deja_en_cache(&dossier, url) {
        return pochette::url_de_fichier(&chemin);
    }
    let resultat = match client.telecharger(url).await {
        Ok((octets, type_mime)) => {
            let nom = pochette::nom_en_cache(url, Some(&type_mime));
            pochette::ranger(&dossier, &nom, &octets).map_err(|e| e.to_string())
        }
        Err(e) => Err(e),
    };
    match resultat {
        Ok(chemin) => pochette::url_de_fichier(&chemin),
        Err(e) => {
            tracing::debug!("pochette {url} : {e}");
            url.to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn l_attente_double_jusqu_a_trente_secondes() {
        let mut a = ATTENTE_MIN;
        let mut suite = vec![];
        for _ in 0..7 {
            suite.push(a.as_secs());
            a = attente_suivante(a);
        }
        assert_eq!(suite, vec![1, 2, 4, 8, 16, 30, 30]);
    }

    #[test]
    fn un_401_est_une_rupture_sans_issue() {
        assert!(matches!(
            Rupture::from(JETON_REQUIS.to_string()),
            Rupture::Jeton
        ));
        assert!(matches!(
            Rupture::from("HTTP 500".to_string()),
            Rupture::Autre(_)
        ));
    }
}
