//! Le client du serveur Tune : API HTTP et adresse du WebSocket.

use serde_json::Value;

use crate::requetes::{Action, Commandes, requete};

/// Adresse par défaut : le serveur de la machine elle-même.
pub const SERVEUR_PAR_DEFAUT: &str = "http://localhost:8888";

/// Le serveur a l'authentification active et le jeton manque ou est refusé.
pub const JETON_REQUIS: &str =
    "le serveur exige un jeton valide : --token ou TUNE_TOKEN (HTTP 401)";

/// `192.168.1.20:8888` → `http://192.168.1.20:8888`, sans `/` final.
pub fn normaliser_serveur(saisie: &str) -> String {
    let brut = saisie.trim().trim_end_matches('/');
    if brut.is_empty() {
        return SERVEUR_PAR_DEFAUT.to_string();
    }
    let minuscule = brut.to_ascii_lowercase();
    if minuscule.starts_with("http://") || minuscule.starts_with("https://") {
        brut.to_string()
    } else {
        format!("http://{brut}")
    }
}

/// `http://h:8888` → `ws://h:8888/ws` ; `https` → `wss`.
pub fn url_du_websocket(serveur: &str) -> String {
    let minuscule = serveur.to_ascii_lowercase();
    if minuscule.starts_with("https://") {
        format!("wss://{}/ws", &serveur["https://".len()..])
    } else if minuscule.starts_with("http://") {
        format!("ws://{}/ws", &serveur["http://".len()..])
    } else {
        format!("ws://{serveur}/ws")
    }
}

/// En-tête `Authorization` du jeton : un JWT des clients web/remote
/// (`Bearer`), ou une clé d'API écrite `ApiKey …` (`tune-server/src/auth.rs`,
/// `extract_token_from_headers`).
pub fn en_tete_d_autorisation(jeton: Option<&str>) -> Option<String> {
    let j = jeton.map(str::trim).filter(|j| !j.is_empty())?;
    if j.starts_with("ApiKey ") || j.starts_with("Bearer ") {
        Some(j.to_string())
    } else {
        Some(format!("Bearer {j}"))
    }
}

/// `GET /zones` rend un tableau ; on accepte aussi `{ zones: [...] }`.
pub fn liste_de_zones(reponse: &Value) -> Vec<Value> {
    if let Some(a) = reponse.as_array() {
        return a.clone();
    }
    reponse
        .get("zones")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

fn id_de(zone: &Value) -> Option<i64> {
    zone.get("id").and_then(Value::as_i64)
}

/// La zone à suivre : celle demandée (identifiant ou nom, sans casse), sinon
/// celle qui joue, sinon une en pause, sinon la première — même choix que le
/// mini-lecteur.
pub fn choisir_zone<'a>(zones: &'a [Value], choix: Option<&str>) -> Result<&'a Value, String> {
    let avec_id: Vec<&Value> = zones.iter().filter(|z| id_de(z).is_some()).collect();
    if let Some(choix) = choix.map(str::trim).filter(|c| !c.is_empty()) {
        let par_id = choix.parse::<i64>().ok();
        return avec_id
            .iter()
            .find(|z| {
                par_id.is_some_and(|id| id_de(z) == Some(id))
                    || z.get("name")
                        .and_then(Value::as_str)
                        .is_some_and(|n| n.eq_ignore_ascii_case(choix))
            })
            .copied()
            .ok_or_else(|| format!("zone « {choix} » introuvable (voir --list-zones)"));
    }
    let etat = |z: &&Value, e: &str| z.get("state").and_then(Value::as_str) == Some(e);
    avec_id
        .iter()
        .find(|z| etat(z, "playing"))
        .or_else(|| avec_id.iter().find(|z| etat(z, "paused")))
        .or_else(|| avec_id.first())
        .copied()
        .ok_or_else(|| "le serveur n'a aucune zone".to_string())
}

/// Le client HTTP d'une zone.
#[derive(Clone)]
pub struct ClientTune {
    pub serveur: String,
    autorisation: Option<String>,
    http: reqwest::Client,
    pub zone_id: i64,
}

impl ClientTune {
    pub fn new(serveur: &str, jeton: Option<&str>) -> Self {
        ClientTune {
            serveur: normaliser_serveur(serveur),
            autorisation: en_tete_d_autorisation(jeton),
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .unwrap_or_default(),
            zone_id: 0,
        }
    }

    pub fn autorisation(&self) -> Option<&str> {
        self.autorisation.as_deref()
    }

    fn avec_jeton(&self, r: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.autorisation {
            Some(a) => r.header(reqwest::header::AUTHORIZATION, a),
            None => r,
        }
    }

