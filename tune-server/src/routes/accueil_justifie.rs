//! L'accueil qui dit POURQUOI, et qui entend « Pas pour moi »
//! (renesenses/tune-web-client#916, FabienM, fil forum 1527, go de Bertrand
//! du 12/10/2026).
//!
//! # La raison structurée : `raison`
//!
//! Chaque élément d'une rangée qui SAIT pourquoi il le propose porte un champ
//! `raison` — un objet, jamais une phrase : la phrase est faite par le client,
//! dans la langue de l'interface. Les types, et seulement eux :
//!
//! | `type`            | champs                  | rangée — ce que la logique sait VRAIMENT |
//! |-------------------|-------------------------|------------------------------------------|
//! | `genre_frequent`  | `genre`                 | `/home/recommendations` : l'album est d'un des 5 genres les plus présents dans `listen_history`, et n'a jamais été écouté |
//! | `artiste_favori`  | `artiste`               | `/home/artist-releases` : l'artiste est en favori (local ou chez un service) |
//! | `artiste_possede` | `artiste`, `albums`     | `/home/artist-releases` : `albums` albums de l'artiste dans la bibliothèque |
//! | `autre_version`   | `titre`                 | `/home/other-versions` : une autre version de `titre`, écouté récemment |
//! | `ajout_recent`    | `ajoute_le` (s, option) | `/home/new-in-library`, `/home/recently-added` : la date d'AJOUT |
//! | `hasard`          | —                       | `/home/recommendations`, repli : tiré au hasard dans la bibliothèque (go du 13/10) |
//! | `reprise`         | `ecoute_le` (s, option) | `/home/continue-listening` : la DERNIÈRE écoute de ce contexte |
//! | `souvent_ecoute`  | `ecoutes`               | `/home`, section `top` : le nombre d'écoutes que compte l'historique |
//! | `radio_favorite`  | —                       | `/home`, section `radios` : la station est en favori |
//! | `radio_ecoutee`   | `ecoute_le` (s, option) | `/home`, section `radios` : écoutée récemment, hors favoris |
//!
//! `/home/top-mixes` porte `genre_frequent` au niveau du MIX : il est bâti
//! sur un des genres les plus présents dans l'historique.
//!
//! Ce qu'on n'a PAS inventé, et pourquoi :
//!
//! - **« Parce que vous écoutez [artiste] »** (`artiste_ecoute`) : aucune
//!   rangée ne choisit aujourd'hui un élément à partir des artistes ÉCOUTÉS.
//!   Les nouveautés partent des favoris et de la bibliothèque, pas de
//!   l'historique ; le dire serait mentir.
//! - **Le repli au hasard** de `/home/recommendations` (`reason: "random"`)
//!   ne disait rien jusqu'au 13/10. Go de Bertrand ce jour-là : il dit
//!   désormais ce qu'il est — `hasard`, « tiré au hasard » —, ce qui est vrai,
//!   et ne fait pas passer le hasard pour un choix.
//!
//! Le champ historique `reason` (`"random"` / `"genre_match"`) reste tel quel
//! pour les clients déployés : il est lu comme une chaîne.
//!
//! # « Pas pour moi » : `/home/dismissed-suggestions`
//!
//! Par profil (`X-Profile-Id`), en base (`suggestions_ecartees`, migration
//! SQLite 144 / PG 110). Les rangées de RECOMMANDATION — recommandations,
//! nouveautés de vos artistes, autres versions — ne proposent plus ce que le
//! profil a écarté. Depuis le go du 13/10, « Continuer l'écoute », les mix
//! par genre et la page composée `/home` aussi (reprises, plus écoutés, à
//! découvrir). Les AJOUTS RÉCENTS ne filtrent pas : un album ajouté hier
//! reste ajouté hier.

use axum::Json;
use axum::extract::State;
use serde_json::{Value, json};
use tune_core::db::suggestions_ecartees::{Ecartees, SuggestionEcartee, SuggestionsEcarteesRepo};

use crate::error::AppError;
use crate::routes::active_profile::ActiveProfile;
use crate::state::AppState;

// ── Les raisons ────────────────────────────────────────────────────────────

