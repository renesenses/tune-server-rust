//! « Mes écoutes, tous serveurs » (#6510) : l'envoi des écoutes de CE serveur
//! au compte mozaiklabs.fr qui le possède.
//!
//! Le site additionne les écoutes de tous les serveurs d'un compte, de tous
//! leurs profils, et les montre sur la page « Mes écoutes ». Ce module en est
//! la moitié serveur : il lit `listen_history` (la table des statistiques
//! d'écoute locales) et la pousse par lots.
//!
//! ## Rien ne part sans DEUX conditions
//!
//! 1. le serveur est relié à un compte : session SSO (`mozaik_access_token`),
//!    `server_id` et jeton de liaison (`cloud_server_link_token`). C'est la
//!    chaîne de la copie de bibliothèque et de Tune Circle, prise telle
//!    quelle : [`super::sauvegarde_config::compte`] pour la lire,
//!    [`super::library_sync::lier_le_serveur`] pour relier une fois ;
//! 2. le réglage [`CLE_ACTIVATION`] vaut vrai. **Désactivé par défaut** : sans
//!    geste de l'utilisateur, rien ne part, jamais.
//!
//! ## Ce qui part, et ce qui ne part jamais
//!
//! Par écoute : un identifiant d'événement opaque ([`identifiant_d_evenement`],
//! la clé du dédoublonnage côté site), la date, la durée, le profil sous forme
//! d'empreinte ([`empreinte_de_profil`]), et pour le morceau : ISRC et MBID
//! d'enregistrement s'ils sont connus, MBID de la release et de l'artiste
//! s'ils sont connus (#6738), titre, artiste, album, durée.
//!
//! Jamais : chemin de fichier, adresse IP, nom de zone, nom ou numéro de
//! profil, pochette, identifiant de service. La requête qui lit l'historique
//! ([`SQL_LOT`]) ne sélectionne aucune de ces colonnes : ce qui n'est pas lu
//! ne peut pas partir.
//!
//! ## Durée
//!
//! `listen_history.duration_ms` est écrit au LANCEMENT de la piste
//! (`orchestrator::commun::record_listen`) : c'est la durée du morceau, celle
//! que somment déjà `GET /stats/listening` et le tableau de bord. Le site
//! reçoit donc la même mesure que les statistiques locales, ni plus ni moins.
//!
//! ## Idempotence et réessai
//!
//! Un curseur local ([`CLE_CURSEUR`]) retient le dernier `listen_history.id`
//! accepté par le site. Il n'avance qu'après un 2xx : hors ligne, 429, 5xx ou
//! refus, le même lot repart au passage suivant. L'identifiant d'événement est
//! stable (empreinte du serveur, de la ligne, de sa date et de son titre) : un
//! lot renvoyé après une réponse perdue est dédoublonné par le site.
//!
//! ## Rattrapage borné
//!
//! Au premier passage après l'activation (pas de curseur), seules les écoutes
//! des [`RATTRAPAGE_JOURS`] derniers jours partent, et au plus
//! [`RATTRAPAGE_MAX`] — les plus récentes. Le plancher de date est noté
//! ([`CLE_PLANCHER`]) : il ne glisse pas d'un passage à l'autre.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tracing::{debug, info, warn};

use crate::cloud::consent::est_vrai;
use crate::cloud::rate_limit::{self, AppelCloud, CloudScope};
use crate::cloud::sauvegarde_config::{self as sc, Compte};
use crate::db::backend::{DbBackend, ToSqlValue};
use crate::db::settings_repo::SettingsRepo;

/// Le réglage « Envoyer mes écoutes à mon compte ». Écrit par le client web
/// (`PATCH /system/config`) ; publié par `GET /system/config` avec
/// [`ACTIVATION_DEFAUT`].
pub const CLE_ACTIVATION: &str = "cloud_listens_upload_enabled";
/// Désactivé par défaut : opt-in strict.
pub const ACTIVATION_DEFAUT: bool = false;
/// Dernier `listen_history.id` accepté par le site.
pub const CLE_CURSEUR: &str = "cloud_listens_cursor";
/// Plancher de date du rattrapage, fixé à l'activation.
pub const CLE_PLANCHER: &str = "cloud_listens_floor";
/// Date du dernier lot accepté (RFC 3339).
pub const CLE_DERNIER_ENVOI: &str = "cloud_listens_last_upload";