    pub async fn zones(&self) -> Result<Vec<Value>, String> {
        let r = self
            .avec_jeton(self.http.get(format!("{}/api/v1/zones", self.serveur)))
            .send()
            .await
            .map_err(|e| e.to_string())?;
        let statut = r.status();
        if statut == reqwest::StatusCode::UNAUTHORIZED {
            return Err(JETON_REQUIS.to_string());
        }
        if !statut.is_success() {
            return Err(format!("GET /api/v1/zones : HTTP {statut}"));
        }
        let corps: Value = r
            .json()
            .await
            .map_err(|e| format!("GET /api/v1/zones : réponse illisible ({e})"))?;
        Ok(liste_de_zones(&corps))
    }

    /// Les octets et le type d'une pochette. Le jeton ne part que vers le
    /// serveur Tune lui-même, jamais vers une URL distante (Qobuz, TIDAL).
    pub async fn telecharger(&self, url: &str) -> Result<(Vec<u8>, Option<String>), String> {
        let mut r = self.http.get(url);
        if url.starts_with(&format!("{}/", self.serveur)) {
            r = self.avec_jeton(r);
        }
        let r = r.send().await.map_err(|e| e.to_string())?;
        if !r.status().is_success() {
            return Err(format!("HTTP {}", r.status()));
        }
        let type_mime = r
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        if let Some(t) = &type_mime
            && !t.starts_with("image/")
        {
            return Err(format!("pas une image : {t}"));
        }
        let octets = r.bytes().await.map_err(|e| e.to_string())?;
        Ok((octets.to_vec(), type_mime))
    }
}

#[async_trait::async_trait]
impl Commandes for ClientTune {
    async fn envoyer(&self, action: Action) -> Result<(), String> {
        let req = requete(self.zone_id, &action);
        let mut r = self.avec_jeton(self.http.post(format!("{}{}", self.serveur, req.chemin)));
        if let Some(corps) = &req.corps {
            r = r.json(corps);
        }
        let reponse = r.send().await.map_err(|e| e.to_string())?;
        if reponse.status().is_success() {
            Ok(())
        } else {
            Err(format!("POST {} : HTTP {}", req.chemin, reponse.status()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn adresse_saisie_normalisee() {
        assert_eq!(
            normaliser_serveur("192.168.1.20:8888/"),
            "http://192.168.1.20:8888"
        );
        assert_eq!(
            normaliser_serveur("https://tune.example"),
            "https://tune.example"
        );
        assert_eq!(normaliser_serveur("  "), SERVEUR_PAR_DEFAUT);
    }

    #[test]
    fn websocket_suit_le_schema() {
        assert_eq!(url_du_websocket("http://nas:8888"), "ws://nas:8888/ws");
        assert_eq!(
            url_du_websocket("https://tune.example"),
            "wss://tune.example/ws"
        );
    }

    #[test]
    fn jeton_en_bearer_ou_cle_d_api() {
        assert_eq!(en_tete_d_autorisation(None), None);
        assert_eq!(en_tete_d_autorisation(Some(" ")), None);
        assert_eq!(
            en_tete_d_autorisation(Some("eyJ.x.y")).as_deref(),
            Some("Bearer eyJ.x.y")
        );
        assert_eq!(
            en_tete_d_autorisation(Some("ApiKey k")).as_deref(),
            Some("ApiKey k")
        );
    }

    fn zones() -> Vec<Value> {
        liste_de_zones(&json!([
            {"id": 1, "name": "Bureau", "state": "stopped"},
            {"id": 2, "name": "Salon", "state": "paused"},
            {"id": 3, "name": "Cuisine", "state": "playing"},
        ]))
    }

    #[test]
    fn zone_par_identifiant_ou_par_nom() {
        let z = zones();
        assert_eq!(choisir_zone(&z, Some("2")).unwrap()["name"], "Salon");
        assert_eq!(choisir_zone(&z, Some("cuisine")).unwrap()["id"], 3);
        assert!(choisir_zone(&z, Some("Garage")).is_err());
    }

    #[test]
    fn sans_choix_la_zone_qui_joue_puis_en_pause_puis_la_premiere() {
        let mut z = zones();
        assert_eq!(choisir_zone(&z, None).unwrap()["id"], 3);
        z[2]["state"] = json!("stopped");
        assert_eq!(choisir_zone(&z, None).unwrap()["id"], 2);
        z[1]["state"] = json!("stopped");
        assert_eq!(choisir_zone(&z, None).unwrap()["id"], 1);
        assert!(choisir_zone(&[], None).is_err());
    }

    #[test]
    fn la_liste_accepte_les_deux_formes() {
        assert_eq!(liste_de_zones(&json!({"zones": [{"id": 1}]})).len(), 1);
        assert_eq!(liste_de_zones(&json!({})).len(), 0);
    }
}
