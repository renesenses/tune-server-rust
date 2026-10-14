//! Côté AGENT : le code d'appairage, les maîtres appairés, les sorties
//! prêtées et l'exécution des ordres (#4626).

use std::sync::Arc;

use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::outputs::traits::{OutputTarget, PlayMedia, TransportState};

use super::{
    AvisDeRevocation, Bail, CLE_MAITRES, Commande, DemandeAppairage, EtatSortieDistante,
    ReponseAppairage, SortieExposee, TYPES_EXPOSES, empreinte, identite, maintenant,
    secret_aleatoire,
};
use crate::state::AppState;

/// Durée de validité d'un code d'appairage.
pub const DUREE_DU_CODE_S: i64 = 300;
/// Essais permis sur un même code avant qu'il ne soit brûlé.
pub const ESSAIS_PAR_CODE: u32 = 5;

/// Le code affiché sur l'agent, gardé en mémoire seulement : un code en cours
/// n'a aucune raison de survivre à un redémarrage.
#[derive(Debug, Clone)]
pub struct CodeEnCours {
    empreinte: String,
    expire_le: i64,
    essais_restants: u32,
}

/// Un maître appairé, tel que l'agent le garde : l'EMPREINTE du jeton, jamais
/// le jeton lui-même.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MaitreAppaire {
    pub maitre_id: String,
    pub nom: String,
    pub empreinte_jeton: String,
    pub appaire_le: i64,
    /// Où joindre le maître (`http://hôte:port`) pour le prévenir d'une
    /// révocation : l'adresse d'où est venue la demande d'appairage, et le
    /// port que le maître a annoncé. `None` pour un maître qui ne l'annonce pas.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adresse: Option<String>,
}

/// `http://hôte:port`, l'IPv6 entre crochets.
fn adresse_http(ip: std::net::IpAddr, port: u16) -> String {
    format!("http://{}", std::net::SocketAddr::new(ip, port))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefusAppairage {
    /// Aucun code n'a été émis sur l'agent.
    AucunCode,
    /// Le code a expiré.
    Expire,
    /// Le code ne correspond pas (il reste des essais).
    Mauvais,
    /// Le dernier essai est passé : le code est brûlé.
    Epuise,
    /// Le maître est ce serveur lui-même.
    SoiMeme,
}

impl RefusAppairage {
    pub fn motif(self) -> &'static str {
        match self {
            Self::AucunCode => "aucun code d'appairage en cours sur ce serveur",
            Self::Expire => "le code d'appairage a expiré",
            Self::Mauvais => "code d'appairage incorrect",
            Self::Epuise => "trop d'essais : le code d'appairage est annulé",
            Self::SoiMeme => "un serveur ne s'appaire pas avec lui-même",
        }
    }
}

