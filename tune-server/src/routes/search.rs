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
//! # Les services sont interrogés ENSEMBLE, et le registre n'est plus tenu
//!
//! Bertrand, 19/09/2026 : « La recherche se fait en deux temps : local puis
//! streaming. Il ne faut pas faire patienter l'utilisateur ».
//!
//! La boucle des services portait son `await` À L'INTÉRIEUR, sous le verrou du
//! registre : quatre appels réseau à la file, du premier au dernier. Mesuré sur
//! le .18 (0.9.155), requête « coltrane », à chaud :
//!
//! ```text
//! qobuz     0,13 s      les quatre ensemble, à la file : 1,20 s
//! tidal     0,03 s      le plus lent seul              : 0,42 s
//! youtube   0,42 s
//! bandcamp  0,30 s
//! ```
//!
//! Le deuxième temps coûtait donc la SOMME au lieu du MAXIMUM, et l'écart
//! grandit avec chaque service ajouté.
//!
//! Deux corrections, et la seconde n'est pas cosmétique :
//!
//! 1. les poignées (`Arc<RwLock<…>>`, clonées par `registry.get`) sont
//!    ramassées d'abord, le verrou du registre tombe, PUIS les recherches
//!    partent sous `join_all` ;
//! 2. le `Mutex` du registre n'est donc plus tenu pendant des appels RÉSEAU.
//!    Il l'était : toute autre route qui demandait le registre — statut des
//!    services, favoris, catalogue — faisait la queue derrière une recherche
//!    Qobuz. Ce n'est pas la recherche qu'on accélère là, c'est le serveur
//!    qu'on arrête de bloquer.
//!
//! Ce qui ne change pas : le filtre `sources`, la limite par service, l'ordre
//! des clés (`service_results` est une `Map` JSON dont l'écran ordonne
//! lui-même les blocs, cf. `ordonnerSources` côté client), et le fait qu'un
//! service non authentifié ou en échec n'apparaît simplement pas.
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
use tune_core::streaming::traits::{SearchResults, StreamTrack, StreamingService};

use crate::routes::filtre_sources::FiltreSources;
use crate::state::AppState;

/// Lancer toutes les recherches de service EN MÊME TEMPS, et ranger ce qu'elles
/// rendent par nom de service.
///
/// 🔴 Extraite pour être MESURABLE. Le défaut qu'elle corrige est un défaut de
/// TEMPS : une boucle qui `await` en son sein rend exactement les mêmes octets
/// qu'un `join_all`, simplement plus tard. Aucune assertion sur le contenu ne
/// peut donc la voir — seul un banc qui chronomètre sait rougir.
///
/// Un service qui rend `None` (non authentifié, ou en échec) n'entre pas dans
/// la réponse : c'est la règle d'avant, et elle ne bouge pas.
async fn recherches_concurrentes<F>(travaux: Vec<(String, F)>) -> serde_json::Map<String, Value>
where
    F: std::future::Future<Output = Option<Value>>,
{
    let mut out = serde_json::Map::new();
    for (nom, trouve) in futures_util::future::join_all(
        travaux
            .into_iter()
            .map(|(nom, travail)| async move { (nom, travail.await) }),
    )
    .await
    {
        if let Some(v) = trouve {
            out.insert(nom, v);
        }
    }
    out
}

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

/// Lignes rendues quand l'appelant ne dit rien. Nommé parce qu'il sert deux
/// fois : ici, et en repli quand `?limit=` porte une valeur qu'un nombre
/// d'éléments ne peut pas prendre (#2160).
const LIMITE_PAR_DEFAUT: i64 = 20;

