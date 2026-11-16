//! La boucle qui tient l'état à jour : instantané `GET /api/v1/zones`, puis
//! le WebSocket `/ws` abonné à `playback.*`, reconnecté de 1 s à 30 s.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

use crate::client::{ClientTune, url_du_websocket};
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
/// Un ping régulier : un serveur disparu sans fermer la connexion (câble,
/// veille du NAS) finit par faire échouer l'envoi.
const PING: Duration = Duration::from_secs(30);

pub fn attente_suivante(attente: Duration) -> Duration {
    (attente * 2).min(ATTENTE_MAX)
}

/// Ne rend jamais la main.
pub async fn suivre(client: ClientTune, etat: EtatPartage, notifieur: Arc<dyn Notifieur>) {
    let mut attente = ATTENTE_MIN;
    loop {
        match une_session(&client, &etat, &notifieur).await {
            Ok(()) => {
                tracing::info!("connexion au serveur fermée, reconnexion");
                attente = ATTENTE_MIN;
            }
            Err(e) => tracing::warn!("serveur {} injoignable : {e}", client.serveur),
        }
        let c = etat.lock().expect("etat").deconnecte(Instant::now());
        if !c.rien() {
            notifieur.notifier(c).await;
        }
        tokio::time::sleep(attente).await;
        attente = attente_suivante(attente);
    }
}

async fn une_session(
    client: &ClientTune,
    etat: &EtatPartage,
    notifieur: &Arc<dyn Notifieur>,
) -> Result<(), String> {
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
    let (mut ws, _) = tokio_tungstenite::connect_async(requete)
        .await
        .map_err(|e| format!("WebSocket : {e}"))?;
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

    let mut ping = tokio::time::interval(PING);
    ping.tick().await;
    loop {
        tokio::select! {
            message = ws.next() => {
                let texte = match message {
                    Some(Ok(Message::Text(t))) => t,
                    Some(Ok(Message::Close(_))) | None => return Ok(()),
                    Some(Ok(_)) => continue,
                    Some(Err(e)) => return Err(e.to_string()),
                };
                let Ok(valeur) = serde_json::from_str::<Value>(&texte) else { continue };
                let c = etat.lock().expect("etat").appliquer_evenement(&valeur, Instant::now());
                if let Some(c) = c {
                    publier(client, etat, notifieur, c).await;
                }
            }
            _ = ping.tick() => {
                ws.send(Message::Ping(Vec::new().into())).await.map_err(|e| e.to_string())?;
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
    tokio::spawn(async move {
        let publiee = match pochette::dossier_de_cache() {
            Some(dossier) => match pochette::deja_en_cache(&dossier, &url) {
                Some(chemin) => pochette::url_de_fichier(&chemin),
                None => match client.telecharger(&url).await {
                    Ok((octets, type_mime)) => {
                        let nom = pochette::nom_en_cache(&url, type_mime.as_deref());
                        match pochette::ranger(&dossier, &nom, &octets) {
                            Ok(chemin) => pochette::url_de_fichier(&chemin),
                            Err(e) => {
                                tracing::debug!("pochette non rangée : {e}");
                                url.clone()
                            }
                        }
                    }
                    Err(e) => {
                        tracing::debug!("pochette {url} : {e}");
                        url.clone()
                    }
                },
            },
            None => url.clone(),
        };
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
}
