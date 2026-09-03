//! `GET /search` — recherche fédérée : bibliothèque locale, radios, services.
//!
//! # #3189 — le compteur disait la longueur de la liste, pas le nombre de
//! correspondances
//!
//! jfpaquet (forum, fil 1644, 02/09/2026 — 0.9.130 Windows/PostgreSQL,
//! 77 291 pistes) cherche « Autumn Leaves » : Tune annonce « Pistes 50 »,
//! Everything en trouve 58 dans UN de ses dossiers et 52 dans un autre.
//!
//! Le 50 n'était pas un compte : c'était `limit`. La réponse ne portait ni
//! total, ni `has_more`, ni pagination — l'écran affichait la longueur de ce
//! qu'il avait reçu, et RIEN ne disait que la liste était coupée. Le plafond
//! venait de #2036, où il est justifié pour les services de streaming
//! (« 50 est le plafond de page de l'API Qobuz ») ; la même constante bornait
//! la bibliothèque locale, où cette contrainte n'a aucun sens. Avant #2036 le
//! plafond local était de 30 : le défaut préexistait, #2036 l'a relevé sans
//! le lever.
//!
//! Relever la limite ne l'aurait pas levé non plus : à 77 291 pistes il y
//! aura toujours un plafond, et le compteur mentirait toujours. Ce qui manque
//! n'est pas du volume, c'est de l'INFORMATION. La route rend donc trois
//! choses de plus, sous `local` :
//!
//!   * `totals` — un `COUNT` séparé, sur le MÊME prédicat que la liste et
//!     indépendant de `limit`. C'est le nombre à afficher.
//!   * `has_more` — dérivé, mais explicite : un client qui n'exploite pas les
//!     totaux sait quand même qu'il manque quelque chose.
//!   * `limit` / `offset` — ce que la page rendue vaut, pour que la suite
//!     soit demandable (`?offset=`).
//!
//! Les trois, et pas seulement l'un d'eux : le total répond à « combien ? »
//! (le défaut signalé), l'offset répond à « et le reste ? », et `has_more`
//! rend la réponse lisible sans arithmétique.
//!
//! **Ce qui ne change pas** : `local.artists`, `local.albums` et
//! `local.tracks` restent des tableaux, au même endroit, avec le même contenu
//! pour `offset = 0` (le défaut). Un client 0.9.130 déjà installé, qui appelle
//! `GET /search?q=…&limit=30` et lit `local.tracks` comme un tableau, voit
//! exactement ce qu'il voyait ; les clés neuves lui sont invisibles.
//!
//! **Les services de streaming ne sont PAS paginés ici** : `limit` continue de
//! leur être passé tel quel, sans `offset`, et ils n'entrent dans aucun total.
//! Le plafond de page de Qobuz reste ce qu'il est, et le contrat de #2036 est
//! intact — la pagination d'un service passe par `SearchPage`, pas par cette
//! route.
//!
//! # #3226 — `sources` ne gouvernait QUE la moitié streaming
//!
//! Reivax66 (forum, fil 1647, 02/09/2026 — 0.9.130 Windows/SQLite) : dans la
//! recherche latérale, la pilule « Qobuz » rend exactement ce que rend la
//! pilule « Tous ».
//!
//! Ce n'était pas une coïncidence de son écran : `sources` était lu APRÈS les
//! quatre recherches locales et ne servait qu'à filtrer la boucle des
//! services. Le bloc `local` — et `radios` avec lui — partait donc dans TOUTES
//! les réponses, quelle que soit la valeur du paramètre. « Local » semblait
//! marcher parce qu'il EXCLUAIT le service ; « Qobuz » rendait `local + qobuz`,
//! et comme Reivax66 n'a qu'un seul service authentifié, c'était mot pour mot
//! le contenu de « Tous ».
//!
//! Le contrat, désormais :
//!
//! | `sources`                 | bloc `local` + `radios` | services            |
//! |---------------------------|-------------------------|---------------------|
//! | absent                    | rendus                  | tous ceux authentifiés |
//! | `local`                   | rendus                  | aucun               |
//! | `all`                     | rendus                  | tous ceux authentifiés |
//! | `qobuz` (un service)      | **vides**               | ce service          |
//! | `local,qobuz`             | rendus                  | ce service          |
//! | valeur inconnue, ou vide  | **vides**               | aucun               |
//!
//! **Le paramètre absent ne change pas** : c'est la pilule « Tous », le seul
//! cas qui marchait, et le seul témoin de non-régression qui vaille. Présent,
//! `sources` est une liste blanche EXPLICITE, et le local y entre sous son
//! propre jeton — exactement la règle que le client applique déjà de son côté
//! pour ses playlists (`includeLocal = !activeSources ||
//! activeSources.includes('local')`, `SearchView.svelte`). Une valeur inconnue
//! ne sélectionne donc rien, ni service ni local : c'est déjà ce que la boucle
//! des services faisait, et la moitié streaming ne bouge pas d'un octet.
//!
//! **La clé `local` reste PRÉSENTE, avec des tableaux vides** — jamais absente.
//! Un champ absent et un champ vide ne se comportent pas pareil en JavaScript,
//! et `federatedSearch` fait `if (result.local) result.local.tracks =
//! mapStreamingTracks(result.local.tracks)` : `local` présent mais amputé de
//! `tracks` planterait l'écran. La forme rendue est donc intégralement celle
//! de #3189, avec des zéros dedans.
//!
//! **Ne rien calculer plutôt que jeter** : quand le local n'est pas demandé,
//! les trois `search_page`, les trois `COUNT`, la recherche par métadonnées et
//! la recherche de radios ne sont pas exécutés du tout. Huit requêtes SQL
//! économisées sur chaque recherche d'un service seul.

