//! Résultats d'analyse partagés par la communauté (#5594, étape 2 côté Tune) :
//! ReplayGain, plage dynamique et empreinte CLAP, échangés avec mozaiklabs.fr
//! par la clé PCM exacte des FLAC.
//!
//! Le contrat est celui de `renesenses/site-mozaiklabs#338`
//! (`/api/v1/community/measures`) :
//!
//! - `POST /measures` — dépôt d'au plus 500 éléments, chacun une clé
//!   `audio_pcm_key` et au moins un bloc `rg`, `dr` ou `clap`, chaque bloc
//!   avec EXACTEMENT ses champs et `source = "analysis"` ;
//! - `GET /measures?prefix=<2 à 4 hex>[&kinds=…][&after=…]` — lecture d'un
//!   SEAU : le site ne sait pas quel signal on cherche, le client compare en
//!   local ;
//! - `POST /measures/erase` — effacement de tout ce que l'installation a
//!   déposé (RGPD), qui marche même interrupteur du site fermé.
//!
//! ## Rien ne part sans option explicite
//!
//! Deux réglages, **désactivés par défaut**, écrits par le client web
//! (`PATCH /system/config`) et publiés avec leur défaut par
//! `GET /system/config` :
//!
//! - [`CLE_RECEPTION`] « recevoir » : la passe interroge le site par seau et
//!   comble les vides locaux ;
//! - [`CLE_CONTRIBUTION`] « contribuer » : la passe envoie les seules valeurs
//!   MESURÉES par cette installation. Un refus de télémétrie l'arrête aussi
//!   (#3383 : un refus, d'où qu'il vienne, reste souverain).
//!
//! Sans l'un ni l'autre, [`passe`] ne fait AUCUN appel réseau : ni dépôt, ni
//! lecture. Le témoin `rien_ne_part_sans_option_explicite` le garde.
//!
//! ## Ce qui part, et ce qui ne part jamais
//!
//! Un dépôt porte l'`instance_id` de l'installation — c'est le contrat du
//! site, comme `community/library/*` ; le site n'en garde qu'un HMAC — et,
//! par piste, la clé PCM et les valeurs. Jamais : chemin, titre, artiste,
//! album, nom de fichier, compte, profil, identifiant de bibliothèque. La
//! requête qui choisit les pistes ([`SQL_A_ENVOYER`]) ne lit aucune de ces
//! colonnes : ce qui n'est pas lu ne peut pas partir. Une lecture ne porte
//! qu'un préfixe de deux chiffres hexadécimaux et les genres demandés.
//!
//! Seules partent les mesures de Tune : `rg_track_source = analysis` avec
//! `rg_algo = RG_ALGO`, `dr_source = analysis` avec `dr_algo = DR_ALGO`, et
//! l'empreinte du modèle courant analysée ici (`source` nulle : ni héritée
//! d'une jumelle, ni reçue de la communauté). Une valeur reçue ne repart
//! jamais.
//!
//! ## Ce qui est reçu ne remplace rien
//!
//! Une valeur reçue ne comble qu'un VIDE : pas de `rg_track_gain` (tag ou
//! mesure), pas de `dr_track`, pas d'empreinte du modèle courant. Elle est
//! marquée `community` (`rg_track_source`, `dr_source`, `source` de
//! l'empreinte) et porte la version de l'algorithme qui l'a produite. Seules
//! sont retenues les lignes de NOTRE version d'algorithme, et celles qu'au
//! moins [`QUORUM_MIN`] installations ont fournies (le site l'impose déjà ;
//! le client le revérifie). Le gain arrive avec SA crête d'échantillon, la
//! plus haute du groupe : l'anti-écrêtage de la lecture s'y applique comme à
//! une mesure locale.
//!
//! ## Retrait du consentement « contribuer »
//!
//! Dès qu'une contribution a été acceptée ([`CLE_CONTRIBUTION_TRANSMISE`]),
//! couper l'option fait appeler l'effacement au passage suivant ; tant que le
//! site ne l'a pas accepté, il est retenté.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use serde_json::{Value, json};
use tracing::{debug, info, warn};

