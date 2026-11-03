//! Favoris radio → playlist chez un service de streaming (Qobuz d'abord).
//!
//! Un favori radio est un MORCEAU capté à l'antenne (`radio_favorites` :
//! titre, artiste, station), sans ISRC ni durée. Pour le faire entrer dans une
//! playlist Qobuz, il faut le retrouver dans le catalogue du service.
//!
//! Ce module tient les deux gestes que la route `POST /radio-favorites/...`
//! enchaîne, pour qu'il n'y en ait qu'UNE écriture :
//!
//! 1. [`apparier_favori_radio`] — chercher le favori chez le service et
//!    rendre la meilleure correspondance, ses alternatives classées et un
//!    statut (`matched`, `approximate`, `ambiguous`, `not_found`,
//!    `search_failed`). Le vainqueur est celui que la création « en un geste »
//!    retenait déjà (`create-playlist` sans aperçu) : mêmes requêtes, même
//!    matcher ([`best_stream_matches_scored`], dont la tête EST le verdict de
//!    `best_stream_match_scored`). L'aperçu ajoute seulement de quoi montrer
//!    la correspondance à l'utilisateur, qui la valide ou l'écarte.
//! 2. [`creer_playlist_avec_titres`] — créer la playlist chez le service et y
//!    verser les titres RETENUS, sans doublon, par les méthodes du trait
//!    ([`StreamingService::create_playlist`] puis
//!    [`StreamingService::add_tracks_to_playlist`]) que Qobuz implémente déjà.

use serde::Serialize;

use crate::library::track_matcher::normalize;
use crate::streaming::matching::{
    LIMITE_RECHERCHE_APPARIEMENT, MATCH_ACCEPT_SCORE, best_stream_matches_scored,
};
use crate::streaming::traits::{StreamTrack, StreamingService};

/// Écart de score sous lequel deux candidats DIFFÉRENTS se valent : le
/// matcher ne sait pas lequel l'auditeur a entendu. Un favori capté sans
/// artiste (« Hallelujah ») score à égalité sur Jeff Buckley et Leonard
/// Cohen ; choisir le premier rendu par le service serait une invention.
pub const ECART_AMBIGUITE: f64 = 0.05;

/// Combien d'alternatives l'aperçu montre sous la correspondance retenue.
pub const MAX_ALTERNATIVES: usize = 3;

/// Description posée sur la playlist créée chez le service.
pub const DESCRIPTION_PLAYLIST: &str = "Created by Tune from radio favorites";

/// Beaucoup de radios livrent tout dans le StreamTitle ICY : le favori arrive
/// alors avec un artiste vide et un titre « Artiste - Titre ». Le découper
/// avant l'appariement, sinon le titre composite ne ressemble à aucun vrai
/// titre et rien ne correspond (forum #1234, Xavier).
///
/// Rend `(titre, artiste)`.
pub fn scinder_titre_icy(title: &str, artist: &str) -> (String, String) {
    if artist.trim().is_empty()
        && let Some((a, t)) = title.split_once(" - ")
    {
        let (a, t) = (a.trim(), t.trim());
        if !a.is_empty() && !t.is_empty() {
            return (t.to_string(), a.to_string());
        }
    }
    (title.to_string(), artist.to_string())
}

/// Les requêtes envoyées au service, dans l'ordre : « artiste titre », puis le
/// titre seul, puis le titre tronqué avant « ( » ou « - ».
///
/// Le matcher normalise déjà « (feat. X) / (Live) » pour SCORER, mais la
/// REQUÊTE envoyée au service les contenait encore, et Qobuz ne renvoyait
/// alors aucun candidat (#1235 : une recherche manuelle sans le suffixe
/// trouvait la piste).
pub fn requetes_pour_favori(title: &str, artist: &str) -> Vec<String> {
    let mut queries: Vec<String> = Vec::new();
    if artist.is_empty() {
        queries.push(title.to_string());
    } else {
        queries.push(format!("{artist} {title}"));
        queries.push(title.to_string());
    }
    let stripped = title
        .split_once(" (")
        .map(|(a, _)| a)
        .unwrap_or(title)
        .split_once(" - ")
        .map(|(a, _)| a)
        .unwrap_or_else(|| title.split_once(" (").map(|(a, _)| a).unwrap_or(title))
        .trim()
        .to_string();
    if !stripped.is_empty() && stripped != title {
        if artist.is_empty() {
            queries.push(stripped);
        } else {
            queries.push(format!("{artist} {stripped}"));
        }
    }
    queries
}