use std::collections::HashMap;

use axum::extract::{Query, State};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::artist_repo::ArtistRepo;
use tune_core::db::radio_repo::RadioRepo;
use tune_core::db::track_metadata_repo::TrackMetadataRepo;
use tune_core::db::track_repo::TrackRepo;

use crate::state::AppState;

/// Plafond des `COUNT` de la bibliothèque locale.
///
/// Un total FAUX serait pire que pas de total : le compte est donc exact
/// jusqu'à cette valeur, et au-delà il est annoncé comme une borne INFÉRIEURE
/// (`totals_capped` vrai, « au moins 5 000 »). Le moteur cesse de lire dès
/// la 5 000ᵉ correspondance, ce qui borne le coût d'une requête d'un seul
/// caractère sur une grande bibliothèque.
///
/// 5 000, et pas « pas de plafond du tout » : chiffres MESURÉS le 02/09/2026
/// sur une base SQLite de 77 291 pistes — la taille de celle de jfpaquet —,
/// profil `release`, meilleure de trois passes à cache chaud :
///
/// | requête   | correspondances | page 50 | COUNT borné 5 000 | COUNT non borné |
/// |-----------|-----------------|---------|-------------------|-----------------|
/// | « Love »  |           5 946 |  6,2 ms |            89 ms  |         106 ms  |
/// | « Morceau »|         71 345 | 17,3 ms |            25 ms  |         135 ms  |
/// | « e »     |          77 291 |  0,5 ms |             4,2 ms|          64 ms  |
///
/// Le prédicat porte des `LIKE` sur `ar.name`, `t.genre` et `t.composer` en OU
/// avec la passe FTS : aucun index ne le couvre en entier, tout compte lit donc
/// la table. Le plafond ne rend pas le cas rare moins cher (« Love » : 89 ms
/// contre 106) — il borne le cas FRÉQUENT, celui d'une requête courte qui
/// ramène la moitié de la bibliothèque : 5,4× sur « Morceau », 15× sur « e ».
/// Et surtout il rend le coût indépendant de la taille de la bibliothèque, là
/// où le compte exact croît avec elle sans limite.
///
/// Les trois comptes ajoutent au total ≈ 135 ms au pire à une recherche dont la
/// page coûte 3 à 17 ms. C'est le prix d'un compteur qui ne ment pas.
///
/// Aucun écran n'a besoin de distinguer « 5 000 » de « 12 000 » : il a besoin
/// de savoir que 50 n'est pas le compte.
const PLAFOND_DE_COMPTAGE: i64 = 5_000;

#[derive(Deserialize)]
struct SearchParams {
    q: String,
    limit: Option<i64>,
    /// Rang de la première ligne locale rendue (#3189). Absent = 0, donc le
    /// comportement d'avant. Ne s'applique QU'À la bibliothèque locale : les
    /// radios et les services n'ont pas de curseur ici.
    offset: Option<i64>,
    sources: Option<String>,
}

/// Le jeton qui désigne la bibliothèque de la machine dans `sources`. C'est
/// celui que les clients envoient déjà pour leur pilule « Local ».
const JETON_LOCAL: &str = "local";

