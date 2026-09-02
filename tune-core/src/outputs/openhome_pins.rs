//! Client du service OpenHome `av.openhome.org:Pins:1` (#2722).
//!
//! Les routes `/zones/{id}/pins` STOCKAIENT des objets dans `settings` puis
//! invoquaient la lecture Tune : elles n'atteignaient jamais le renderer, et
//! `max_slots` — que le contrat web exige — n'existait nulle part dans le
//! serveur. Corriger la seule enveloppe JSON aurait affiché une capacité que
//! l'appareil n'a jamais annoncée ; ce module est l'appel réel.
//!
//! Règle tenue ici de bout en bout : **aucune capacité n'est fabriquée**.
//! `device_max` est ce que l'appareil répond à `GetDeviceMax`, une lecture en
//! échec rend une erreur, et un tableau d'identifiants illisible remonte sa
//! valeur brute au lieu de se ramener à « aucun pin ».

use std::time::Duration;

use reqwest::Client;
use serde::Serialize;
use tracing::debug;

use super::openhome::extract_tag;

/// Type de service tel qu'il apparaît dans le descriptif de l'appareil.
pub const SVC_PINS: &str = "urn:av-openhome-org:service:Pins:1";

/// Clé de ce service dans la carte `service_urls` que produit la découverte
/// (`discovery::xml_parser::service_key`, qui connaît déjà `pins`).
pub const PINS_SERVICE_KEY: &str = "pins";

/// Même plafond que les autres appels SOAP OpenHome (`openhome.rs`).
const SOAP_TIMEOUT: Duration = Duration::from_secs(5);

/// Un pin tel que l'APPAREIL le décrit.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DevicePin {
    /// Identifiant attribué par l'appareil (`GetIdArray`).
    pub id: u32,
    /// Rang dans le tableau d'identifiants — c'est ce que l'écran manipule.
    pub index: usize,
    pub mode: String,
    #[serde(rename = "type")]
    pub pin_type: String,
    pub uri: String,
    pub title: String,
    pub description: String,
    pub artwork_uri: String,
    pub shuffle: bool,
}

/// Ce que l'appareil annonce en une lecture complète.
#[derive(Debug, Clone, PartialEq)]
pub struct PinsSnapshot {
    /// `DeviceMax` — la capacité **annoncée par l'appareil**. Jamais un
    /// littéral Tune : c'est tout l'objet de #2722.
    pub device_max: u32,
    pub pins: Vec<DevicePin>,
}

/// Ce qu'on demande à l'appareil de poser dans un emplacement.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PinWrite {
    pub index: usize,
    pub mode: String,
    pub pin_type: String,
    pub uri: String,
    pub title: String,
    pub description: String,
    pub artwork_uri: String,
    pub shuffle: bool,
}

/// Client SOAP du service `Pins:1` d'UN renderer.
#[derive(Clone)]
pub struct PinsService {
    control_url: String,
    client: Client,
}

impl PinsService {
    pub fn new(control_url: String, client: Client) -> Self {
        Self {
            control_url,
            client,
        }
    }

    /// Variante autonome, pour un appelant qui n'a pas déjà un client partagé.
    pub fn with_default_client(control_url: String) -> Self {
        let client = crate::http::client::builder()
            .timeout(SOAP_TIMEOUT)
            .build()
            .unwrap_or_default();
        Self::new(control_url, client)
    }

    pub fn control_url(&self) -> &str {
        &self.control_url
    }