/// L'album est d'un genre que l'historique compte parmi les plus écoutés.
pub(crate) fn raison_genre_frequent(genre: &str) -> Option<Value> {
    let g = genre.trim();
    (!g.is_empty()).then(|| json!({"type": "genre_frequent", "genre": g}))
}

/// L'artiste d'une nouveauté : favori d'abord, sinon possédé, sinon rien.
pub(crate) fn raison_artiste(artiste: &str, favori: bool, albums_possedes: i64) -> Option<Value> {
    let a = artiste.trim();
    if a.is_empty() {
        return None;
    }
    if favori {
        Some(json!({"type": "artiste_favori", "artiste": a}))
    } else if albums_possedes > 0 {
        Some(json!({"type": "artiste_possede", "artiste": a, "albums": albums_possedes}))
    } else {
        None
    }
}

/// Une autre version d'un titre écouté récemment.
pub(crate) fn raison_autre_version(titre: &str) -> Option<Value> {
    let t = titre.trim();
    (!t.is_empty()).then(|| json!({"type": "autre_version", "titre": t}))
}

/// Ajouté récemment ; `ajoute_le` en secondes depuis l'époque quand on l'a.
pub(crate) fn raison_ajout_recent(ajoute_le: Option<f64>) -> Value {
    match ajoute_le.filter(|s| s.is_finite() && *s > 0.0) {
        Some(s) => json!({"type": "ajout_recent", "ajoute_le": s.floor() as i64}),
        None => json!({"type": "ajout_recent"}),
    }
}

/// Tiré au hasard dans la bibliothèque : le repli des recommandations.
pub(crate) fn raison_hasard() -> Value {
    json!({"type": "hasard"})
}

/// Une date de `listen_history` / `radio_stations` (ISO-8601, `Z` ou non,
/// `T` ou espace) en secondes depuis l'époque. `None` si illisible.
pub(crate) fn secondes_de(date: &str) -> Option<i64> {
    let d = date.trim();
    if d.is_empty() {
        return None;
    }
    if let Ok(t) = chrono::DateTime::parse_from_rfc3339(d) {
        return Some(t.timestamp());
    }
    let sans_z = d.trim_end_matches('Z');
    ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%d %H:%M:%S%.f"]
        .iter()
        .find_map(|f| chrono::NaiveDateTime::parse_from_str(sans_z, f).ok())
        .map(|n| n.and_utc().timestamp())
}

/// Une reprise d'écoute : le contexte a été écouté pour la dernière fois à
/// `ecoute_le`. Sans date lisible, la raison reste (« écouté récemment »).
pub(crate) fn raison_reprise(ecoute_le: Option<&str>) -> Value {
    match ecoute_le.and_then(secondes_de) {
        Some(s) => json!({"type": "reprise", "ecoute_le": s}),
        None => json!({"type": "reprise"}),
    }
}

/// Un des titres les plus écoutés : `ecoutes` écoutes dans l'historique.
pub(crate) fn raison_souvent_ecoute(ecoutes: i64) -> Option<Value> {
    (ecoutes > 0).then(|| json!({"type": "souvent_ecoute", "ecoutes": ecoutes}))
}

/// Une radio de la rangée `radios` de `/home` : favorite, ou écoutée
/// récemment.
pub(crate) fn raison_radio(favorite: bool, derniere_ecoute: Option<&str>) -> Value {
    if favorite {
        return json!({"type": "radio_favorite"});
    }
    match derniere_ecoute.and_then(secondes_de) {
        Some(s) => json!({"type": "radio_ecoutee", "ecoute_le": s}),
        None => json!({"type": "radio_ecoutee"}),
    }
}

// ── Le filtre ──────────────────────────────────────────────────────────────

/// Ce que le profil qui regarde a écarté.
pub(crate) fn ecartees_du_profil(state: &AppState, profile: &ActiveProfile) -> Ecartees {
    SuggestionsEcarteesRepo::with_backend(state.backend.clone()).ensemble(profile.id())
}

/// Un album de recommandation (`id` local, `artist_name`) est-il écarté ?
pub(crate) fn album_local_ecarte(e: &Ecartees, album: &Value) -> bool {
    album["id"].as_i64().is_some_and(|id| e.album_local(id))
        || album["artist_name"].as_str().is_some_and(|a| e.artiste(a))
}