/// Le joker : « toutes les sources ». Le serveur l'acceptait déjà pour les
/// services ; il vaut donc aussi pour le local, sans quoi `sources=all`
/// rendrait moins que `sources` absent.
const JETON_TOUTES: &str = "all";

/// La bibliothèque locale (et les radios) entre-t-elle dans cette réponse ?
///
/// La liste est prise en PARAMÈTRE, et non relue depuis `SearchParams` : c'est
/// ce qui permet d'éprouver la règle seule, sans monter un routeur. `None` =
/// paramètre absent = « Tous », le comportement d'avant #3226.
///
/// Une liste présente est une sélection EXPLICITE : elle ne se replie pas sur
/// « tout » quand elle ne reconnaît rien. Un `sources=service-inexistant` ne
/// rend donc aucun service — ce qu'il faisait déjà — ET aucun local, ce qui
/// est la même règle appliquée aux deux moitiés.
fn le_local_est_demande(sources: Option<&[String]>) -> bool {
    match sources {
        None => true,
        Some(liste) => liste.iter().any(|s| s == JETON_LOCAL || s == JETON_TOUTES),
    }
}

pub fn router() -> Router<AppState> {
    Router::new().route("/", get(federated_search))
}

async fn federated_search(
    State(state): State<AppState>,
    Query(p): Query<SearchParams>,
) -> Json<Value> {
    let limit = p.limit.unwrap_or(20);
    let offset = p.offset.unwrap_or(0).max(0);

    // #3226 — LU EN PREMIER. Tant que ce parsing vivait sous les recherches
    // locales, il ne pouvait par construction gouverner qu'elles seules.
    let requested_sources: Option<Vec<String>> = p
        .sources
        .as_deref()
        .map(|s| s.split(',').map(|s| s.trim().to_string()).collect());
    let local_demande = le_local_est_demande(requested_sources.as_deref());

    let artist_repo = ArtistRepo::with_backend(state.backend.clone());
    let album_repo = AlbumRepo::with_backend(state.backend.clone());
    let track_repo = TrackRepo::with_backend(state.backend.clone());

    let (artists, albums, tracks, radios) = if local_demande {
        (
            artist_repo
                .search_page(&p.q, limit, offset)
                .unwrap_or_default(),
            album_repo
                .search_page(&p.q, limit, offset)
                .unwrap_or_default(),
            track_repo
                .search_page(&p.q, limit, offset)
                .unwrap_or_default(),
            RadioRepo::with_backend(state.backend.clone())
                .search(&p.q)
                .unwrap_or_default(),
        )
    } else {
        // Pas « calculer puis jeter » : les requêtes ne partent pas.
        (Vec::new(), Vec::new(), Vec::new(), Vec::new())
    };

    // Les totaux. Un `COUNT` qui échoue ne doit pas rendre 0 alors qu'une
    // liste non vide est servie : le repli est « au moins ce qu'on rend »,
    // qui reste vrai.
    let plancher = |liste: usize| offset.saturating_add(liste as i64);
    let (total_artists, total_albums, total_tracks) = if local_demande {
        (
            artist_repo
                .search_count(&p.q, PLAFOND_DE_COMPTAGE)
                .unwrap_or_else(|_| plancher(artists.len())),
            album_repo
                .search_count(&p.q, PLAFOND_DE_COMPTAGE)
                .unwrap_or_else(|_| plancher(albums.len())),
            track_repo
                .search_count(&p.q, PLAFOND_DE_COMPTAGE)
                .unwrap_or_else(|_| plancher(tracks.len())),
        )
    } else {
        // Zéro, et non « le plancher » : rien n'a été cherché, donc rien n'est
        // annoncé. Un total non nul en regard d'une liste vide ferait afficher
        // « Pistes 137 » sous zéro ligne.
        (0, 0, 0)
    };

    // « Y a-t-il une suite ? »
    //
    // Sous le plafond, le total est exact et tranche seul. AU plafond il ne
    // tranche plus rien — il ne sait pas compter au-delà — et c'est alors la
    // FORME de la page qui parle : une page pleine peut être suivie, une page
    // courte est la dernière. Sans cette bascule, un client arrivé au-delà du
    // plafond se verrait dire « c'est fini » alors qu'il reste des lignes.
    let a_la_suite = |rendus: usize, total: i64| {
        if total >= PLAFOND_DE_COMPTAGE {
            limit > 0 && rendus as i64 >= limit
        } else {
            plancher(rendus) < total
        }
    };

    // --- Extended metadata search ---
    //
    // Cet apport n'est PAS paginé : `search_by_value` rend des correspondances
    // sur les VALEURS de métadonnées (compositeur, label, paroles…), sans ordre
    // exploitable comme curseur. Le servir à chaque page rendrait les mêmes
    // pistes page après page — exactement le doublon que la pagination doit
    // exclure. Il reste donc là où il a toujours été : sur la PREMIÈRE page,
    // en supplément, et il est COMPTÉ à part (`totals.tracks_via_metadata`)
    // plutôt que fondu dans `totals.tracks`, qui compte le prédicat que
    // `offset`/`limit` parcourent.
    let meta_repo = TrackMetadataRepo::with_backend(state.backend.clone());
    let meta_matches = if local_demande {
        meta_repo.search_by_value(&p.q, limit).unwrap_or_default()
    } else {
        Vec::new()
    };

    let fts_track_ids: std::collections::HashSet<i64> =
        tracks.iter().filter_map(|t| t.id).collect();

    let mut matched_metadata: HashMap<i64, HashMap<String, String>> = HashMap::new();
    for (track_id, key, value) in &meta_matches {
        matched_metadata
            .entry(*track_id)
            .or_default()
            .insert(key.clone(), value.clone());
    }

    let extra_ids: Vec<i64> = if offset > 0 {
        Vec::new()
    } else {
        meta_matches
            .iter()
            .map(|(id, _, _)| *id)
            .filter(|id| !fts_track_ids.contains(id))
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect()
    };

    let extra_tracks = if extra_ids.is_empty() {
        Vec::new()
    } else {
        track_repo.get_multiple(&extra_ids).unwrap_or_default()
    };

    // Build track JSON with matched_metadata annotations
    let mut track_results: Vec<Value> = Vec::with_capacity(tracks.len() + extra_tracks.len());
    for t in tracks.iter().chain(extra_tracks.iter()) {
        let mut v = t.to_json();
        if let Some(id) = t.id {
            if let Some(meta) = matched_metadata.get(&id) {
                v.as_object_mut()
                    .unwrap()
                    .insert("matched_metadata".into(), json!(meta));
            }
        }
        track_results.push(v);
    }

    // La moitié streaming ne change pas d'un octet : la liste blanche est la
    // même, lue plus haut, et la règle qu'elle applique ici est celle d'avant.
    let mut service_results: serde_json::Map<String, Value> = serde_json::Map::new();

    {
        let registry = state.services.lock().await;
        for svc_name in registry.list() {
            if let Some(ref sources) = requested_sources
                && !sources.contains(&svc_name)
                && !sources.contains(&JETON_TOUTES.to_string())
            {
                continue;
            }

            if let Some(svc) = registry.get(&svc_name) {
                let svc = svc.read().await;
                if !svc.auth_status().await.authenticated {
                    continue;
                }
                // `limit` tel quel, sans `offset` : le plafond de page d'un
                // service (Qobuz : 50) est SA contrainte, et #2036 dit qu'on la
                // pagine par `SearchPage`, pas en gonflant ce nombre.
                if let Ok(results) = svc.search(&p.q, limit as usize).await {
                    service_results.insert(svc_name, json!(results));
                }
            }
        }
    }

    Json(json!({
        "local": {
            "artists": artists,
            "albums": albums,
            "tracks": track_results,
            // #3189 — ce que la liste ne disait pas.
            "totals": {
                "artists": total_artists,
                "albums": total_albums,
                "tracks": total_tracks,
                // Pistes rendues EN SUPPLÉMENT parce que leurs métadonnées
                // correspondent ; hors `tracks` ci-dessus, et première page
                // seulement.
                "tracks_via_metadata": extra_tracks.len(),
            },
            // `true` : le total en regard est une borne INFÉRIEURE — « au
            // moins N » — et non un compte exact (voir PLAFOND_DE_COMPTAGE).
            "totals_capped": {
                "artists": total_artists >= PLAFOND_DE_COMPTAGE,
                "albums": total_albums >= PLAFOND_DE_COMPTAGE,
                "tracks": total_tracks >= PLAFOND_DE_COMPTAGE,
            },
            "has_more": {
                "artists": a_la_suite(artists.len(), total_artists),
                "albums": a_la_suite(albums.len(), total_albums),
                "tracks": a_la_suite(tracks.len(), total_tracks),
            },
            "limit": limit,
            "offset": offset,
        },
        "radios": radios,
        "services": service_results,
    }))
}