    /// Un aller-retour SOAP. Un statut d'erreur est une erreur : contrairement
    /// au transport historique de `openhome.rs`, un `500` porteur d'un
    /// `<faultstring>` ne peut pas passer pour une réponse valide.
    async fn call(&self, action: &str, args: &[(&str, String)]) -> Result<String, String> {
        let mut body_args = String::new();
        for (cle, valeur) in args {
            let echappe = quick_xml::escape::escape(valeur.as_str());
            body_args.push_str(&format!("<{cle}>{echappe}</{cle}>"));
        }

        let envelope = format!(
            r#"<?xml version="1.0" encoding="utf-8"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/">
  <s:Body>
    <u:{action} xmlns:u="{SVC_PINS}">
      {body_args}
    </u:{action}>
  </s:Body>
</s:Envelope>"#
        );

        let reponse = self
            .client
            .post(&self.control_url)
            .header("Content-Type", r#"text/xml; charset="utf-8""#)
            .header("SOAPAction", format!("\"{SVC_PINS}#{action}\""))
            .body(envelope)
            .send()
            .await
            .map_err(|erreur| format!("pins {action}: envoi impossible: {erreur}"))?;

        let statut = reponse.status();
        let texte = reponse
            .text()
            .await
            .map_err(|erreur| format!("pins {action}: corps illisible: {erreur}"))?;

        if !statut.is_success() {
            let detail = extract_tag(&texte, "errorDescription")
                .or_else(|| extract_tag(&texte, "faultstring"))
                .unwrap_or_else(|| statut.to_string());
            return Err(format!("pins {action}: {statut} — {detail}"));
        }
        Ok(texte)
    }

    /// `GetDeviceMax` — le nombre d'emplacements que l'appareil annonce.
    ///
    /// Le service est publié sous deux noms d'action selon les révisions :
    /// #2722 nomme `GetDeviceMax`, la définition ohNet nomme
    /// `GetDeviceAccountMax`. On demande le premier, un refus fait retomber
    /// sur le second. Les deux en échec, c'est une erreur — jamais un défaut
    /// fabriqué.
    pub async fn device_max(&self) -> Result<u32, String> {
        let xml = match self.call("GetDeviceMax", &[]).await {
            Ok(xml) => xml,
            Err(premier) => {
                debug!(
                    error = %premier,
                    "pins_get_device_max_repli_sur_get_device_account_max"
                );
                self.call("GetDeviceAccountMax", &[])
                    .await
                    .map_err(|second| format!("{premier} / {second}"))?
            }
        };
        let brut = extract_tag(&xml, "DeviceMax").ok_or_else(|| {
            format!("pins GetDeviceMax: aucun <DeviceMax> dans la reponse: {xml}")
        })?;
        brut.parse::<u32>().map_err(|erreur| {
            format!("pins GetDeviceMax: <DeviceMax> illisible ({erreur}): {brut}")
        })
    }

    /// `GetIdArray` — les identifiants des emplacements de l'appareil.
    pub async fn id_array(&self) -> Result<Vec<u32>, String> {
        let xml = self.call("GetIdArray", &[]).await?;
        let brut = extract_tag(&xml, "IdArray").unwrap_or_default();
        parse_id_array(&brut)
    }

    /// `ReadList` — le détail des pins désignés par `ids`.
    pub async fn read_list(&self, ids: &[u32]) -> Result<Vec<DevicePin>, String> {
        let demandes: Vec<u32> = ids.iter().copied().filter(|id| *id != 0).collect();
        if demandes.is_empty() {
            return Ok(Vec::new());
        }
        let liste = demandes
            .iter()
            .map(|id| id.to_string())
            .collect::<Vec<_>>()
            .join(" ");
        let xml = self.call("ReadList", &[("Ids", liste)]).await?;
        let json = extract_tag(&xml, "List").unwrap_or_default();
        parse_pin_list(&json, ids)
    }

    /// `SetDevice` — pose (ou remplace) le pin d'un emplacement.
    pub async fn set_device(&self, pin: &PinWrite) -> Result<(), String> {
        self.call(
            "SetDevice",
            &[
                ("Index", pin.index.to_string()),
                ("Mode", pin.mode.clone()),
                ("Type", pin.pin_type.clone()),
                ("Uri", pin.uri.clone()),
                ("Title", pin.title.clone()),
                ("Description", pin.description.clone()),
                ("ArtworkUri", pin.artwork_uri.clone()),
                ("Shuffle", if pin.shuffle { "1" } else { "0" }.to_string()),
            ],
        )
        .await
        .map(|_| ())
    }

    /// `InvokeIndex` — déclenche l'emplacement de rang `index`.
    pub async fn invoke_index(&self, index: usize) -> Result<(), String> {
        self.call("InvokeIndex", &[("Index", index.to_string())])
            .await
            .map(|_| ())
    }

    /// `InvokeId` — déclenche le pin d'identifiant `id`.
    pub async fn invoke_id(&self, id: u32) -> Result<(), String> {
        self.call("InvokeId", &[("Id", id.to_string())])
            .await
            .map(|_| ())
    }

    /// `Clear` — retire le pin d'identifiant `id`.
    pub async fn clear(&self, id: u32) -> Result<(), String> {
        self.call("Clear", &[("Id", id.to_string())])
            .await
            .map(|_| ())
    }

    /// Lecture complète : `GetDeviceMax`, puis `GetIdArray`, puis `ReadList`.
    pub async fn snapshot(&self) -> Result<PinsSnapshot, String> {
        let device_max = self.device_max().await?;
        let ids = self.id_array().await?;
        let pins = self.read_list(&ids).await?;
        Ok(PinsSnapshot { device_max, pins })
    }
}

/// Décode `IdArray`.
///
/// OpenHome publie ce tableau sous deux formes selon les appareils : une liste
/// décimale séparée par des virgules ou des espaces, ou — comme `Playlist:1` —
/// un base64 d'entiers 32 bits gros-boutiens. Les deux sont acceptées. Une
/// troisième forme n'est **pas** ramenée à une liste vide : elle remonte la
/// valeur brute dans l'erreur, pour qu'un essai sur appareil réel dise ce qui
/// est arrivé au lieu d'afficher « aucun pin ».
pub fn parse_id_array(brut: &str) -> Result<Vec<u32>, String> {
    let texte = brut.trim();
    if texte.is_empty() {
        return Ok(Vec::new());
    }
    let jetons: Vec<&str> = texte
        .split([',', ' ', '\n', '\t', '\r'])
        .filter(|jeton| !jeton.is_empty())
        .collect();
    if jetons
        .iter()
        .all(|jeton| jeton.bytes().all(|octet| octet.is_ascii_digit()))
    {
        return jetons
            .iter()
            .map(|jeton| {
                jeton
                    .parse::<u32>()
                    .map_err(|erreur| format!("pins GetIdArray: « {jeton} » illisible: {erreur}"))
            })
            .collect();
    }
    if let Some(octets) = decode_base64(texte)
        && octets.len().is_multiple_of(4)
    {
        return Ok(octets
            .as_chunks::<4>()
            .0
            .iter()
            .map(|bloc| u32::from_be_bytes(*bloc))
            .collect());
    }
    Err(format!(
        "pins GetIdArray: encodage inconnu, valeur brute: {texte}"
    ))
}

/// Décode la valeur JSON que `ReadList` rend dans son argument `List`.
pub fn parse_pin_list(json: &str, ids: &[u32]) -> Result<Vec<DevicePin>, String> {
    let texte = quick_xml::escape::unescape(json)
        .map(|decode| decode.into_owned())
        .unwrap_or_else(|_| json.to_string());
    let texte = texte.trim();
    if texte.is_empty() {
        return Ok(Vec::new());
    }
    let brut: Vec<serde_json::Value> = serde_json::from_str(texte)
        .map_err(|erreur| format!("pins ReadList: liste JSON illisible ({erreur}): {texte}"))?;
    Ok(brut
        .iter()
        .enumerate()
        .map(|(rang, objet)| {
            let id = objet
                .get("id")
                .and_then(|valeur| valeur.as_u64())
                .map(|valeur| valeur as u32)
                .unwrap_or_else(|| ids.get(rang).copied().unwrap_or(0));
            let index = ids.iter().position(|connu| *connu == id).unwrap_or(rang);
            DevicePin {
                id,
                index,
                mode: chaine(objet, &["mode"]),
                pin_type: chaine(objet, &["type"]),
                uri: chaine(objet, &["uri"]),
                title: chaine(objet, &["title"]),
                description: chaine(objet, &["description"]),
                artwork_uri: chaine(objet, &["artworkUri", "artwork_uri"]),
                shuffle: objet
                    .get("shuffle")
                    .and_then(|valeur| valeur.as_bool())
                    .unwrap_or(false),
            }
        })
        .collect())
}

fn chaine(objet: &serde_json::Value, cles: &[&str]) -> String {
    for cle in cles {
        if let Some(texte) = objet.get(*cle).and_then(|valeur| valeur.as_str()) {
            return texte.to_string();
        }
    }
    String::new()
}

/// Base64 standard, sans dépendance nouvelle : la caisse `base64` n'est pas
/// dans l'arbre et une dépendance ajoutée pour quinze lignes se paierait sur
/// les trois plateformes.
fn decode_base64(texte: &str) -> Option<Vec<u8>> {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut valeurs: Vec<u32> = Vec::new();
    for octet in texte.bytes() {
        if octet.is_ascii_whitespace() {
            continue;
        }
        if octet == b'=' {
            break;
        }
        let rang = TABLE.iter().position(|attendu| *attendu == octet)? as u32;
        valeurs.push(rang);
    }
    let mut octets = Vec::with_capacity(valeurs.len() * 3 / 4);
    for groupe in valeurs.chunks(4) {
        if groupe.len() < 2 {
            return None;
        }
        let mut accumulateur = 0u32;
        for (rang, valeur) in groupe.iter().enumerate() {
            accumulateur |= valeur << (18 - 6 * rang as u32);
        }
        let utiles = groupe.len() - 1;
        for rang in 0..utiles {
            octets.push(((accumulateur >> (16 - 8 * rang)) & 0xFF) as u8);
        }
    }
    Some(octets)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_array_decimal_separe_par_virgules() {
        assert_eq!(parse_id_array("11,12,13").unwrap(), vec![11, 12, 13]);
        assert_eq!(parse_id_array(" 4 5 ").unwrap(), vec![4, 5]);
        assert_eq!(parse_id_array("").unwrap(), Vec::<u32>::new());
    }

    #[test]
    fn id_array_base64_de_u32_gros_boutiens() {
        // [7, 258] en gros-boutien -> 00 00 00 07 00 00 01 02
        let encode = "AAAABwAAAQI=";
        assert_eq!(parse_id_array(encode).unwrap(), vec![7, 258]);
    }

    #[test]
    fn id_array_illisible_remonte_la_valeur_brute_au_lieu_de_vider() {
        let erreur = parse_id_array("**pas un tableau**").unwrap_err();
        assert!(
            erreur.contains("**pas un tableau**"),
            "l'erreur doit porter la valeur brute, obtenu: {erreur}"
        );
    }

    #[test]
    fn read_list_lit_ce_que_l_appareil_decrit() {
        let json = r#"[{"id":12,"mode":"tidal","type":"playlist","uri":"tidal://x",
            "title":"Publie par l appareil","description":"d","artworkUri":"a","shuffle":true}]"#;
        let pins = parse_pin_list(json, &[11, 12]).unwrap();
        assert_eq!(pins.len(), 1);
        assert_eq!(pins[0].id, 12);
        assert_eq!(pins[0].index, 1, "le rang vient de la place dans l'IdArray");
        assert_eq!(pins[0].title, "Publie par l appareil");
        assert!(pins[0].shuffle);
    }

    #[test]
    fn read_list_accepte_le_json_echappe_du_xml() {
        let json = "[{&quot;id&quot;:3,&quot;title&quot;:&quot;Echappe&quot;}]";
        let pins = parse_pin_list(json, &[3]).unwrap();
        assert_eq!(pins[0].title, "Echappe");
    }
}