/// Le titre réduit à son NOYAU : normalisé, puis coupé avant la première
/// parenthèse, le premier crochet ou le premier « - ». « Heroes (2017
/// Remaster) » et « Heroes » ont le même noyau : ce sont deux éditions du
/// même morceau, pas deux morceaux.
fn titre_noyau(title: &str) -> String {
    let n = normalize(title);
    let coupe = [" (", " [", " - "]
        .iter()
        .filter_map(|sep| n.find(sep))
        .min()
        .unwrap_or(n.len());
    n[..coupe].trim().to_string()
}

/// Deux candidats désignent-ils le même morceau (même artiste, même noyau de
/// titre) ? Une réédition remasterisée n'est pas une ambiguïté.
fn meme_morceau(a: &StreamTrack, b: &StreamTrack) -> bool {
    normalize(&a.artist) == normalize(&b.artist) && titre_noyau(&a.title) == titre_noyau(&b.title)
}

/// Le verdict d'un favori, tel que l'écran le montre.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StatutAppariement {
    /// Correspondance sûre (score ≥ [`MATCH_ACCEPT_SCORE`]), sans rivale.
    Matched,
    /// Seule la bande 0,6–0,7 a répondu : à faire valider.
    Approximate,
    /// Correspondance sûre, mais un AUTRE morceau (autre artiste ou autre
    /// titre) score presque autant : à faire choisir.
    Ambiguous,
    /// Le service n'a rien rendu d'assez proche.
    NotFound,
    /// Toutes les recherches ont échoué (réseau, session) : rien n'est conclu.
    SearchFailed,
}

impl StatutAppariement {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Matched => "matched",
            Self::Approximate => "approximate",
            Self::Ambiguous => "ambiguous",
            Self::NotFound => "not_found",
            Self::SearchFailed => "search_failed",
        }
    }
}

/// Ce que la recherche d'un favori chez le service a donné.
#[derive(Debug, Clone)]
pub struct AppariementFavori {
    /// La correspondance retenue et son score.
    pub meilleur: Option<(StreamTrack, f64)>,
    /// Les candidats suivants, classés, au plus [`MAX_ALTERNATIVES`].
    pub alternatives: Vec<(StreamTrack, f64)>,
    /// Vrai quand AUCUNE requête n'a abouti (toutes en erreur).
    pub recherche_en_echec: bool,
}

impl AppariementFavori {
    pub fn statut(&self) -> StatutAppariement {
        let Some((tete, score)) = self.meilleur.as_ref() else {
            return if self.recherche_en_echec {
                StatutAppariement::SearchFailed
            } else {
                StatutAppariement::NotFound
            };
        };
        if *score < MATCH_ACCEPT_SCORE {
            return StatutAppariement::Approximate;
        }
        let rivale = self
            .alternatives
            .iter()
            .any(|(alt, s)| *s >= score - ECART_AMBIGUITE && !meme_morceau(tete, alt));
        if rivale {
            StatutAppariement::Ambiguous
        } else {
            StatutAppariement::Matched
        }
    }
}

/// Chercher un favori radio chez le service et l'apparier.
///
/// Les requêtes de [`requetes_pour_favori`] sont essayées dans l'ordre ; on
/// s'arrête à la première qui produit une correspondance sûre. À défaut, on
/// garde la meilleure correspondance approximative rencontrée. C'est
/// exactement le choix que faisait la création « en un geste » avant
/// l'extraction (même ordre, même seuil, même matcher) : les deux chemins
/// désignent toujours la même piste.
///
/// `service` ne sert qu'au journal.
pub async fn apparier_favori_radio(
    svc: &dyn StreamingService,
    service: &str,
    title: &str,
    artist: &str,
) -> AppariementFavori {
    let queries = requetes_pour_favori(title, artist);
    let mut meilleur: Option<(StreamTrack, f64)> = None;
    let mut alternatives: Vec<(StreamTrack, f64)> = Vec::new();
    let mut echecs = 0usize;

    for q in &queries {
        let results = match svc.search(q, LIMITE_RECHERCHE_APPARIEMENT).await {
            Ok(r) => r,
            Err(e) => {
                echecs += 1;
                tracing::warn!(service = %service, query = %q, error = %e, "radio_fav_search_failed");
                continue;
            }
        };
        let n = results.tracks.len();
        let classement: Vec<(StreamTrack, f64)> =
            best_stream_matches_scored(title, artist, "", 0, &results.tracks, MAX_ALTERNATIVES + 1)
                .into_iter()
                .map(|(t, s)| (t.clone(), s))
                .collect();
        let Some((tete, score)) = classement.first().cloned() else {
            tracing::warn!(service = %service, query = %q, results = n, "radio_fav_match_rejected");
            continue;
        };
        if score >= MATCH_ACCEPT_SCORE {
            tracing::info!(service = %service, query = %q, results = n, score, "radio_fav_match_ok");
            meilleur = Some((tete, score));
            alternatives = classement.into_iter().skip(1).collect();
            break;
        }
        tracing::info!(service = %service, query = %q, results = n, score, "radio_fav_match_approx");
        if meilleur.as_ref().is_none_or(|(_, s)| score > *s) {
            meilleur = Some((tete, score));
            alternatives = classement.into_iter().skip(1).collect();
        }
    }

    AppariementFavori {
        meilleur,
        alternatives,
        recherche_en_echec: !queries.is_empty() && echecs == queries.len(),
    }
}