pub fn maitres(state: &AppState) -> Vec<MaitreAppaire> {
    SettingsRepo::with_backend(state.backend.clone())
        .get(CLE_MAITRES)
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn enregistrer_maitres(state: &AppState, maitres: &[MaitreAppaire]) -> Result<(), String> {
    let json = serde_json::to_string(maitres).map_err(|e| e.to_string())?;
    SettingsRepo::with_backend(state.backend.clone()).set(CLE_MAITRES, &json)
}

/// Émet un nouveau code à six chiffres (remplace le précédent).
/// Rend le code en clair et sa durée de validité en secondes.
pub fn emettre_code(state: &AppState) -> (String, i64) {
    let code = format!("{:06}", uuid::Uuid::new_v4().as_u128() % 1_000_000);
    *state.agent_tune.code.lock().unwrap() = Some(CodeEnCours {
        empreinte: empreinte(&code),
        expire_le: maintenant() + DUREE_DU_CODE_S,
        essais_restants: ESSAIS_PAR_CODE,
    });
    (code, DUREE_DU_CODE_S)
}

/// Consomme le code et appaire le maître. Rend la réponse à lui transmettre.
/// `ip_du_maitre` : l'adresse d'où vient la demande, gardée (avec le port
/// annoncé) pour prévenir le maître d'une révocation.
pub async fn appairer(
    state: &AppState,
    demande: &DemandeAppairage,
    ip_du_maitre: Option<std::net::IpAddr>,
) -> Result<ReponseAppairage, RefusAppairage> {
    let (agent_id, agent_nom) = identite(state);
    if demande.maitre_id == agent_id {
        return Err(RefusAppairage::SoiMeme);
    }
    {
        let mut code = state.agent_tune.code.lock().unwrap();
        let Some(en_cours) = code.as_mut() else {
            return Err(RefusAppairage::AucunCode);
        };
        if maintenant() > en_cours.expire_le {
            *code = None;
            return Err(RefusAppairage::Expire);
        }
        if empreinte(demande.code.trim()) != en_cours.empreinte {
            en_cours.essais_restants = en_cours.essais_restants.saturating_sub(1);
            if en_cours.essais_restants == 0 {
                *code = None;
                return Err(RefusAppairage::Epuise);
            }
            return Err(RefusAppairage::Mauvais);
        }
        // Usage unique.
        *code = None;
    }
    let jeton = secret_aleatoire();
    let mut liste = maitres(state);
    liste.retain(|m| m.maitre_id != demande.maitre_id);
    liste.push(MaitreAppaire {
        maitre_id: demande.maitre_id.clone(),
        nom: demande.maitre_nom.clone(),
        empreinte_jeton: empreinte(&jeton),
        appaire_le: maintenant(),
        adresse: ip_du_maitre
            .zip(demande.maitre_port)
            .map(|(ip, port)| adresse_http(ip, port)),
    });
    if let Err(e) = enregistrer_maitres(state, &liste) {
        tracing::warn!(error = %e, "agent_tune_maitre_non_persiste");
        return Err(RefusAppairage::AucunCode);
    }
    tracing::info!(maitre = %demande.maitre_nom, maitre_id = %demande.maitre_id, "agent_tune_maitre_appaire");
    Ok(ReponseAppairage {
        agent_id,
        agent_nom,
        jeton,
        sorties: sorties_exposees(state).await,
    })
}

/// Le maître qui présente ce jeton, s'il est appairé.
pub fn maitre_du_jeton(state: &AppState, jeton: &str) -> Option<MaitreAppaire> {
    if jeton.is_empty() {
        return None;
    }
    let e = empreinte(jeton);
    maitres(state).into_iter().find(|m| m.empreinte_jeton == e)
}

/// Retire un maître ; les sorties qu'il tenait sont arrêtées.
///
/// `prevenir` : la révocation vient de l'utilisateur de l'agent, et le maître
/// en est averti (au mieux, en tâche de fond) pour qu'il supprime les zones
/// qu'il tenait de cet agent. Quand c'est le maître lui-même qui s'est retiré
/// (`POST /agent-tune/oublier`), il n'y a personne à prévenir.
pub async fn oublier_maitre(state: &AppState, maitre_id: &str, prevenir: bool) -> bool {
    let mut liste = maitres(state);
    let Some(position) = liste.iter().position(|m| m.maitre_id == maitre_id) else {
        return false;
    };
    let oublie = liste.remove(position);
    if let Err(e) = enregistrer_maitres(state, &liste) {
        tracing::warn!(error = %e, "agent_tune_maitre_non_retire");
        return false;
    }
    let tenues: Vec<String> = {
        let mut baux = state.agent_tune.baux.lock().unwrap();
        let ids: Vec<String> = baux
            .iter()
            .filter(|(_, b)| b.maitre_id == maitre_id)
            .map(|(id, _)| id.clone())
            .collect();
        for id in &ids {
            baux.remove(id);
        }
        ids
    };
    for device_id in tenues {
        if let Some(sortie) = sortie_locale(state, &device_id).await {
            let _ = sortie.lock().await.stop().await;
        }
    }
    tracing::info!(maitre_id = %maitre_id, "agent_tune_maitre_oublie");
    if prevenir && let Some(adresse) = oublie.adresse {
        let (agent_id, _) = identite(state);
        tokio::spawn(async move {
            let envoi = tune_core::http::client::builder()
                .build()
                .unwrap_or_default()
                .post(format!("{adresse}/agent-tune/revocation"))
                .timeout(std::time::Duration::from_secs(5))
                .json(&AvisDeRevocation { agent_id })
                .send()
                .await;
            if let Err(e) = envoi {
                // Le maître constatera la révocation à son prochain démarrage.
                tracing::info!(error = %e, "agent_tune_revocation_non_transmise");
            }
        });
    }
    true
}

/// La sortie LOCALE de cet agent, si elle est d'un type prêté.
async fn sortie_locale(
    state: &AppState,
    device_id: &str,
) -> Option<Arc<Mutex<Box<dyn OutputTarget>>>> {
    let registre = state.outputs.lock().await;
    let type_ = registre.type_of(device_id)?;
    if !TYPES_EXPOSES.contains(&type_.as_str()) {
        return None;
    }
    registre.get(device_id)
}

/// Les sorties que cet agent prête : ses sorties locales.
pub async fn sorties_exposees(state: &AppState) -> Vec<SortieExposee> {
    // Le registre est relâché AVANT de verrouiller chaque sortie (voir la
    // note de `OutputRegistry::meta` sur l'interblocage).
    let sorties: Vec<(String, Arc<Mutex<Box<dyn OutputTarget>>>)> = {
        let registre = state.outputs.lock().await;
        let mut ids = registre.list();
        ids.sort();
        ids.into_iter()
            .filter(|id| {
                registre
                    .type_of(id)
                    .is_some_and(|t| TYPES_EXPOSES.contains(&t.as_str()))
            })
            .filter_map(|id| registre.get(&id).map(|s| (id, s)))
            .collect()
    };
    let mut rendu = Vec::with_capacity(sorties.len());
    for (device_id, sortie) in sorties {
        let sortie = sortie.lock().await;
        rendu.push(SortieExposee {
            device_id,
            nom: sortie.name().to_string(),
            capacites: sortie.capabilities(),
        });
    }
    rendu
}

/// Deux URL désignent-elles le même flux ? On compare chemin et requête : la
/// sortie locale peut réécrire l'HÔTE (boucle locale, #5639).
fn meme_flux(a: &str, b: &str) -> bool {
    match (reqwest::Url::parse(a), reqwest::Url::parse(b)) {
        (Ok(a), Ok(b)) => a.path() == b.path() && a.query() == b.query(),
        _ => a == b,
    }
}

fn occupee(etat: TransportState) -> bool {
    matches!(
        etat,
        TransportState::Playing | TransportState::Paused | TransportState::Transitioning
    )
}

/// Le bail de cette sortie, s'il est à ce maître. Un bail dont la sortie joue
/// désormais AUTRE CHOSE (la zone locale de l'agent l'a reprise) est rompu.
fn bail_valide(
    state: &AppState,
    maitre_id: &str,
    device_id: &str,
    uri_courante: Option<&str>,
) -> bool {
    let mut baux = state.agent_tune.baux.lock().unwrap();
    let Some(bail) = baux.get(device_id) else {
        return false;
    };
    if bail.maitre_id != maitre_id {
        return false;
    }
    if let Some(uri) = uri_courante
        && !meme_flux(uri, &bail.url)
    {
        tracing::info!(device_id = %device_id, "agent_tune_bail_rompu_reprise_locale");
        baux.remove(device_id);
        return false;
    }
    true
}

type Refus = (StatusCode, String);

/// Exécute un ordre du maître sur une sortie de l'agent.
///
/// Règle de partage avec l'usage local de l'agent :
/// - une sortie qui joue déjà (zone locale de l'agent, ou un AUTRE maître)
///   est refusée en 409 — on ne coupe pas quelqu'un qui écoute ;
/// - une fois tenue par un maître, la sortie lui reste jusqu'à `arreter`, ou
///   jusqu'à ce qu'on la reprenne sur place (le bail se rompt alors) ;
/// - le volume d'une sortie libre peut être réglé (le maître le pose avant
///   la lecture) ; celui d'une sortie occupée par un autre, non.
pub async fn executer(
    state: &AppState,
    maitre_id: &str,
    device_id: &str,
    commande: Commande,
) -> Result<(), Refus> {
    let sortie = sortie_locale(state, device_id).await.ok_or((
        StatusCode::NOT_FOUND,
        format!("sortie inconnue sur cet agent : {device_id}"),
    ))?;
    let sortie = sortie.lock().await;
    let statut = sortie
        .get_status()
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, e))?;
    let tenue = bail_valide(state, maitre_id, device_id, statut.current_uri.as_deref());
    let libre = !occupee(statut.state);
    let erreur = |e: String| (StatusCode::BAD_GATEWAY, e);
    let pas_tenue = || {
        (
            StatusCode::CONFLICT,
            "sortie occupée sur l'agent : une lecture locale ou un autre maître la tient"
                .to_string(),
        )
    };
    match commande {
        Commande::Lire {
            url,
            mime_type,
            titre,
            artiste,
            album,
            pochette,
            duree_ms,
            direct,
        } => {
            if !tenue && !libre {
                return Err(pas_tenue());
            }
            let media = PlayMedia {
                url: &url,
                mime_type: &mime_type,
                title: titre.as_deref(),
                artist: artiste.as_deref(),
                album: album.as_deref(),
                cover_url: pochette.as_deref(),
                duration_ms: duree_ms,
                live_stream: direct,
                ..Default::default()
            };
            sortie.play_media(&media).await.map_err(erreur)?;
            state.agent_tune.baux.lock().unwrap().insert(
                device_id.to_string(),
                Bail {
                    maitre_id: maitre_id.to_string(),
                    url,
                },
            );
            Ok(())
        }
        Commande::Volume { volume } => {
            if !tenue && !libre {
                return Err(pas_tenue());
            }
            sortie.set_volume(volume).await.map_err(erreur)
        }
        Commande::Muet { muet } => {
            if !tenue && !libre {
                return Err(pas_tenue());
            }
            sortie.set_mute(muet).await.map_err(erreur)
        }
        Commande::Arreter => {
            if !tenue {
                // Rien à arrêter pour ce maître : ne touche pas la lecture
                // d'un autre.
                return Ok(());
            }
            state.agent_tune.baux.lock().unwrap().remove(device_id);
            sortie.stop().await.map_err(erreur)
        }
        Commande::Pause => {
            if !tenue {
                return Err(pas_tenue());
            }
            sortie.pause().await.map_err(erreur)
        }
        Commande::Reprendre => {
            if !tenue {
                return Err(pas_tenue());
            }
            sortie.resume().await.map_err(erreur)
        }
        Commande::Position { position_ms } => {
            if !tenue {
                return Err(pas_tenue());
            }
            sortie.seek(position_ms).await.map_err(erreur)
        }
    }
}