use crate::audio::embedding_store::{EMBED_DIM, MODEL_ID, to_bytes};
use crate::audio::replaygain::{DR_ALGO, RG_ALGO, format_gain, format_peak};
use crate::cloud::consent::est_vrai;
use crate::cloud::rate_limit::{self, AppelCloud, CloudScope};
use crate::db::backend::{DbBackend, SqlValue, ToSqlValue};
use crate::db::settings_repo::SettingsRepo;
use crate::db::track_metadata_repo::TrackMetadataRepo;

/// « Recevoir les résultats d'analyse de la communauté ».
pub const CLE_RECEPTION: &str = "community_measures_receive_enabled";
/// Désactivé par défaut : opt-in strict.
pub const RECEPTION_DEFAUT: bool = false;
/// « Partager mes résultats d'analyse avec la communauté ».
pub const CLE_CONTRIBUTION: &str = "community_measures_contribute_enabled";
/// Désactivé par défaut : opt-in strict.
pub const CONTRIBUTION_DEFAUT: bool = false;

/// Une contribution a été acceptée par le site, et n'a pas été effacée
/// depuis : c'est ce qui fait appeler l'effacement au retrait de l'option.
pub const CLE_CONTRIBUTION_TRANSMISE: &str = "community_measures_contributed";
/// Curseur de la passe d'envoi (`tracks.id`), remis à zéro en fin de tour.
pub const CLE_CURSEUR_ENVOI: &str = "community_measures_upload_cursor";
/// Dernier seau lu (deux chiffres hexadécimaux), pour reprendre au suivant.
pub const CLE_CURSEUR_SEAU: &str = "community_measures_bucket_cursor";
/// Nombre de valeurs reçues et appliquées depuis l'activation.
pub const CLE_RECUES: &str = "community_measures_received";
/// Nombre de valeurs acceptées par le site depuis l'activation.
pub const CLE_ENVOYEES: &str = "community_measures_sent";

/// Par piste, les genres déjà envoyés avec leur version
/// (`rg:bs1770-tp4x-v2,dr:tt-dr-v1,clap:…`) : une piste n'est renvoyée que
/// lorsqu'une mesure de la version courante lui manque encore au site.
pub const CLE_PISTE_ENVOYEE: &str = "community_measures_sent_kinds";

/// La marque de provenance d'une valeur reçue.
pub const SOURCE_COMMUNAUTE: &str = "community";

/// Préfixe des routes, sous la racine du site.
pub const CHEMIN_API: &str = "/api/v1/community/measures";
/// Racine du site quand `mozaik_base_url` ne la redirige pas.
pub const RACINE_PAR_DEFAUT: &str = "https://mozaiklabs.fr";

/// Éléments par dépôt (le site en accepte 500 au plus).
pub const TAILLE_DU_LOT: i64 = 500;
/// Dépôts au plus par passage.
pub const LOTS_PAR_PASSE: usize = 10;
/// Longueur du préfixe demandé : deux chiffres, le seau le plus large que le
/// site sert. 256 seaux couvrent tout ; le site ne voit qu'un 256e.
pub const LONGUEUR_DU_PREFIXE: usize = 2;
/// Seaux lus au plus par passage.
pub const SEAUX_PAR_PASSE: usize = 16;
/// Pages lues au plus par seau et par passage.
pub const PAGES_PAR_SEAU: usize = 20;
/// Le quorum que le site applique (deux installations concordantes,
/// décision de Bertrand du 12/10) : le client ne retient rien en dessous.
pub const QUORUM_MIN: i64 = 2;
/// Cadence de la tâche de fond.
pub const CADENCE_SECONDES: u64 = 1800;

const PREFIXE_CLE: &str = "flac-md5-v1:";

/// Lit un booléen de `settings`. Absent, illisible ou autre : `defaut`.
fn option(settings: &SettingsRepo, cle: &str, defaut: bool) -> bool {
    settings
        .get(cle)
        .ok()
        .flatten()
        .map(|v| est_vrai(&v))
        .unwrap_or(defaut)
}

/// L'utilisateur a-t-il choisi de RECEVOIR ?
pub fn reception_activee(settings: &SettingsRepo) -> bool {
    option(settings, CLE_RECEPTION, RECEPTION_DEFAUT)
}

/// L'utilisateur a-t-il choisi de CONTRIBUER, et la télémétrie n'est-elle pas
/// refusée ?
pub fn contribution_activee(settings: &SettingsRepo) -> bool {
    crate::cloud::telemetry::TelemetryReporter::is_enabled_for(settings)
        && option(settings, CLE_CONTRIBUTION, CONTRIBUTION_DEFAUT)
}