/// Chemin de l'API du site.
pub const CHEMIN_API: &str = "/api/v1/listens";
/// Écoutes par lot (le site en accepte 500 au plus).
pub const TAILLE_DU_LOT: i64 = 200;
/// Lots au plus par passage : un rattrapage de 20 000 écoutes s'étale sur
/// quelques passages au lieu de tenir le site une minute d'affilée.
pub const LOTS_PAR_PASSE: usize = 25;
/// Fenêtre du rattrapage à l'activation.
pub const RATTRAPAGE_JOURS: i64 = 365;
/// Écoutes au plus dans le rattrapage.
pub const RATTRAPAGE_MAX: i64 = 20_000;
/// Cadence de la tâche de fond.
pub const CADENCE_SECONDES: u64 = 300;
/// Longueur maximale d'un titre, d'un artiste ou d'un album envoyé (celle que
/// le site accepte).
pub const LONGUEUR_MAX: usize = 300;

/// L'utilisateur a-t-il activé l'envoi ? Absent, illisible ou autre : non.
pub fn envoi_active(settings: &SettingsRepo) -> bool {
    settings
        .get(CLE_ACTIVATION)
        .ok()
        .flatten()
        .map(|v| est_vrai(&v))
        .unwrap_or(ACTIVATION_DEFAUT)
}

/// Ce qu'un passage a fait.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Issue {
    /// Rien n'est parti, et c'est normal : `disabled`, `account_not_linked`,
    /// `nothing_new`.
    Rien(&'static str),
    /// Des lots ont été acceptés.
    Envoye {
        lots: usize,
        ecoutes: usize,
        doublons: i64,
    },
    /// Un envoi a échoué : le curseur n'a pas bougé, le lot repartira.
    Echec(String),
}

fn hex(octets: &[u8], longueur: usize) -> String {
    let mut s = String::with_capacity(octets.len() * 2);
    for o in octets {
        s.push_str(&format!("{o:02x}"));
    }
    s.truncate(longueur);
    s
}

/// L'identifiant d'événement d'une écoute : stable d'un envoi à l'autre (le
/// site dédoublonne dessus), opaque (il ne dit rien de la base locale), et
/// propre à la ligne ET à sa date : une base remise à zéro, qui réutilise
/// des numéros de ligne, ne fait pas passer ses nouvelles écoutes pour des
/// doublons des anciennes.
pub fn identifiant_d_evenement(server_id: &str, id: i64, ecoutee_le: &str, titre: &str) -> String {
    let mut h = Sha256::new();
    h.update(b"tune-listen\x1f");
    h.update(server_id.as_bytes());
    h.update(b"\x1f");
    h.update(id.to_string().as_bytes());
    h.update(b"\x1f");
    h.update(ecoutee_le.as_bytes());
    h.update(b"\x1f");
    h.update(titre.as_bytes());
    hex(&h.finalize(), 32)
}

/// Le profil, tel que le site le voit : une empreinte du serveur et du numéro
/// local, jamais le numéro ni le nom. Deux profils d'un même serveur restent
/// distincts ; le même numéro sur deux serveurs donne deux empreintes.
pub fn empreinte_de_profil(server_id: &str, profile_id: i64) -> String {
    let mut h = Sha256::new();
    h.update(b"tune-profile\x1f");
    h.update(server_id.as_bytes());
    h.update(b"\x1f");
    h.update(profile_id.to_string().as_bytes());
    hex(&h.finalize(), 16)
}

fn borne(v: Option<String>) -> Option<String> {
    v.map(|s| s.trim().chars().take(LONGUEUR_MAX).collect::<String>())
        .filter(|s| !s.is_empty())
}