/// L'état d'une sortie, vu par ce maître. Une sortie qu'il ne tient pas est
/// rendue « arrêtée » : la lecture locale de l'agent ne le regarde pas.
pub async fn etat(
    state: &AppState,
    maitre_id: &str,
    device_id: &str,
) -> Result<EtatSortieDistante, Refus> {
    let sortie = sortie_locale(state, device_id).await.ok_or((
        StatusCode::NOT_FOUND,
        format!("sortie inconnue sur cet agent : {device_id}"),
    ))?;
    let sortie = sortie.lock().await;
    let mut statut = sortie
        .get_status()
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, e))?;
    let tenue = bail_valide(state, maitre_id, device_id, statut.current_uri.as_deref());
    if !tenue {
        statut.state = TransportState::Stopped;
        statut.position_ms = 0;
        statut.current_uri = None;
        statut.track_title = None;
        statut.track_artist = None;
        statut.ended_naturally = false;
    }
    Ok(EtatSortieDistante { tenue, statut })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meme_flux_ignore_l_hote_mais_pas_le_chemin() {
        assert!(meme_flux(
            "http://192.0.2.5:8888/stream/1.flac",
            "http://127.0.0.1:8888/stream/1.flac"
        ));
        assert!(!meme_flux(
            "http://192.0.2.5:8888/stream/1.flac",
            "http://192.0.2.5:8888/stream/2.flac"
        ));
        assert!(!meme_flux(
            "http://192.0.2.5:8888/stream/1.flac?s=1",
            "http://192.0.2.5:8888/stream/1.flac?s=2"
        ));
    }
}