/// La limite telle qu'un service de streaming peut la recevoir (#2160).
///
/// `limit` est un `i64` que rien ne borne en bas : la moitié LOCALE lit un
/// nombre négatif comme « sans limite », mais un `as usize` en faisait ici
/// 18 446 744 073 709 551 615 — que `plafond_recherche` traduit chez Qobuz en
/// « Tous », soit dix allers-retours par service et par frappe, pour une
/// valeur que personne n'a demandée.
///
/// Retomber sur `0` serait pire encore : `0` EST le « Tous » de Qobuz. Le
/// repli est donc le défaut de la route — la seule valeur dont on sait qu'elle
/// a été voulue par quelqu'un.
fn limite_pour_les_services(limit: i64) -> usize {
    usize::try_from(limit).unwrap_or(LIMITE_PAR_DEFAUT as usize)
}

/// #4441 — la règle de #4367, appliquée aux pistes venues d'un SERVICE.
///
/// FabienM (fil 1839, point 4), après la v0.9.154 : « wish you were here »
/// rend toujours *Have a Cigar* en section Titres. #4367 avait retiré
/// `album_title` de ce que l'index LOCAL rapproche (`COLONNES_IDENTITE_PISTE`)
/// — et la capture le confirme, la bibliothèque passe de 10 à 6 lignes. Mais
/// les lignes restantes portent le badge QOBUZ : `/catalog/search` de Qobuz
/// rapproche lui aussi sur le titre d'album, et cette route recopiait sa
/// réponse telle quelle.
///
/// La même règle vaut donc ici, après réception : une piste de service reste
/// en section Titres si TOUS les jetons de la requête se retrouvent dans ce
/// qui l'identifie — titre, interprète, compositeur —, jamais dans son album.
/// L'album, lui, reste trouvé par la section Albums, qui est sa place.
///
/// Les jetons sont ceux de l'index local (`format_fts_query` : alphanumériques,
/// la ponctuation sépare), les guillemets de FabienM tombent donc d'eux-mêmes,
/// et les accents sont pliés des deux côtés — Qobuz trouve « Déjà Vu » pour
/// « deja vu », on ne le lui reprend pas. Le rapprochement est par
/// sous-chaîne, comme le préfixe FTS : « floy » retient encore Pink Floyd.
/// Une requête sans jeton ne filtre rien.
///
/// Ce qui est perdu, et assumé : la tolérance aux fautes de frappe de Qobuz.
/// Une piste rendue pour « wish you where here » sans qu'aucun mot ne
/// corresponde ne peut plus rester.
fn ne_garder_que_les_pistes_qui_repondent(requete: &str, resultats: &mut SearchResults) -> usize {
    let jetons = jetons_de_recherche(requete);
    if jetons.is_empty() {
        return 0;
    }
    let avant = resultats.tracks.len();
    resultats
        .tracks
        .retain(|piste| piste_de_service_repond(&jetons, piste));
    avant - resultats.tracks.len()
}

/// Minuscules, sans accents (NFD, marques combinantes retirées).
fn plier(texte: &str) -> String {
    use unicode_normalization::UnicodeNormalization;
    texte
        .nfd()
        .filter(|c| !unicode_normalization::char::is_combining_mark(*c))
        .collect::<String>()
        .to_lowercase()
}

/// Les jetons alphanumériques d'un texte plié, dans l'ordre.
fn jetons_plies(texte: &str) -> Vec<String> {
    plier(texte)
        .split(|c: char| !c.is_alphanumeric())
        .filter(|jeton| !jeton.is_empty())
        .map(str::to_string)
        .collect()
}

fn jetons_de_recherche(requete: &str) -> Vec<String> {
    jetons_plies(requete)
}

/// Tous les jetons de la requête se retrouvent dans ce qui identifie la
/// piste — jamais dans son album.
///
/// Deux formes sont regardées, comme `format_fts_query_libre` le fait pour
/// l'index : les jetons séparés d'un espace, et collés — « acdc » retient
/// « AC/DC ».
fn piste_de_service_repond(jetons: &[String], piste: &StreamTrack) -> bool {
    let identite = jetons_plies(&format!(
        "{} {} {}",
        piste.title,
        piste.artist,
        piste.composer.as_deref().unwrap_or_default()
    ));
    let separee = identite.join(" ");
    let collee = identite.concat();
    jetons
        .iter()
        .all(|jeton| separee.contains(jeton.as_str()) || collee.contains(jeton.as_str()))
}

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