/// Une entrée de « Continuer l'écoute » est-elle écartée ? Son album local,
/// son artiste, ou l'objet de service qu'elle rouvre (album, playlist).
pub(crate) fn reprise_ecartee(e: &Ecartees, item: &Value) -> bool {
    if e.est_vide() {
        return false;
    }
    if item["album_id"]
        .as_i64()
        .is_some_and(|id| e.album_local(id))
        || item["artist_name"].as_str().is_some_and(|a| e.artiste(a))
    {
        return true;
    }
    let source = item["source"].as_str().unwrap_or_default();
    let contexte = item["context_id"].as_str().unwrap_or_default();
    match item["context_type"].as_str() {
        Some("album") => e.album_de_service(source, contexte),
        Some("playlist") => e.playlist_de_service(source, contexte),
        _ => false,
    }
}

/// Une piste proposée (mix, plus écoutés) est-elle écartée — son album
/// local (`album_id`) ou son artiste (`artist_name`) ?
pub(crate) fn piste_ecartee(e: &Ecartees, piste: &Value) -> bool {
    !e.est_vide()
        && (piste["album_id"]
            .as_i64()
            .is_some_and(|id| e.album_local(id))
            || piste["artist_name"].as_str().is_some_and(|a| e.artiste(a)))
}

/// Épure les groupes d'« Autres versions » : versions locales et de service
/// écartées retirées, groupe d'un artiste écarté retiré, groupe vidé retiré.
/// Pose la `raison` de chaque groupe restant.
pub(crate) fn epurer_autres_versions(groupes: Vec<Value>, e: &Ecartees) -> Vec<Value> {
    groupes
        .into_iter()
        .filter_map(|mut g| {
            if g["artist_name"].as_str().is_some_and(|a| e.artiste(a)) {
                return None;
            }
            if !e.est_vide() {
                if let Some(v) = g["versions"].as_array_mut() {
                    v.retain(|x| !x["album_id"].as_i64().is_some_and(|id| e.album_local(id)));
                }
                if let Some(s) = g["streaming"].as_array_mut() {
                    s.retain(|x| {
                        let service = x["service"].as_str().unwrap_or_default();
                        let album = x["album_id"].as_str().unwrap_or_default();
                        let artiste = x["artist_name"].as_str().unwrap_or_default();
                        !(e.album_de_service(service, album) || e.artiste(artiste))
                    });
                }
                let vide = |k: &str| g[k].as_array().is_none_or(|a| a.is_empty());
                if vide("versions") && vide("streaming") {
                    return None;
                }
            }
            if let Some(r) = g["title"].as_str().and_then(raison_autre_version) {
                g["raison"] = r;
            }
            Some(g)
        })
        .collect()
}

// ── Les routes ─────────────────────────────────────────────────────────────

/// `GET /home/dismissed-suggestions` — ce que le profil qui agit a écarté,
/// le plus récent d'abord. La liste des Réglages (« Suggestions écartées »).
pub(crate) async fn lister(
    State(state): State<AppState>,
    profile: ActiveProfile,
) -> Result<Json<Value>, AppError> {
    let items = SuggestionsEcarteesRepo::with_backend(state.backend.clone())
        .lister(profile.id())
        .map_err(AppError::internal)?;
    Ok(Json(json!({
        "profile_id": profile.id(),
        "total": items.len(),
        "items": items,
    })))
}

/// `POST /home/dismissed-suggestions` — « Pas pour moi ». Corps :
/// `{item_type, source, item_id, title?, subtitle?, cover_url?}`. Rend la
/// désignation telle qu'elle est rangée (normalisée) : c'est elle que le
/// client renvoie pour annuler. 400 si elle ne désigne rien.
pub(crate) async fn ecarter(
    State(state): State<AppState>,
    profile: ActiveProfile,
    Json(body): Json<SuggestionEcartee>,
) -> Result<Json<Value>, AppError> {
    let repo = SuggestionsEcarteesRepo::with_backend(state.backend.clone());
    match repo
        .ecarter(profile.id(), &body)
        .map_err(AppError::internal)?
    {
        Some(rangee) => Ok(Json(json!({
            "profile_id": profile.id(),
            "dismissed": true,
            "item": rangee,
        }))),
        None => Err(AppError::bad_request(
            "item_type must be album, artist or playlist, with a usable item_id \
             (a library album id, a service source_id, or an artist name); \
             a playlist must come from a streaming service",
        )),
    }
}