/// La racine du site : `mozaik_base_url` s'il est réglé, sinon
/// [`RACINE_PAR_DEFAUT`].
fn racine(settings: &SettingsRepo) -> String {
    settings
        .get("mozaik_base_url")
        .ok()
        .flatten()
        .map(|v| v.trim().trim_end_matches('/').to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| RACINE_PAR_DEFAUT.to_string())
}

/// La clé est-elle une clé PCM que le site accepte ? Même règle que sa regex
/// (`CommunityMeasureKeys::PCM_KEY_REGEX`) : MD5 de 32 chiffres minuscules,
/// non nul, échantillons, fréquence, canaux 1 à 8, résolution 4 à 32 bits.
pub fn cle_pcm_valide(cle: &str) -> bool {
    let Some(reste) = cle.strip_prefix(PREFIXE_CLE) else {
        return false;
    };
    let parts: Vec<&str> = reste.split(':').collect();
    let [md5, echantillons, frequence, canaux, bits] = parts.as_slice() else {
        return false;
    };
    let entier_positif = |s: &str, max_chiffres: usize| {
        !s.is_empty()
            && s.len() <= max_chiffres
            && !s.starts_with('0')
            && s.bytes().all(|b| b.is_ascii_digit())
    };
    md5.len() == 32
        && md5.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        && md5.bytes().any(|b| b != b'0')
        && entier_positif(echantillons, 12)
        && entier_positif(frequence, 7)
        && matches!(canaux.parse::<u8>(), Ok(1..=8))
        && canaux.len() == 1
        && entier_positif(bits, 2)
        && matches!(bits.parse::<u8>(), Ok(4..=32))
}

/// Le seau d'une clé : les [`LONGUEUR_DU_PREFIXE`] premiers chiffres du MD5.
fn seau(cle: &str) -> Option<&str> {
    cle.strip_prefix(PREFIXE_CLE)
        .and_then(|r| r.get(..LONGUEUR_DU_PREFIXE))
}

/// Ce qu'un passage a fait.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Bilan {
    /// Requêtes HTTP émises (dépôt, lecture, effacement).
    pub appels: usize,
    /// Valeurs acceptées par le site.
    pub envoyees: i64,
    /// Valeurs reçues et appliquées en local.
    pub recues: i64,
    /// L'effacement a été accepté.
    pub efface: bool,
    /// Un appel a échoué (rien n'a avancé, il repartira).
    pub echec: Option<String>,
}

enum Reponse {
    Ok(Value),
    /// L'interrupteur du site est fermé (503 `community_measures_disabled`).
    Ferme,
    Echec(String),
}

async fn appeler(
    settings: &SettingsRepo,
    portee: CloudScope,
    requete: reqwest::RequestBuilder,
) -> Reponse {
    let requete = requete
        .header(reqwest::header::ACCEPT, "application/json")
        .timeout(Duration::from_secs(30));
    match rate_limit::appeler(settings, portee, requete).await {
        AppelCloud::Retenu(_) => Reponse::Echec("rate_limited".into()),
        AppelCloud::Erreur(e) => Reponse::Echec(format!("network: {e}")),
        AppelCloud::Reponse(r) if r.status().is_success() => {
            Reponse::Ok(r.json().await.unwrap_or(Value::Null))
        }
        AppelCloud::Reponse(r) => {
            let statut = r.status().as_u16();
            let corps: Value = r.json().await.unwrap_or(Value::Null);
            if statut == 503 && corps["error"] == "community_measures_disabled" {
                Reponse::Ferme
            } else {
                Reponse::Echec(format!("HTTP {statut}"))
            }
        }
    }
}

// ── Envoi ──────────────────────────────────────────────────────────────────