// Les jetons `local` / `all` et la règle qui les lit vivaient ICI, sous la
// forme de deux constantes et d'un `le_local_est_demande` local à ce fichier.
// Ils sont partis dans `routes::filtre_sources`, SANS changer de sémantique —
// jeton pour jeton et bord pour bord.
//
// La raison : `/home/other-versions`, `/home/artist-releases` et
// `/library/tracks/{id}/versions` mélangeaient local et streaming exactement
// comme cette route, et devaient recevoir LE MÊME contrat. Le réécrire à côté
// aurait fait deux implémentations d'une même règle — le défaut qu'on passe
// la semaine à corriger ailleurs. Il n'en existe donc qu'une, et c'est elle
// que les quatre routes appellent.

pub fn router() -> Router<AppState> {
    Router::new().route("/", get(federated_search))
}

async fn federated_search(
    State(state): State<AppState>,
    Query(p): Query<SearchParams>,
) -> Json<Value> {
    let limit = p.limit.unwrap_or(LIMITE_PAR_DEFAUT);
    let offset = p.offset.unwrap_or(0).max(0);

    // #3226 — LU EN PREMIER. Tant que ce parsing vivait sous les recherches
    // locales, il ne pouvait par construction gouverner qu'elles seules.
    let filtre = FiltreSources::depuis(p.sources.as_deref());
    let local_demande = filtre.local_demande();

    let artist_repo = ArtistRepo::with_backend(state.backend.clone());
    let album_repo = AlbumRepo::with_backend(state.backend.clone());
    let track_repo = TrackRepo::with_backend(state.backend.clone());

    let (artists, albums, tracks, radios) = if local_demande {
        (
            artist_repo
                .search_page(&p.q, limit, offset)
                .unwrap_or_default(),
            avec_date_d_ajout(
                &album_repo,
                album_repo
                    .search_page(&p.q, limit, offset)
                    .unwrap_or_default(),
            ),
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
    let service_results: serde_json::Map<String, Value>;

    // Les poignées d'abord, le verrou ensuite — puis les quatre recherches
    // EN MÊME TEMPS. Voir la note « Les services sont interrogés ensemble »
    // en tête de fichier.
    let poignees: Vec<(
        String,
        std::sync::Arc<tokio::sync::RwLock<Box<dyn StreamingService>>>,
    )> = {
        let registry = state.services.lock().await;
        registry
            .list()
            .into_iter()
            .filter(|nom| filtre.service_demande(nom))
            .filter_map(|nom| registry.get(&nom).map(|svc| (nom, svc)))
            .collect()
    };

    let limite = limite_pour_les_services(limit);
    let requete = p.q.clone();
    let travaux: Vec<_> = poignees
        .into_iter()
        .map(|(nom, svc)| {
            let requete = requete.clone();
            let nom_log = nom.clone();
            (nom, async move {
                let svc = svc.read().await;
                if !svc.auth_status().await.authenticated {
                    return None;
                }
                // `limit` tel quel, sans `offset` : le plafond de page d'un
                // service (Qobuz : 50) est SA contrainte, et #2036 dit qu'on la
                // pagine par `SearchPage`, pas en gonflant ce nombre.
                //
                // « Tel quel » s'arrête au SIGNE : voir
                // [`limite_pour_les_services`] (#2160).
                let mut results = svc.search(&requete, limite).await.ok()?;
                // #4441 — voir `ne_garder_que_les_pistes_qui_repondent` : le filtre
                // s'applique à chaque service, dans sa tâche, avant la réunion.
                let ecartees = ne_garder_que_les_pistes_qui_repondent(&requete, &mut results);
                if ecartees > 0 {
                    tracing::debug!(
                        service = %nom_log,
                        ecartees,
                        gardees = results.tracks.len(),
                        "search_pistes_de_service_hors_identite_ecartees"
                    );
                }
                Some(json!(results))
            })
        })
        .collect();
    service_results = recherches_concurrentes(travaux).await;

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

/// Attache `added_at` aux albums d'une page de résultats.
///
/// `search_page` lit les albums par `select_album()`, qui laisse `added_at` à
/// `None`. L'écran de recherche trie désormais ses albums par date d'ajout
/// (Bertrand, 16/09/2026) : sans cette passe, ce tri serait un tri sur rien.
fn avec_date_d_ajout(
    repo: &AlbumRepo,
    mut albums: Vec<tune_core::db::models::Album>,
) -> Vec<tune_core::db::models::Album> {
    repo.attacher_added_at(&mut albums);
    albums
}

#[cfg(test)]
mod tests_date_d_ajout {
    use super::*;

    /// Une page de recherche porte la date d'ajout de ses albums locaux —
    /// lue de la même source que la Bibliothèque (`file_first_seen`, sinon
    /// mtime), jamais inventée : un album sans piste locale reste sans date.
    #[test]
    fn la_page_de_recherche_porte_la_date_d_ajout() {
        let state = crate::state::AppState::new(":memory:", 0, Default::default()).unwrap();
        let b = &state.backend;
        b.execute(
            "INSERT INTO artists (id, name) VALUES (1, 'Nick Drake')",
            &[],
        )
        .unwrap();
        b.execute(
            "INSERT INTO albums (id, title, artist_id) VALUES (1, 'Pink Moon', 1), (2, 'Bryter Layter', 1)",
            &[],
        )
        .unwrap();
        b.execute(
            "INSERT INTO tracks (title, album_id, artist_id, file_path, file_mtime, source) \
             VALUES ('Pink Moon', 1, 1, '/m/pink.flac', 1600000000, 'local')",
            &[],
        )
        .unwrap();
        let repo = AlbumRepo::with_backend(state.backend.clone());
        let page = repo.search_page("Nick", 10, 0).unwrap();
        assert_eq!(page.len(), 2, "{page:?}");
        assert!(
            page.iter().all(|a| a.added_at.is_none()),
            "select_album() ne la porte pas"
        );

        let page = avec_date_d_ajout(&repo, page);
        let pink = page.iter().find(|a| a.id == Some(1)).unwrap();
        let bryter = page.iter().find(|a| a.id == Some(2)).unwrap();
        assert!(pink.added_at.is_some_and(|t| t > 0.0), "{pink:?}");
        assert_eq!(bryter.added_at, None, "aucune piste locale : aucune date");
    }
}

#[cfg(test)]
mod tests_limite_services {
    use super::*;

    /// #2160 — la limite envoyée aux services de streaming.
    #[test]
    fn une_limite_negative_ne_devient_pas_tous() {
        // Le défaut : `-1 as usize` = `usize::MAX`, que Qobuz borne à 500 —
        // c'est-à-dire dix allers-retours pour une recherche fédérée.
        assert_eq!(limite_pour_les_services(-1), LIMITE_PAR_DEFAUT as usize);
        assert_eq!(
            limite_pour_les_services(i64::MIN),
            LIMITE_PAR_DEFAUT as usize
        );
        assert_ne!(
            limite_pour_les_services(-1),
            0,
            "0 est le « Tous » de Qobuz : y retomber aggraverait le défaut"
        );
    }

    /// Contre-épreuve : tout ce qui est un nombre d'éléments traverse intact,
    /// y compris le `0` explicite, qui reste le « Tous » documenté.
    #[test]
    fn une_limite_valide_traverse_intacte() {
        for demandee in [0i64, 1, 20, 50, 200, 5_000] {
            assert_eq!(limite_pour_les_services(demandee), demandee as usize);
        }
    }
}

/// #4441 — la section Titres et les pistes venues d'un SERVICE.
#[cfg(test)]
mod tests_pistes_de_service_i4441 {
    use super::{SearchParams, federated_search};
    use axum::extract::{Query, State};
    use tune_core::TuneError;
    use tune_core::streaming::traits::{
        AuthStatus, SearchResults, StreamAlbum, StreamArtist, StreamPlaylist, StreamTrack,
        StreamUrl, StreamingService,
    };

    fn piste(id: &str, titre: &str, artiste: &str, album: &str) -> StreamTrack {
        StreamTrack {
            id: id.to_string(),
            title: titre.to_string(),
            artist: artiste.to_string(),
            album: Some(album.to_string()),
            album_id: None,
            duration_ms: 300_000,
            cover_path: None,
            track_number: None,
            disc_number: None,
            explicit: false,
            disponible: None,
            quality: None,
            isrc: None,
            composer: None,
            artist_id: None,
        }
    }

    /// Un « Qobuz » qui répond comme le vrai à « wish you were here » (fil
    /// 1839, capture `cJ0FqgEA…`) : les trois pistes de l'album, dont deux
    /// dont le titre ne porte aucun mot de la requête — Qobuz rapproche sur
    /// le titre d'album. Plus une piste accentuée, pour la pliure.
    struct QobuzDeFabien;

    #[async_trait::async_trait]
    impl StreamingService for QobuzDeFabien {
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
        fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
            self
        }
        fn name(&self) -> &str {
            "qobuz"
        }
        fn enabled(&self) -> bool {
            true
        }
        fn set_enabled(&mut self, _enabled: bool) {}
        async fn authenticate(&mut self, _c: &serde_json::Value) -> Result<AuthStatus, TuneError> {
            Ok(self.auth_status().await)
        }
        async fn auth_status(&self) -> AuthStatus {
            AuthStatus {
                authenticated: true,
                ..Default::default()
            }
        }
        async fn logout(&mut self) -> Result<(), TuneError> {
            Ok(())
        }
        async fn search(&self, _q: &str, _l: usize) -> Result<SearchResults, TuneError> {
            Ok(SearchResults {
                tracks: vec![
                    piste(
                        "q-machine",
                        "Welcome to the Machine",
                        "Pink Floyd",
                        "Wish You Were Here",
                    ),
                    piste(
                        "q-cigar",
                        "Have a Cigar",
                        "Pink Floyd",
                        "Wish You Were Here",
                    ),
                    piste(
                        "q-wywh",
                        "Wish You Were Here",
                        "Pink Floyd",
                        "Wish You Were Here",
                    ),
                    piste("q-deja", "Déjà Vu", "Beyoncé", "B'Day"),
                ],
                albums: vec![],
                artists: vec![],
                playlists: vec![],
            })
        }
        async fn get_track(&self, _t: &str) -> Result<StreamTrack, TuneError> {
            Err("hors sujet".into())
        }
        async fn get_track_url(&self, _t: &str, _q: Option<&str>) -> Result<StreamUrl, TuneError> {
            Err("hors sujet".into())
        }
        async fn get_album(&self, _a: &str) -> Result<StreamAlbum, TuneError> {
            Err("hors sujet".into())
        }
        async fn get_album_tracks(&self, _a: &str) -> Result<Vec<StreamTrack>, TuneError> {
            Err("hors sujet".into())
        }
        async fn get_artist(&self, _a: &str) -> Result<StreamArtist, TuneError> {
            Err("hors sujet".into())
        }
        async fn get_playlist(&self, _p: &str) -> Result<StreamPlaylist, TuneError> {
            Err("hors sujet".into())
        }
        async fn get_playlist_tracks(&self, _p: &str) -> Result<Vec<StreamTrack>, TuneError> {
            Err("hors sujet".into())
        }
        async fn get_user_playlists(&self) -> Result<Vec<StreamPlaylist>, TuneError> {
            Ok(vec![])
        }
        async fn get_user_albums(&self) -> Result<Vec<StreamAlbum>, TuneError> {
            Ok(vec![])
        }
        async fn get_user_artists(&self) -> Result<Vec<StreamArtist>, TuneError> {
            Ok(vec![])
        }
    }

    /// Les identifiants des pistes Qobuz que `GET /search?q=…&sources=qobuz`
    /// rend, dans l'ordre.
    async fn pistes_qobuz_pour(q: &str) -> Vec<String> {
        let state = crate::state::AppState::new(":memory:", 0, Default::default()).unwrap();
        state
            .services
            .lock()
            .await
            .register(Box::new(QobuzDeFabien));
        let reponse = federated_search(
            State(state),
            Query(SearchParams {
                q: q.to_string(),
                limit: None,
                offset: None,
                sources: Some("qobuz".into()),
            }),
        )
        .await;
        reponse.0["services"]["qobuz"]["tracks"]
            .as_array()
            .expect("un tableau de pistes Qobuz")
            .iter()
            .map(|p| p["source_id"].as_str().unwrap_or_default().to_string())
            .collect()
    }

    /// ⭐ Le fil 1839, point 4 : « Tune retourne toujours "Have a cigar" dans
    /// les titres ». La règle de #4367 — une piste est trouvée par ce qui
    /// l'identifie, pas par son album — vaut pour les pistes de service
    /// comme pour l'index local. Guillemets compris : c'est ainsi que FabienM
    /// l'a saisie.
    #[tokio::test]
    async fn une_piste_de_service_n_est_pas_retenue_par_son_seul_titre_d_album() {
        assert_eq!(
            pistes_qobuz_pour("\"wish you were here\"").await,
            vec!["q-wywh".to_string()],
            "seule la piste dont le TITRE porte la requête doit rester (#4441)"
        );
        assert_eq!(
            pistes_qobuz_pour("wish you were here").await,
            vec!["q-wywh"]
        );
    }

    /// Contre-épreuve : l'artiste identifie la piste — « pink floyd » garde
    /// les trois ; et la pliure des accents rend « beyonce deja vu » capable
    /// de trouver « Déjà Vu » de Beyoncé, comme Qobuz sait le faire.
    #[tokio::test]
    async fn l_artiste_et_les_accents_plies_identifient_toujours_la_piste() {
        assert_eq!(
            pistes_qobuz_pour("pink floyd").await,
            vec!["q-machine", "q-cigar", "q-wywh"]
        );
        assert_eq!(pistes_qobuz_pour("beyonce deja vu").await, vec!["q-deja"]);
        assert_eq!(pistes_qobuz_pour("Déjà").await, vec!["q-deja"]);
    }

    /// Les pièces du filtre : guillemets et ponctuation tombent, la forme
    /// collée retient « AC/DC », une requête vide ne filtre rien.
    #[test]
    fn les_jetons_et_la_forme_collee() {
        use super::{jetons_de_recherche, ne_garder_que_les_pistes_qui_repondent};
        assert_eq!(
            jetons_de_recherche("\"Wish You Were Here\""),
            vec!["wish", "you", "were", "here"]
        );
        assert_eq!(jetons_de_recherche("Beyoncé"), vec!["beyonce"]);
        assert!(jetons_de_recherche("\"\" - ").is_empty());

        let mut r = SearchResults {
            tracks: vec![
                piste("acdc", "Back in Black", "AC/DC", "Back in Black"),
                piste("cigar", "Have a Cigar", "Pink Floyd", "Wish You Were Here"),
            ],
            albums: vec![],
            artists: vec![],
            playlists: vec![],
        };
        assert_eq!(ne_garder_que_les_pistes_qui_repondent("acdc", &mut r), 1);
        assert_eq!(r.tracks.len(), 1);
        assert_eq!(r.tracks[0].id, "acdc");
        assert_eq!(ne_garder_que_les_pistes_qui_repondent("  ", &mut r), 0);

/// Les services sont interrogés ENSEMBLE — Bertrand, 19/09/2026.
///
/// « La recherche se fait en deux temps : local puis streaming. Il ne faut pas
/// faire patienter l'utilisateur ».
///
/// 🔴 Ce banc CHRONOMÈTRE, et c'est le seul moyen de garder la propriété : une
/// boucle qui `await` en son sein rend exactement les mêmes octets qu'un
/// `join_all`, simplement plus tard. Toute assertion sur le contenu resterait
/// verte sous le défaut.
#[cfg(test)]
mod tests_services_en_parallele {
    use super::*;
    use std::time::{Duration, Instant};

    /// Quatre services qui mettent chacun `DELAI` à répondre.
    ///
    /// À la file : 4 × DELAI. Ensemble : ≈ DELAI. Le seuil est posé à la
    /// MOITIÉ de la somme — assez bas pour qu'un enchaînement séquentiel le
    /// franchisse à coup sûr, assez haut pour ne pas rougir sur une machine
    /// chargée (Shrek compile souvent à 60 tâches).
    const DELAI: Duration = Duration::from_millis(150);
    const NOMBRE: usize = 4;

    fn travaux() -> Vec<(String, impl std::future::Future<Output = Option<Value>>)> {
        (0..NOMBRE)
            .map(|i| {
                let nom = format!("service{i}");
                (nom.clone(), async move {
                    tokio::time::sleep(DELAI).await;
                    Some(json!({ "nom": nom }))
                })
            })
            .collect()
    }

    #[tokio::test]
    async fn les_quatre_partent_en_meme_temps() {
        let debut = Instant::now();
        let out = recherches_concurrentes(travaux()).await;
        let ecoule = debut.elapsed();

        assert_eq!(out.len(), NOMBRE, "les quatre réponses doivent être là");
        let a_la_file = DELAI * NOMBRE as u32;
        assert!(
            ecoule < a_la_file / 2,
            "les services sont interrogés À LA FILE : {ecoule:?} pour {NOMBRE} \
             services à {DELAI:?} (à la file : {a_la_file:?})"
        );
    }

    #[tokio::test]
    async fn chaque_reponse_est_rangee_sous_son_propre_service() {
        // Concurrent ne veut pas dire mélangé : `join_all` préserve
        // l'appariement, et c'est ce qui est vérifié ici.
        let out = recherches_concurrentes(travaux()).await;
        for i in 0..NOMBRE {
            let nom = format!("service{i}");
            assert_eq!(
                out.get(&nom)
                    .and_then(|v| v.get("nom"))
                    .and_then(|v| v.as_str()),
                Some(nom.as_str()),
                "{nom} mal apparié"
            );
        }
    }

    #[tokio::test]
    async fn un_service_muet_n_entre_pas_dans_la_reponse() {
        // Non authentifié, ou en échec : la règle d'avant, inchangée.
        // 🔴 Les deux travaux sortent de LA MÊME fermeture : deux blocs
        // `async` écrits séparément, même identiques au caractère près, n'ont
        // pas le même type et ne tiennent pas dans un `Vec`.
        let travaux: Vec<(String, _)> = [("qui_repond", true), ("qui_se_tait", false)]
            .into_iter()
            .map(|(nom, repond)| {
                (nom.to_string(), async move {
                    repond.then(|| json!({ "ok": true }))
                })
            })
            .collect();
        let out = recherches_concurrentes(travaux).await;
        assert_eq!(out.len(), 1);
        assert!(out.contains_key("qui_repond"));
        assert!(!out.contains_key("qui_se_tait"));
    }

    #[tokio::test]
    async fn sans_service_demande_la_reponse_est_vide_pas_absente() {
        let travaux: Vec<(String, std::future::Ready<Option<Value>>)> = vec![];
        assert!(recherches_concurrentes(travaux).await.is_empty());
    }
}