/// `POST /home/dismissed-suggestions/restore` — « Annuler », ou le bouton
/// « Rétablir » des Réglages. Même corps que l'écart. Idempotent : rétablir
/// ce qui ne l'était pas rend `restored: false`.
pub(crate) async fn retablir(
    State(state): State<AppState>,
    profile: ActiveProfile,
    Json(body): Json<SuggestionEcartee>,
) -> Result<Json<Value>, AppError> {
    let restored = SuggestionsEcarteesRepo::with_backend(state.backend.clone())
        .retablir(profile.id(), &body)
        .map_err(AppError::internal)?;
    Ok(Json(json!({
        "profile_id": profile.id(),
        "restored": restored,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn les_dates_de_l_historique_se_lisent_dans_leurs_trois_formes() {
        let attendu = Some(1_760_270_400);
        assert_eq!(secondes_de("2025-10-12T12:00:00Z"), attendu);
        assert_eq!(secondes_de("2025-10-12T12:00:00"), attendu);
        assert_eq!(secondes_de("2025-10-12 12:00:00"), attendu);
        assert_eq!(secondes_de("hier"), None);
        assert_eq!(
            raison_reprise(Some("2025-10-12T12:00:00Z")),
            json!({"type": "reprise", "ecoute_le": 1_760_270_400})
        );
        assert_eq!(raison_reprise(None), json!({"type": "reprise"}));
        assert_eq!(raison_souvent_ecoute(0), None);
        assert_eq!(
            raison_radio(true, Some("2025-10-12T12:00:00Z")),
            json!({"type": "radio_favorite"})
        );
    }

    #[test]
    fn une_reprise_s_ecarte_par_son_album_son_artiste_ou_son_objet_de_service() {
        let mut e = Ecartees::default();
        assert!(!reprise_ecartee(&e, &json!({"album_id": 4})));
        e = ecartees_de(&[
            ("album", "local", "4"),
            ("playlist", "qobuz", "pl-1"),
            ("artist", "nom", "miles davis"),
        ]);
        assert!(reprise_ecartee(&e, &json!({"album_id": 4})));
        assert!(reprise_ecartee(&e, &json!({"artist_name": "Miles  Davis"})));
        assert!(reprise_ecartee(
            &e,
            &json!({"context_type": "playlist", "source": "Qobuz", "context_id": "pl-1"})
        ));
        assert!(!reprise_ecartee(
            &e,
            &json!({"context_type": "album", "source": "qobuz", "context_id": "pl-1"})
        ));
        assert!(piste_ecartee(
            &e,
            &json!({"album_id": 4, "artist_name": "X"})
        ));
        assert!(!piste_ecartee(
            &e,
            &json!({"album_id": 5, "artist_name": "X"})
        ));
    }

    fn ecartees_de(liste: &[(&str, &str, &str)]) -> Ecartees {
        let mut e = Ecartees::default();
        for (t, s, i) in liste {
            e.ajouter(&SuggestionEcartee {
                item_type: (*t).into(),
                source: (*s).into(),
                item_id: (*i).into(),
                title: None,
                subtitle: None,
                cover_url: None,
                created_at: None,
            });
        }
        e
    }

    #[test]
    fn le_vide_n_est_pas_une_raison() {
        assert_eq!(raison_genre_frequent("  "), None);
        assert_eq!(raison_artiste("Miles Davis", false, 0), None);
        assert_eq!(raison_artiste("", true, 3), None);
        assert_eq!(raison_autre_version(""), None);
        assert_eq!(
            raison_ajout_recent(Some(f64::NAN)),
            json!({"type": "ajout_recent"})
        );
    }

    #[test]
    fn le_favori_passe_avant_la_possession() {
        assert_eq!(
            raison_artiste("Miles Davis", true, 5),
            Some(json!({"type": "artiste_favori", "artiste": "Miles Davis"}))
        );
        assert_eq!(
            raison_artiste("Miles Davis", false, 5),
            Some(json!({"type": "artiste_possede", "artiste": "Miles Davis", "albums": 5}))
        );
    }
}