/// Les pistes qui ont une mesure de la version courante à envoyer. Ne lit
/// AUCUNE colonne d'identité (chemin, titre, artiste) : l'identifiant local,
/// la clé PCM, les valeurs et leurs marques, l'empreinte analysée ici.
///
/// Paramètres, dans l'ordre : modèle (jointure), curseur, modèle (filtre),
/// limite.
const SQL_A_ENVOYER: &str = "SELECT t.id, t.audio_pcm_key, \
       (SELECT value FROM track_metadata WHERE track_id = t.id AND key = 'rg_track_gain'), \
       (SELECT value FROM track_metadata WHERE track_id = t.id AND key = 'rg_track_peak'), \
       (SELECT value FROM track_metadata WHERE track_id = t.id AND key = 'rg_track_source'), \
       (SELECT value FROM track_metadata WHERE track_id = t.id AND key = 'rg_algo'), \
       (SELECT value FROM track_metadata WHERE track_id = t.id AND key = 'dr_track'), \
       (SELECT value FROM track_metadata WHERE track_id = t.id AND key = 'dr_source'), \
       (SELECT value FROM track_metadata WHERE track_id = t.id AND key = 'dr_algo'), \
       e.embedding, \
       (SELECT value FROM track_metadata WHERE track_id = t.id \
              AND key = 'community_measures_sent_kinds') \
     FROM tracks t \
     LEFT JOIN track_audio_embedding e \
            ON e.track_id = t.id AND e.model = ? AND e.source IS NULL \
     WHERE t.audio_pcm_key LIKE 'flac-md5-v1:%' AND t.id > ? \
       AND (EXISTS (SELECT 1 FROM track_metadata m WHERE m.track_id = t.id \
                    AND m.key = 'rg_track_source' AND m.value = 'analysis') \
         OR EXISTS (SELECT 1 FROM track_metadata m WHERE m.track_id = t.id \
                    AND m.key = 'dr_source' AND m.value = 'analysis') \
         OR EXISTS (SELECT 1 FROM track_audio_embedding x WHERE x.track_id = t.id \
                    AND x.model = ? AND x.source IS NULL)) \
     ORDER BY t.id LIMIT ?";

fn texte(r: &[SqlValue], i: usize) -> Option<String> {
    r.get(i).and_then(|v| v.as_string())
}

/// Le bloc `rg`, seulement pour une mesure de Tune de la version courante,
/// dans les bornes du contrat.
fn bloc_rg(
    gain: Option<String>,
    crete: Option<String>,
    source: Option<String>,
    algo: Option<String>,
) -> Option<Value> {
    if source.as_deref().map(str::trim) != Some("analysis")
        || algo.as_deref().map(str::trim) != Some(RG_ALGO)
    {
        return None;
    }
    let gain = crate::audio::replaygain::parse_gain_db(gain?)?;
    let crete = crete?.trim().parse::<f64>().ok()?;
    if !gain.is_finite() || !(-64.0..=64.0).contains(&gain) {
        return None;
    }
    if !crete.is_finite() || crete <= 0.0 || crete > 2.0 {
        return None;
    }
    Some(json!({
        "algo": RG_ALGO,
        "source": "analysis",
        "track_gain_db": gain,
        "track_peak": crete,
    }))
}

fn bloc_dr(valeur: Option<String>, source: Option<String>, algo: Option<String>) -> Option<Value> {
    if source.as_deref().map(str::trim) != Some("analysis")
        || algo.as_deref().map(str::trim) != Some(DR_ALGO)
    {
        return None;
    }
    let dr = valeur?.trim().parse::<i64>().ok()?;
    (0..=99).contains(&dr).then(|| {
        json!({
            "algo": DR_ALGO,
            "source": "analysis",
            "value": dr,
        })
    })
}

/// 512 flottants finis, de norme non nulle — la règle du site.
fn vecteur_valide(octets: &[u8]) -> bool {
    if octets.len() != EMBED_DIM * 4 {
        return false;
    }
    let v = crate::audio::embedding_store::from_bytes(octets);
    v.iter().all(|x| x.is_finite()) && v.iter().map(|x| x * x).sum::<f32>() > 0.0
}

fn bloc_clap(octets: Option<&[u8]>) -> Option<Value> {
    let octets = octets?;
    vecteur_valide(octets).then(|| {
        json!({
            "model_id": MODEL_ID,
            "source": "analysis",
            "embedding": base64::engine::general_purpose::STANDARD.encode(octets),
        })
    })
}

/// Les genres d'un élément, avec leur version : la marque posée après un
/// dépôt accepté.
fn genres_de(element: &Value) -> String {
    let mut g = Vec::new();
    if element.get("rg").is_some() {
        g.push(format!("rg:{RG_ALGO}"));
    }
    if element.get("dr").is_some() {
        g.push(format!("dr:{DR_ALGO}"));
    }
    if element.get("clap").is_some() {
        g.push(format!("clap:{MODEL_ID}"));
    }
    g.join(",")
}