/// Les identifiants dans l'ordre reçu, vides et doublons retirés. Deux
/// favoris captés sur deux stations peuvent désigner la même piste Qobuz :
/// elle n'entre qu'une fois.
pub fn dedoublonner_titres(ids: &[String]) -> Vec<String> {
    let mut vus = std::collections::HashSet::new();
    ids.iter()
        .map(|id| id.trim())
        .filter(|id| !id.is_empty() && vus.insert(id.to_string()))
        .map(str::to_string)
        .collect()
}

/// Ce que la création a produit chez le service.
#[derive(Debug, Clone, Serialize)]
pub struct PlaylistCreee {
    pub playlist_id: String,
    /// Titres versés (après dédoublonnage).
    pub ajoutes: usize,
    /// Doublons écartés avant l'envoi.
    pub doublons: usize,
    /// Titres retenus DÉJÀ présents dans la playlist cible, donc non
    /// rajoutés (ajout à une playlist existante ; toujours 0 à la création).
    pub deja_presents: usize,
    /// L'ajout a échoué APRÈS la création : la playlist existe, vide ou
    /// partielle. Le motif est rendu, pas avalé.
    pub erreur_ajout: Option<String>,
}

/// Créer la playlist `name` chez le service et y verser `track_ids`.
///
/// Rend `Err` si la création elle-même échoue (rien n'existe chez le
/// service). Un échec de l'AJOUT est rendu dans [`PlaylistCreee::erreur_ajout`] :
/// la playlist existe, et l'écran doit le dire au lieu de prétendre qu'elle
/// est pleine.
pub async fn creer_playlist_avec_titres(
    svc: &dyn StreamingService,
    name: &str,
    track_ids: &[String],
) -> Result<PlaylistCreee, String> {
    let ids = dedoublonner_titres(track_ids);
    let doublons = track_ids.len() - ids.len();
    let playlist_id = svc
        .create_playlist(name, Some(DESCRIPTION_PLAYLIST))
        .await
        .map_err(|e| e.to_string())?;
    let (ajoutes, erreur_ajout) = if ids.is_empty() {
        (0, None)
    } else {
        match svc.add_tracks_to_playlist(&playlist_id, &ids).await {
            Ok(n) => (n, None),
            Err(e) => {
                tracing::warn!(error = %e, "radio_fav_add_tracks_failed");
                (0, Some(e.to_string()))
            }
        }
    };
    Ok(PlaylistCreee {
        playlist_id,
        ajoutes,
        doublons,
        deja_presents: 0,
        erreur_ajout,
    })
}

/// Verser `track_ids` dans une playlist EXISTANTE du compte (décision de
/// Bertrand du 14/10/2026 : « nouvelle playlist » ou une playlist Qobuz
/// existante).
///
/// Les titres déjà présents dans la playlist cible ne sont pas ajoutés une
/// seconde fois : on relit d'abord son contenu. Si cette lecture échoue, on
/// n'ajoute RIEN (`Err`) — ajouter à l'aveugle créerait précisément les
/// doublons que la règle interdit.
pub async fn ajouter_a_playlist_existante(
    svc: &dyn StreamingService,
    playlist_id: &str,
    track_ids: &[String],
) -> Result<PlaylistCreee, String> {
    let ids = dedoublonner_titres(track_ids);
    let doublons = track_ids.len() - ids.len();
    let presents: std::collections::HashSet<String> = svc
        .get_playlist_tracks(playlist_id)
        .await
        .map_err(|e| e.to_string())?
        .into_iter()
        .map(|t| t.id)
        .collect();
    let nouveaux: Vec<String> = ids
        .into_iter()
        .filter(|id| !presents.contains(id))
        .collect();
    let deja_presents = track_ids.len() - doublons - nouveaux.len();
    let (ajoutes, erreur_ajout) = if nouveaux.is_empty() {
        (0, None)
    } else {
        match svc.add_tracks_to_playlist(playlist_id, &nouveaux).await {
            Ok(n) => (n, None),
            Err(e) => {
                tracing::warn!(error = %e, "radio_fav_add_tracks_failed");
                (0, Some(e.to_string()))
            }
        }
    };
    Ok(PlaylistCreee {
        playlist_id: playlist_id.to_string(),
        ajoutes,
        doublons,
        deja_presents,
        erreur_ajout,
    })
}

#[cfg(test)]
#[path = "appariement_favoris_radio_tests.rs"]
mod tests;