/// Le lot à envoyer : les écoutes après le curseur, hors radio, à partir du
/// plancher. ISRC et MBID (et celui de l'artiste) viennent de la piste
/// locale rapprochée par titre ET album, comme la liste de l'historique
/// (`history_repo::JOINTURE_PISTE`) ; celui de la release, de l'album ;
/// une sous-requête `LIMIT 1` par champ, pour qu'un titre présent deux fois
/// sur un album ne duplique pas l'écoute.
///
/// Aucune colonne de chemin, de zone, de pochette ni d'identifiant de service
/// n'est lue ici.
pub const SQL_LOT: &str = "SELECT CAST(h.id AS BIGINT), h.listened_at, \
     CAST(h.duration_ms AS BIGINT), CAST(h.profile_id AS BIGINT), \
     h.title, h.artist_name, h.album_title, \
     (SELECT t.isrc FROM tracks t WHERE t.title = h.title AND t.album_id = h.album_id \
        AND t.isrc IS NOT NULL AND t.isrc <> '' LIMIT 1), \
     (SELECT t.musicbrainz_recording_id FROM tracks t WHERE t.title = h.title \
        AND t.album_id = h.album_id AND t.musicbrainz_recording_id IS NOT NULL \
        AND t.musicbrainz_recording_id <> '' LIMIT 1), \
     (SELECT a.musicbrainz_release_id FROM albums a WHERE a.id = h.album_id \
        AND a.musicbrainz_release_id IS NOT NULL AND a.musicbrainz_release_id <> '' LIMIT 1), \
     (SELECT ar.musicbrainz_id FROM tracks t JOIN artists ar ON ar.id = t.artist_id \
        WHERE t.title = h.title AND t.album_id = h.album_id \
        AND ar.musicbrainz_id IS NOT NULL AND ar.musicbrainz_id <> '' LIMIT 1) \
     FROM listen_history h \
     WHERE CAST(h.id AS BIGINT) > ? AND COALESCE(h.source, '') <> 'radio' \
       AND h.listened_at >= ? \
     ORDER BY CAST(h.id AS BIGINT) ASC LIMIT ?";

/// Une écoute prête à partir.
#[derive(Debug, Clone)]
struct Ligne {
    id: i64,
    json: Value,
}

fn lire_le_lot(
    backend: &Arc<dyn DbBackend>,
    server_id: &str,
    curseur: i64,
    plancher: &str,
) -> Result<Vec<Ligne>, String> {
    let limite = TAILLE_DU_LOT;
    let lignes = backend.query_many(
        SQL_LOT,
        &[
            &curseur as &dyn ToSqlValue,
            &plancher.to_string() as &dyn ToSqlValue,
            &limite as &dyn ToSqlValue,
        ],
    )?;
    let mut lot = Vec::with_capacity(lignes.len());
    for r in &lignes {
        let Some(id) = r.first().and_then(|v| v.as_i64()) else {
            continue;
        };
        let ecoutee_le = r.get(1).and_then(|v| v.as_string()).unwrap_or_default();
        let duree = r.get(2).and_then(|v| v.as_i64()).filter(|d| *d > 0);
        let profil = r.get(3).and_then(|v| v.as_i64());
        let titre_brut = r.get(4).and_then(|v| v.as_string()).unwrap_or_default();
        let Some(titre) = borne(Some(titre_brut.clone())) else {
            // Une écoute sans titre ne se consolide avec rien : elle avance le
            // curseur sans partir.
            lot.push(Ligne {
                id,
                json: Value::Null,
            });
            continue;
        };
        lot.push(Ligne {
            id,
            json: json!({
                "event_id": identifiant_d_evenement(server_id, id, &ecoutee_le, &titre_brut),
                "listened_at": ecoutee_le,
                "duration_ms": duree,
                "profile": profil.map(|p| empreinte_de_profil(server_id, p)),
                "track": {
                    "isrc": borne(r.get(7).and_then(|v| v.as_string())),
                    "mbid": borne(r.get(8).and_then(|v| v.as_string())),
                    // #6738 : l'album (release) et l'artiste, pour que le
                    // site retrouve leurs écoutes par MBID (site-mozaiklabs#330).
                    "release_mbid": borne(r.get(9).and_then(|v| v.as_string())),
                    "artist_mbid": borne(r.get(10).and_then(|v| v.as_string())),
                    "title": titre,
                    "artist": borne(r.get(5).and_then(|v| v.as_string())),
                    "album": borne(r.get(6).and_then(|v| v.as_string())),
                    "duration_ms": duree,
                },
            }),
        });
    }
    Ok(lot)
}

/// Le corps exact d'un lot, tel qu'il part. Exposé pour les bancs : la charge
/// éprouvée est celle qui part, pas une charge reconstruite à côté.
#[doc(hidden)]
pub fn corps_du_lot(server_id: &str, ecoutes: &[Value]) -> Value {
    json!({ "server_id": server_id, "listens": ecoutes })
}