/// Un lot à déposer : `(track_id, élément)` et le dernier `tracks.id` lu.
fn lot_a_envoyer(backend: &Arc<dyn DbBackend>, apres: i64) -> (Vec<(i64, Value)>, Option<i64>) {
    let rows = match backend.query_many(
        SQL_A_ENVOYER,
        &[
            &MODEL_ID as &dyn ToSqlValue,
            &apres as &dyn ToSqlValue,
            &MODEL_ID as &dyn ToSqlValue,
            &TAILLE_DU_LOT as &dyn ToSqlValue,
        ],
    ) {
        Ok(r) => r,
        Err(e) => {
            warn!(error = %e, "community_measures_select_failed");
            return (Vec::new(), None);
        }
    };
    let mut dernier = None;
    let mut lot = Vec::new();
    for r in &rows {
        let Some(id) = r.first().and_then(|v| v.as_i64()) else {
            continue;
        };
        dernier = Some(id);
        let Some(cle) = texte(r, 1).filter(|c| cle_pcm_valide(c)) else {
            continue;
        };
        let mut element = serde_json::Map::new();
        element.insert("audio_pcm_key".into(), json!(cle));
        if let Some(b) = bloc_rg(texte(r, 2), texte(r, 3), texte(r, 4), texte(r, 5)) {
            element.insert("rg".into(), b);
        }
        if let Some(b) = bloc_dr(texte(r, 6), texte(r, 7), texte(r, 8)) {
            element.insert("dr".into(), b);
        }
        if let Some(b) = bloc_clap(r.get(9).and_then(|v| v.as_blob())) {
            element.insert("clap".into(), b);
        }
        let element = Value::Object(element);
        let genres = genres_de(&element);
        // Rien de mesurable, ou tout déjà envoyé dans cette version.
        if genres.is_empty() || texte(r, 10).as_deref() == Some(genres.as_str()) {
            continue;
        }
        lot.push((id, element));
    }
    (lot, dernier)
}

async fn envoyer(
    backend: &Arc<dyn DbBackend>,
    http: &reqwest::Client,
    settings: &SettingsRepo,
    instance_id: &str,
    bilan: &mut Bilan,
) {
    let url = format!("{}{CHEMIN_API}", racine(settings));
    let meta = TrackMetadataRepo::with_backend(backend.clone());
    for _ in 0..LOTS_PAR_PASSE {
        let apres = settings
            .get(CLE_CURSEUR_ENVOI)
            .ok()
            .flatten()
            .and_then(|v| v.trim().parse::<i64>().ok())
            .unwrap_or(0);
        let (lot, dernier) = lot_a_envoyer(backend, apres);
        let Some(dernier) = dernier else {
            // Fin du tour : le suivant repart du début.
            let _ = settings.set(CLE_CURSEUR_ENVOI, "0");
            return;
        };
        if lot.is_empty() {
            let _ = settings.set(CLE_CURSEUR_ENVOI, &dernier.to_string());
            continue;
        }
        let elements: Vec<&Value> = lot.iter().map(|(_, e)| e).collect();
        let corps = json!({ "instance_id": instance_id, "measures": elements });
        bilan.appels += 1;
        match appeler(
            settings,
            CloudScope::CommunityMeasuresWrite,
            http.post(&url).json(&corps),
        )
        .await
        {
            Reponse::Ok(r) => {
                let acceptees = r["accepted"].as_i64().unwrap_or(0);
                bilan.envoyees += acceptees;
                for (id, element) in &lot {
                    let _ = meta.set(*id, CLE_PISTE_ENVOYEE, &genres_de(element));
                }
                let _ = settings.set(CLE_CURSEUR_ENVOI, &dernier.to_string());
                let _ = settings.set(CLE_CONTRIBUTION_TRANSMISE, "true");
                ajouter(settings, CLE_ENVOYEES, acceptees);
            }
            Reponse::Ferme => {
                debug!("community_measures_site_disabled");
                return;
            }
            Reponse::Echec(e) => {
                warn!(raison = %e, "community_measures_upload_failed");
                bilan.echec = Some(e);
                return;
            }
        }
    }
}

fn ajouter(settings: &SettingsRepo, cle: &str, n: i64) {
    if n <= 0 {
        return;
    }
    let avant = settings
        .get(cle)
        .ok()
        .flatten()
        .and_then(|v| v.trim().parse::<i64>().ok())
        .unwrap_or(0);
    let _ = settings.set(cle, &(avant + n).to_string());
}

async fn effacer(
    backend: &Arc<dyn DbBackend>,
    http: &reqwest::Client,
    settings: &SettingsRepo,
    instance_id: &str,
    bilan: &mut Bilan,
) {
    let url = format!("{}{CHEMIN_API}/erase", racine(settings));
    bilan.appels += 1;
    match appeler(
        settings,
        CloudScope::CommunityMeasuresWrite,
        http.post(&url).json(&json!({ "instance_id": instance_id })),
    )
    .await
    {
        Reponse::Ok(r) => {
            info!(
                contributions = r["contributions"].as_i64().unwrap_or(0),
                "community_measures_erased"
            );
            let _ = settings.set(CLE_CONTRIBUTION_TRANSMISE, "false");
            let _ = settings.set(CLE_CURSEUR_ENVOI, "0");
            // Réactiver plus tard renverra tout : le site n'a plus rien.
            let _ = backend.execute(
                "DELETE FROM track_metadata WHERE key = 'community_measures_sent_kinds'",
                &[],
            );
            bilan.efface = true;
        }
        Reponse::Ferme => {}
        Reponse::Echec(e) => {
            warn!(raison = %e, "community_measures_erase_failed");
            bilan.echec = Some(e);
        }
    }
}

// ── Réception ──────────────────────────────────────────────────────────────

/// Les besoins locaux par seau : `(seau, rg, dr, clap)`, un genre étant
/// demandé dès qu'une piste du seau en manque.
fn seaux_a_lire(backend: &Arc<dyn DbBackend>) -> Vec<(String, bool, bool, bool)> {
    let sql = "SELECT substr(t.audio_pcm_key, 13, 2), \
          MAX(CASE WHEN NOT EXISTS (SELECT 1 FROM track_metadata m WHERE m.track_id = t.id \
                AND m.key = 'rg_track_gain') THEN 1 ELSE 0 END), \
          MAX(CASE WHEN NOT EXISTS (SELECT 1 FROM track_metadata m WHERE m.track_id = t.id \
                AND m.key = 'dr_track' AND m.value != '') THEN 1 ELSE 0 END), \
          MAX(CASE WHEN NOT EXISTS (SELECT 1 FROM track_metadata m WHERE m.track_id = t.id \
                AND m.key = 'audio_embed_analyzed' AND m.value = ?) THEN 1 ELSE 0 END) \
        FROM tracks t WHERE t.audio_pcm_key LIKE 'flac-md5-v1:%' \
        GROUP BY substr(t.audio_pcm_key, 13, 2) ORDER BY 1";
    backend
        .query_many(sql, &[&MODEL_ID as &dyn ToSqlValue])
        .unwrap_or_default()
        .iter()
        .filter_map(|r| {
            let s = texte(r, 0)?;
            let besoin = |i: usize| r.get(i).and_then(|v| v.as_i64()).unwrap_or(0) > 0;
            (s.len() == LONGUEUR_DU_PREFIXE && s.bytes().all(|b| b.is_ascii_hexdigit()))
                .then(|| (s, besoin(1), besoin(2), besoin(3)))
        })
        .filter(|(_, rg, dr, clap)| *rg || *dr || *clap)
        .collect()
}

/// Les pistes locales d'un seau, par clé PCM.
fn pistes_du_seau(backend: &Arc<dyn DbBackend>, seau: &str) -> HashMap<String, Vec<i64>> {
    let motif = format!("{PREFIXE_CLE}{seau}%");
    let mut par_cle: HashMap<String, Vec<i64>> = HashMap::new();
    for r in backend
        .query_many(
            "SELECT id, audio_pcm_key FROM tracks WHERE audio_pcm_key LIKE ?",
            &[&motif as &dyn ToSqlValue],
        )
        .unwrap_or_default()
    {
        if let (Some(id), Some(cle)) = (r.first().and_then(|v| v.as_i64()), texte(&r, 1)) {
            par_cle.entry(cle).or_default().push(id);
        }
    }
    par_cle
}