fn date_iso(d: DateTime<Utc>) -> String {
    d.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// Le curseur et le plancher. Absent (première activation) : les fixe pour
/// un rattrapage borné à [`RATTRAPAGE_JOURS`] et [`RATTRAPAGE_MAX`], et les
/// écrit tout de suite — le plancher ne glisse pas d'un passage à l'autre.
pub fn curseur_et_plancher(
    backend: &Arc<dyn DbBackend>,
    settings: &SettingsRepo,
    maintenant: DateTime<Utc>,
) -> (i64, String) {
    let curseur = settings
        .get(CLE_CURSEUR)
        .ok()
        .flatten()
        .and_then(|v| v.trim().parse::<i64>().ok());
    let plancher = settings
        .get(CLE_PLANCHER)
        .ok()
        .flatten()
        .filter(|v| !v.trim().is_empty());
    if let (Some(c), Some(p)) = (curseur, plancher.clone()) {
        return (c, p);
    }
    let plancher =
        plancher.unwrap_or_else(|| date_iso(maintenant - chrono::Duration::days(RATTRAPAGE_JOURS)));
    // La RATTRAPAGE_MAX-ième écoute la plus récente de la fenêtre : tout ce
    // qui la précède reste local.
    let decalage = RATTRAPAGE_MAX - 1;
    let curseur = curseur.unwrap_or_else(|| {
        backend
            .query_one(
                "SELECT CAST(h.id AS BIGINT) FROM listen_history h \
                 WHERE COALESCE(h.source, '') <> 'radio' AND h.listened_at >= ? \
                 ORDER BY CAST(h.id AS BIGINT) DESC LIMIT 1 OFFSET ?",
                &[&plancher as &dyn ToSqlValue, &decalage as &dyn ToSqlValue],
            )
            .ok()
            .flatten()
            .and_then(|r| r.first().and_then(|v| v.as_i64()))
            .map(|id| id - 1)
            .unwrap_or(0)
    });
    settings.set(CLE_CURSEUR, &curseur.to_string()).ok();
    settings.set(CLE_PLANCHER, &plancher).ok();
    info!(curseur, plancher = %plancher, "cloud_listens_catchup_window_fixed");
    (curseur, plancher)
}

pub(crate) fn motif(corps: &str) -> Option<String> {
    serde_json::from_str::<Value>(corps)
        .ok()?
        .get("error")?
        .as_str()
        .map(str::to_string)
}

/// Un passage : jusqu'à [`LOTS_PAR_PASSE`] lots, chacun accepté par le site
/// avant que le curseur n'avance.
pub async fn passe(
    backend: &Arc<dyn DbBackend>,
    http: &reqwest::Client,
    maintenant: DateTime<Utc>,
) -> Issue {
    let settings = SettingsRepo::with_backend(backend.clone());
    if !envoi_active(&settings) {
        return Issue::Rien("disabled");
    }
    let Ok(mut compte) = sc::compte(&settings) else {
        return Issue::Rien("account_not_linked");
    };
    // Une seule reliaison et un seul rafraîchissement de session par passage :
    // un site qui refuse le jeton neuf ne fait pas tourner l'envoi en rond.
    let mut liaison_tentee = false;
    let mut session_rafraichie = false;
    if compte.liaison.is_none() {
        liaison_tentee = true;
        if !relier(backend, http, &settings, &mut compte).await {
            return Issue::Echec("server_not_linked".into());
        }
    }

    let (mut curseur, plancher) = curseur_et_plancher(backend, &settings, maintenant);
    let mut lots = 0usize;
    let mut ecoutes = 0usize;
    let mut doublons = 0i64;

    while lots < LOTS_PAR_PASSE {
        let lot = match lire_le_lot(backend, &compte.server_id, curseur, &plancher) {
            Ok(l) => l,
            Err(e) => return Issue::Echec(format!("listen_history: {e}")),
        };
        let Some(dernier) = lot.last().map(|l| l.id) else {
            break;
        };
        let plein = lot.len() as i64 >= TAILLE_DU_LOT;
        let charges: Vec<Value> = lot
            .into_iter()
            .map(|l| l.json)
            .filter(|j| !j.is_null())
            .collect();
        if !charges.is_empty() {
            let corps = corps_du_lot(&compte.server_id, &charges);
            loop {
                let requete = http
                    .post(format!("{}{CHEMIN_API}", compte.base))
                    .bearer_auth(&compte.acces)
                    .header(
                        crate::cloud::library_sync::EN_TETE_JETON_DE_SERVEUR,
                        compte.liaison.clone().unwrap_or_default(),
                    )
                    .header(reqwest::header::ACCEPT, "application/json")
                    .json(&corps)
                    .timeout(std::time::Duration::from_secs(30));
                match rate_limit::appeler(&settings, CloudScope::ListensSync, requete).await {
                    AppelCloud::Retenu(_) => return fin(lots, ecoutes, doublons, "rate_limited"),
                    AppelCloud::Erreur(e) => {
                        let erreur = e.without_url().to_string();
                        debug!(error = %erreur, "cloud_listens_upload_offline");
                        return fin(lots, ecoutes, doublons, "network");
                    }
                    AppelCloud::Reponse(r) if r.status().is_success() => {
                        let v: Value = r.json().await.unwrap_or(Value::Null);
                        doublons += v.get("duplicates").and_then(Value::as_i64).unwrap_or(0);
                        break;
                    }
                    AppelCloud::Reponse(r) => {
                        let statut = r.status().as_u16();
                        let code = motif(&r.text().await.unwrap_or_default());
                        // Jeton de liaison faux ou ancien, ou serveur pas (ou
                        // plus) relié à ce compte : relier, puis réessayer UNE fois.
                        let liaison_refusee = matches!(
                            (statut, code.as_deref()),
                            (401, Some("server_token_invalid")) | (404, _)
                        );
                        if liaison_refusee && !liaison_tentee {
                            liaison_tentee = true;
                            if relier(backend, http, &settings, &mut compte).await {
                                continue;
                            }
                        } else if statut == 401 && !session_rafraichie {
                            session_rafraichie = true;
                            if let Some(c) = sc::rafraichir(&settings).await {
                                compte = c;
                                continue;
                            }
                        }
                        let raison = match code {
                            Some(c) => format!("HTTP {statut} {c}"),
                            None => format!("HTTP {statut}"),
                        };
                        if statut == 429 || statut >= 500 {
                            debug!(statut, "cloud_listens_upload_deferred");
                        } else {
                            warn!(raison = %raison, "cloud_listens_upload_refused");
                        }
                        return fin(lots, ecoutes, doublons, &raison);
                    }
                }
            }
            ecoutes += charges.len();
        }
        // Accepté (ou rien à envoyer dans ce lot) : le curseur avance.
        curseur = dernier;
        settings.set(CLE_CURSEUR, &curseur.to_string()).ok();
        settings
            .set(CLE_DERNIER_ENVOI, &maintenant.to_rfc3339())
            .ok();
        lots += 1;
        if !plein {
            break;
        }
    }
    if lots == 0 {
        return Issue::Rien("nothing_new");
    }
    info!(lots, ecoutes, doublons, "cloud_listens_uploaded");
    Issue::Envoye {
        lots,
        ecoutes,
        doublons,
    }
}

/// Un échec en cours de passage : ce qui a déjà été accepté le reste (le
/// curseur est écrit lot par lot), le reste repartira.
fn fin(lots: usize, ecoutes: usize, doublons: i64, raison: &str) -> Issue {
    if lots > 0 {
        info!(
            lots,
            ecoutes, doublons, raison, "cloud_listens_uploaded_partially"
        );
    }
    Issue::Echec(raison.to_string())
}

/// Relie le serveur au compte par la route de la copie de bibliothèque, et
/// relit le jeton. Rend `true` quand un jeton est là.
pub(crate) async fn relier(
    backend: &Arc<dyn DbBackend>,
    http: &reqwest::Client,
    settings: &SettingsRepo,
    compte: &mut Compte,
) -> bool {
    let api = format!("{}/api/v1/cloud-library", compte.base);
    if crate::cloud::library_sync::lier_le_serveur(
        backend,
        http,
        &api,
        &compte.server_id,
        &compte.acces,
    )
    .await
    .is_err()
    {
        return false;
    }
    match sc::compte(settings) {
        Ok(c) if c.liaison.is_some() => {
            *compte = c;
            true
        }
        _ => false,
    }
}

/// La tâche de fond : un passage toutes les [`CADENCE_SECONDES`], deux
/// minutes après le démarrage. Chaque passage relit le réglage : l'activer ou
/// le couper prend effet sans redémarrage.
pub fn spawn(backend: Arc<dyn DbBackend>, http: reqwest::Client) {
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(120)).await;
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(CADENCE_SECONDES));
        loop {
            ticker.tick().await;
            match passe(&backend, &http, Utc::now()).await {
                Issue::Echec(e) => debug!(raison = %e, "cloud_listens_pass_failed"),
                Issue::Envoye { .. } | Issue::Rien(_) => {}
            }
        }
    });
}

#[cfg(test)]
#[path = "ecoutes_au_compte_tests.rs"]
mod tests;