/// Applique UNE ligne reçue à une piste locale. Rend le nombre de valeurs
/// écrites (0 ou 1). Ne comble qu'un vide ; ne retient que notre version.
pub fn appliquer(backend: &Arc<dyn DbBackend>, track_id: i64, ligne: &Value) -> i64 {
    if ligne["installations"].as_i64().unwrap_or(0) < QUORUM_MIN {
        return 0;
    }
    let meta = TrackMetadataRepo::with_backend(backend.clone());
    let existant = meta.get_all(track_id).unwrap_or_default();
    match (ligne["kind"].as_str(), ligne["algo"].as_str()) {
        (Some("rg"), Some(RG_ALGO)) => {
            let (Some(gain), Some(crete)) = (
                ligne["track_gain_db"].as_f64(),
                ligne["track_peak"].as_f64(),
            ) else {
                return 0;
            };
            if !gain.is_finite()
                || !(-64.0..=64.0).contains(&gain)
                || !crete.is_finite()
                || crete <= 0.0
                || crete > 2.0
                || existant.contains_key("rg_track_gain")
            {
                return 0;
            }
            let mut champs = HashMap::new();
            champs.insert("rg_track_gain".to_string(), format_gain(gain));
            champs.insert("rg_track_peak".to_string(), format_peak(crete));
            champs.insert(
                crate::audio::replaygain::TRACK_SOURCE_KEY.to_string(),
                SOURCE_COMMUNAUTE.to_string(),
            );
            champs.insert(
                crate::audio::replaygain::RG_ALGO_KEY.to_string(),
                RG_ALGO.to_string(),
            );
            i64::from(meta.set_batch(track_id, &champs).is_ok())
        }
        (Some("dr"), Some(DR_ALGO)) => {
            let Some(dr) = ligne["dr"].as_i64().filter(|d| (0..=99).contains(d)) else {
                return 0;
            };
            if !crate::audio::replaygain::peut_ecrire_le_dr(
                existant.get("dr_track").map(String::as_str),
            ) {
                return 0;
            }
            let mut champs = HashMap::new();
            champs.insert("dr_track".to_string(), dr.to_string());
            champs.insert("dr_source".to_string(), SOURCE_COMMUNAUTE.to_string());
            champs.insert(
                crate::audio::replaygain::DR_ALGO_KEY.to_string(),
                DR_ALGO.to_string(),
            );
            i64::from(meta.set_batch(track_id, &champs).is_ok())
        }
        (Some("clap"), Some(MODEL_ID)) => {
            if existant.get("audio_embed_analyzed").map(String::as_str) == Some(MODEL_ID) {
                return 0;
            }
            let Some(octets) = ligne["embedding"]
                .as_str()
                .and_then(|b| base64::engine::general_purpose::STANDARD.decode(b).ok())
                .filter(|o| vecteur_valide(o))
            else {
                return 0;
            };
            // Vecteur normalisé, comme ceux de la passe : le classement par
            // produit scalaire suppose des normes unitaires.
            let mut v = crate::audio::embedding_store::from_bytes(&octets);
            let norme = v.iter().map(|x| x * x).sum::<f32>().sqrt();
            v.iter_mut().for_each(|x| *x /= norme);
            let blob = Some(to_bytes(&v));
            let maintenant = chrono::Utc::now().timestamp();
            let ecrit = backend.execute(
                "INSERT INTO track_audio_embedding (track_id, model, embedding, analyzed_at, source) \
                 VALUES (?, ?, ?, ?, ?) ON CONFLICT (track_id) DO NOTHING",
                &[
                    &track_id as &dyn ToSqlValue,
                    &MODEL_ID as &dyn ToSqlValue,
                    &blob as &dyn ToSqlValue,
                    &maintenant as &dyn ToSqlValue,
                    &SOURCE_COMMUNAUTE as &dyn ToSqlValue,
                ],
            );
            match ecrit {
                Ok(1) => {
                    let _ = meta.set(track_id, "audio_embed_analyzed", MODEL_ID);
                    1
                }
                _ => 0,
            }
        }
        _ => 0,
    }
}

async fn recevoir(
    backend: &Arc<dyn DbBackend>,
    http: &reqwest::Client,
    settings: &SettingsRepo,
    bilan: &mut Bilan,
) {
    let tous = seaux_a_lire(backend);
    if tous.is_empty() {
        return;
    }
    let curseur = settings
        .get(CLE_CURSEUR_SEAU)
        .ok()
        .flatten()
        .unwrap_or_default();
    // Les seaux après le curseur, puis le début : un tour complet se fait en
    // plusieurs passages, sans jamais relire deux fois le même d'affilée.
    let (apres, avant): (Vec<_>, Vec<_>) = tous
        .into_iter()
        .partition(|(s, ..)| s.as_str() > curseur.as_str());
    let url = format!("{}{CHEMIN_API}", racine(settings));
    for (s, rg, dr, clap) in apres.into_iter().chain(avant).take(SEAUX_PAR_PASSE) {
        let genres: Vec<&str> = [(rg, "rg"), (dr, "dr"), (clap, "clap")]
            .into_iter()
            .filter_map(|(b, g)| b.then_some(g))
            .collect();
        let locales = pistes_du_seau(backend, &s);
        let mut suite: Option<String> = None;
        for _ in 0..PAGES_PAR_SEAU {
            let mut requete = vec![("prefix", s.clone()), ("kinds", genres.join(","))];
            if let Some(a) = &suite {
                requete.push(("after", a.clone()));
            }
            bilan.appels += 1;
            match appeler(
                settings,
                CloudScope::CommunityMeasuresRead,
                http.get(&url).query(&requete),
            )
            .await
            {
                Reponse::Ok(page) => {
                    for ligne in page["measures"].as_array().into_iter().flatten() {
                        let Some(cle) = ligne["audio_pcm_key"].as_str() else {
                            continue;
                        };
                        // Le seau entier arrive : on compare EN LOCAL.
                        if seau(cle) != Some(s.as_str()) {
                            continue;
                        }
                        for id in locales.get(cle).into_iter().flatten() {
                            let n = appliquer(backend, *id, ligne);
                            bilan.recues += n;
                        }
                    }
                    suite = page["next"]
                        .as_str()
                        .map(str::to_string)
                        .or_else(|| page["next"].as_i64().map(|n| n.to_string()));
                    if suite.is_none() {
                        break;
                    }
                }
                Reponse::Ferme => {
                    debug!("community_measures_site_disabled");
                    return;
                }
                Reponse::Echec(e) => {
                    warn!(raison = %e, "community_measures_lookup_failed");
                    bilan.echec = Some(e);
                    return;
                }
            }
        }
        let _ = settings.set(CLE_CURSEUR_SEAU, &s);
    }
}

/// Un passage : effacement dû, envoi, réception — chacun sous SON option.
/// Sans option, aucun appel.
pub async fn passe(backend: &Arc<dyn DbBackend>, http: &reqwest::Client) -> Bilan {
    let settings = SettingsRepo::with_backend(backend.clone());
    let mut bilan = Bilan::default();
    let contribuer = contribution_activee(&settings);
    let a_effacer = !contribuer && option(&settings, CLE_CONTRIBUTION_TRANSMISE, false);
    let recevoir_ = reception_activee(&settings);
    if !contribuer && !a_effacer && !recevoir_ {
        return bilan;
    }
    let instance_id = settings
        .get("instance_id")
        .ok()
        .flatten()
        .map(|v| v.trim().to_string())
        .unwrap_or_default();
    if a_effacer && !instance_id.is_empty() {
        effacer(backend, http, &settings, &instance_id, &mut bilan).await;
    }
    if contribuer && !instance_id.is_empty() {
        envoyer(backend, http, &settings, &instance_id, &mut bilan).await;
    }
    if recevoir_ {
        recevoir(backend, http, &settings, &mut bilan).await;
        ajouter(&settings, CLE_RECUES, bilan.recues);
    }
    bilan
}

/// La tâche de fond : un passage toutes les [`CADENCE_SECONDES`], cinq
/// minutes après le démarrage. Chaque passage relit les options : les
/// activer ou les couper prend effet sans redémarrage.
pub fn spawn(backend: Arc<dyn DbBackend>, http: reqwest::Client) {
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(300)).await;
        let mut ticker = tokio::time::interval(Duration::from_secs(CADENCE_SECONDES));
        loop {
            ticker.tick().await;
            let b = passe(&backend, &http).await;
            if b.appels > 0 {
                info!(
                    appels = b.appels,
                    envoyees = b.envoyees,
                    recues = b.recues,
                    efface = b.efface,
                    echec = b.echec.as_deref().unwrap_or(""),
                    "community_measures_pass"
                );
            }
        }
    });
}

#[cfg(test)]
#[path = "mesures_communautaires_tests.rs"]
mod tests;
