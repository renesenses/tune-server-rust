use axum::Json;
use axum::extract::{Multipart, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::state::AppState;
use tune_core::cloud::ecoutes_communaute::{self, Genre};
use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::artist_repo::ArtistRepo;
use tune_core::db::track_repo::{TrackRepo, dedup_display_tracks};

use super::{api_cache_get, api_cache_set, artwork_cache_dir, now_iso_utc};

#[derive(Deserialize)]
pub(super) struct LangQuery {
    lang: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct ImageReportBody {
    reason: Option<String>,
}

// ── BIB-C1, phase 0 — artistes homographes, en LECTURE SEULE ────────────────

/// Ce que le rapprochement lit d'une fiche artiste.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ArtisteVue {
    pub(crate) id: i64,
    pub(crate) name: String,
    pub(crate) musicbrainz_id: Option<String>,
    pub(crate) albums: i64,
}

/// BIB-C1, phase 0 : les groupes de fiches artistes qui portent la MÊME clé
/// de rapprochement (`cle_artiste`, BIB-C2 : accents, casse, « The »,
/// ponctuation, « & ») — les treize paires mesurées sur .18 le 30/08. Depuis
/// C2 les nouvelles écritures retombent sur la fiche existante ; ce rapport
/// nomme celles qui se sont créées AVANT, pour la table d'alias (C1) et
/// l'écran de fusion (C3). Rien n'est fusionné : les homonymes existent, et
/// deux fiches aux MBID différents sont peut-être deux artistes — le rapport
/// le dit (`mbid_distincts`). Le plus ancien d'abord dans chaque groupe.
pub(crate) fn grouper_les_artistes_homographes(artistes: &[ArtisteVue]) -> Vec<Value> {
    use std::collections::{BTreeMap, BTreeSet};
    let mut par_cle: BTreeMap<String, Vec<&ArtisteVue>> = BTreeMap::new();
    for a in artistes {
        let cle = tune_core::db::artist_repo::cle_artiste(&a.name);
        if cle.is_empty() {
            continue;
        }
        par_cle.entry(cle).or_default().push(a);
    }
    let mut groupes: Vec<Value> = par_cle
        .into_iter()
        .filter(|(_, v)| v.len() >= 2)
        .map(|(cle, mut v)| {
            v.sort_by_key(|a| a.id);
            let mbids: BTreeSet<&str> = v
                .iter()
                .filter_map(|a| a.musicbrainz_id.as_deref())
                .filter(|m| !m.trim().is_empty())
                .collect();
            json!({
                "cle": cle,
                "mbid_distincts": mbids.len() > 1,
                "albums": v.iter().map(|a| a.albums).sum::<i64>(),
                "artistes": v.iter().map(|a| json!({
                    "id": a.id,
                    "name": a.name,
                    "musicbrainz_id": a.musicbrainz_id,
                    "albums": a.albums,
                })).collect::<Vec<_>>(),
            })
        })
        .collect();
    // Les plus sûrs d'abord (MBID non contradictoires), puis les plus fournis.
    groupes.sort_by(|x, y| {
        let sx = (
            !x["mbid_distincts"].as_bool().unwrap_or(false),
            x["albums"].as_i64().unwrap_or(0),
        );
        let sy = (
            !y["mbid_distincts"].as_bool().unwrap_or(false),
            y["albums"].as_i64().unwrap_or(0),
        );
        sy.cmp(&sx)
    });
    groupes
}

/// `GET /library/artists/doublons` — les artistes homographes (BIB-C1, phase 0).
/// `POST /library/artists/{cible}/absorber/{doublon}` — BIB-C1, phase 1.
///
/// La phase 0 (`GET /library/artists/doublons`) nomme les groupes de même
/// clé (`cle_artiste`). Ici l'utilisateur tranche, un couple à la fois, et le
/// serveur vérifie : même clé, pas deux MBID différents (le drapeau
/// `mbid_distincts` de la phase 0), aucun des deux n'est l'artiste inconnu.
/// Le survivant est celui que l'URL nomme `cible` — l'écran propose le plus
/// ancien, comme la liste de la phase 0. Rien n'est automatique.
///
/// Réponses : `200` avec le bilan de [`ArtistRepo::absorber`] ; `404`
/// artiste inconnu ; `400` même artiste des deux côtés ; `409
/// cles_differentes`, `409 mbid_distincts`, `409 artiste_inconnu_non_absorbable`.
pub(super) async fn absorber_artiste(
    State(state): State<AppState>,
    Path((cible, doublon)): Path<(i64, i64)>,
) -> Response {
    use tune_core::db::artist_repo::{UNKNOWN_ARTIST_NAME, cle_artiste};
    if cible == doublon {
        return super::refus(
            StatusCode::BAD_REQUEST,
            "meme_artiste",
            "un artiste ne s'absorbe pas lui-même".to_string(),
        );
    }
    let repo = ArtistRepo::with_backend(state.backend.clone());
    let (Ok(Some(artiste_cible)), Ok(Some(artiste_doublon))) = (repo.get(cible), repo.get(doublon))
    else {
        return super::refus(
            StatusCode::NOT_FOUND,
            "artiste_inconnu",
            format!("artistes {cible} / {doublon}"),
        );
    };
    if artiste_cible.name == UNKNOWN_ARTIST_NAME || artiste_doublon.name == UNKNOWN_ARTIST_NAME {
        return super::refus(
            StatusCode::CONFLICT,
            "artiste_inconnu_non_absorbable",
            "« Unknown Artist » n'est pas une fiche : rien ne s'y fond, il ne se fond dans rien"
                .to_string(),
        );
    }
    if cle_artiste(&artiste_cible.name) != cle_artiste(&artiste_doublon.name) {
        return super::refus(
            StatusCode::CONFLICT,
            "cles_differentes",
            format!(
                "« {} » et « {} » ne sont pas deux graphies du même nom",
                artiste_cible.name, artiste_doublon.name
            ),
        );
    }
    let mbid = |m: &Option<String>| {
        m.as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    if let Some((a, b)) = mbid(&artiste_cible.musicbrainz_id)
        .zip(mbid(&artiste_doublon.musicbrainz_id))
        .filter(|(a, b)| a != b)
    {
        return super::refus(
            StatusCode::CONFLICT,
            "mbid_distincts",
            format!(
                "deux identifiants MusicBrainz différents ({a} / {b}) : deux artistes, pas deux graphies"
            ),
        );
    }
    match repo.absorber(cible, doublon) {
        Ok(rapport) => {
            state.event_bus.emit(
                tune_core::event_types::EventType::LibraryUpdated.as_str(),
                json!({ "source": "artists_absorber", "cible": cible, "doublon": doublon }),
            );
            (StatusCode::OK, Json(json!(rapport))).into_response()
        }
        Err(e) => super::refus(
            StatusCode::INTERNAL_SERVER_ERROR,
            "absorption_echouee",
            e.to_string(),
        ),
    }
}

pub(super) async fn artists_doublons(State(state): State<AppState>) -> Json<Value> {
    use tune_http_types::panne_sql::OuDefautJournalise;
    let sql = "SELECT a.id, a.name, a.musicbrainz_id, \
               (SELECT COUNT(*) FROM albums al WHERE al.artist_id = a.id) AS albums \
               FROM artists a ORDER BY a.id";
    let artistes: Vec<ArtisteVue> = state
        .backend
        .query_many(sql, &[])
        .ou_defaut_journalise()
        .into_iter()
        .filter_map(|r| {
            Some(ArtisteVue {
                id: r.first().and_then(|v| v.as_i64())?,
                name: r.get(1).and_then(|v| v.as_string())?,
                musicbrainz_id: r
                    .get(2)
                    .and_then(|v| v.as_string())
                    .filter(|m| !m.trim().is_empty()),
                albums: r.get(3).and_then(|v| v.as_i64()).unwrap_or(0),
            })
        })
        .collect();
    let groupes = grouper_les_artistes_homographes(&artistes);
    Json(json!({
        "count": groupes.len(),
        "artistes_concernes": groupes.iter().map(|g| g["artistes"].as_array().map(|a| a.len()).unwrap_or(0)).sum::<usize>(),
        "groups": groupes,
    }))
}

/// `GET /library/artists` : la pagination, et `provenances=1` (tune-web-client#1716).
#[derive(Deserialize)]
pub(super) struct ArtistsQuery {
    limit: Option<i64>,
    offset: Option<i64>,
    /// `1` / `true` : chaque artiste porte `provenances`, ses albums et pistes
    /// visibles par clé de provenance. Absent : la réponse d'avant, au
    /// caractère près, et aucune requête de plus.
    provenances: Option<String>,
}

fn veut_les_sources(v: Option<&str>) -> bool {
    matches!(v.map(str::trim), Some("1" | "true"))
}

/// Pose `provenances` sur chaque artiste de la page — tune-web-client#1716.
///
/// L'onglet Artistes chargeait toute la bibliothèque (pistes ET albums) pour
/// calculer ces sources dans le navigateur. Elles viennent maintenant d'ici,
/// pour la page seulement. Un artiste sans album ni piste visible reçoit `{}` :
/// le client le lit comme « aucune source », comme avant.
fn joindre_les_sources(
    items: &mut [Value],
    sources: &tune_core::db::artist_repo::SourcesDesArtistes,
) {
    for item in items {
        let Some(obj) = item.as_object_mut() else {
            continue;
        };
        let id = obj.get("id").and_then(Value::as_i64);
        let s = id.and_then(|id| sources.get(&id));
        obj.insert("provenances".into(), json!(s.cloned().unwrap_or_default()));
    }
}

pub(super) async fn list_artists(
    State(state): State<AppState>,
    Query(p): Query<ArtistsQuery>,
) -> Json<Value> {
    let limit = p.limit.unwrap_or(50);
    let offset = p.offset.unwrap_or(0);
    // Les lectures sont synchrones (rusqlite) : hors de l'exécuteur, comme
    // `GET /library/tracks` (#5138). Les sources parcourent les pistes.
    let avec_sources = veut_les_sources(p.provenances.as_deref());
    match tokio::task::spawn_blocking(move || {
        lire_la_page_d_artistes(&state, limit, offset, avec_sources)
    })
    .await
    {
        Ok(corps) => Json(corps),
        Err(e) => {
            tracing::error!(error = %e, "list_artists_tache_bloquante_perdue");
            Json(json!({"items": [], "total": 0, "limit": limit, "offset": offset}))
        }
    }
}

fn lire_la_page_d_artistes(state: &AppState, limit: i64, offset: i64, avec_sources: bool) -> Value {
    let repo = ArtistRepo::with_backend(state.backend.clone());
    let total = repo.count().unwrap_or(0);
    let items = match repo.list(limit, offset) {
        Ok(artists) => artists,
        Err(e) => {
            tracing::error!(
                error = %e,
                limit,
                offset,
                total,
                "list_artists_query_failed — stats show {total} artists but query returned error"
            );
            Vec::new()
        }
    };
    if !avec_sources {
        return json!({"items": items, "total": total, "limit": limit, "offset": offset});
    }
    let ids: Vec<i64> = items.iter().filter_map(|a| a.id).collect();
    // Un échec ne pose PAS `provenances` : un `{}` dirait « aucune source » à
    // tort, alors que son absence fait retomber le client sur son calcul.
    let sources = match repo.sources_des_artistes(&ids) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, "list_artists_sources_query_failed");
            return json!({"items": items, "total": total, "limit": limit, "offset": offset});
        }
    };
    let mut items: Vec<Value> = items.iter().map(|a| json!(a)).collect();
    joindre_les_sources(&mut items, &sources);
    json!({"items": items, "total": total, "limit": limit, "offset": offset})
}

pub(super) async fn get_artist(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let repo = ArtistRepo::with_backend(state.backend.clone());
    match repo.get(id) {
        Ok(Some(artist)) => {
            let mut j = json!(artist);
            if let (Some(obj), Ok(Some(prov))) = (j.as_object_mut(), repo.bio_provenance(id)) {
                obj.insert("bio_provenance".into(), prov);
            }
            // #6463 — d'où vient le MBID (`balise`, `nom`, `pressage`,
            // `manuel`, ou null : inconnu). La fiche s'en sert pour proposer
            // « Détacher l'identité » et dire qu'elle a été détachée.
            if let Some(obj) = j.as_object_mut() {
                let provenance = repo.provenance_du_mbid(id).ok().flatten();
                obj.insert(
                    "musicbrainz_id_source".into(),
                    json!(provenance.map(|p| p.as_str())),
                );
            }
            // #6738 — le total des écoutes de la communauté, absent sous le seuil.
            ecoutes_communaute::annoter_un(
                &state.backend,
                &state.http_client,
                Genre::Artiste,
                &mut j,
            )
            .await;
            Json(j).into_response()
        }
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

pub(super) async fn artist_bio(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Query(q): Query<LangQuery>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let repo = ArtistRepo::with_backend(state.backend.clone());
    let artist = repo.get(id).ok().flatten();
    let Some(artist) = artist else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let lang = langue_demandee(q.lang.as_deref(), &headers);
    let lang = lang.as_str();

    // Prefer a locally-enriched bio (with provenance/attribution) over the
    // community proxy — this surfaces the sourced Wikipedia/Last.fm/Qobuz/
    // TheAudioDB bio and its licence to the client.
    //
    // MAIS seulement si elle est dans la bonne langue (#1849, Dimitri).
    //
    // Cette branche rendait la bio stockée sans jamais regarder ni sa langue ni
    // celle qu'on demande, et le `lang` ci-dessus n'était donc JAMAIS atteint
    // pour un artiste qui en possédait une. Or l'enrichissement récupère les
    // bios dans la langue de CELUI QUI LE LANCE (`enrich.rs:139`,
    // `lang_from_header`) : une bibliothèque enrichie depuis une interface en
    // français stocke tout en français, et le sert ensuite à tout le monde.
    // C'est exactement ce que voit Dimitri.
    let prov = repo.bio_provenance(id).ok().flatten();
    let bio_lang = prov
        .as_ref()
        .and_then(|p| p.get("lang").and_then(|v| v.as_str()));
    let stored_ok = langue_convient(bio_lang, lang);

    if let Some(ref bio) = artist.bio
        && !bio.is_empty()
        && stored_ok
    {
        return Json(json!({
            "artist": artist.name,
            "bio": bio,
            "bio_provenance": prov,
        }))
        .into_response();
    }
    // Community artist-bio API is keyed by NAME (AI-generated on demand) — NO
    // MusicBrainz id required, so it works for the whole library, most of which
    // has no MBID. Mirrors the album path.
    let cache_key = format!("cache:artistbio:{}:{lang}", artist.name);
    if let Some(cached) = api_cache_get(&state.backend, &cache_key) {
        return Json(cached).into_response();
    }
    match state
        .http_client
        .get("https://mozaiklabs.fr/api/v1/artists/bio")
        // `lang` est transmis au site, qui ne le recevait pas : il ne servait
        // qu'à nommer l'entrée de cache. ⚠️ L'effet dépend du site
        // (`site-mozaiklabs`) : s'il ignore le paramètre, il rendra la même
        // langue qu'avant — sans régression, mais sans gain non plus. À
        // vérifier là-bas avant d'annoncer #1849 comme réglé.
        .query(&[("name", artist.name.as_str()), ("lang", lang)])
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            let data: Value = resp.json().await.unwrap_or(json!({}));
            let out = reponse_du_proxy("artist", &artist.name, &data);
            if out.get("bio").map(|b| !b.is_null()).unwrap_or(false) {
                api_cache_set(&state.backend, &cache_key, &out);
            }
            Json(out).into_response()
        }
        _ => repli_sur_la_bio_stockee(&artist.name, artist.bio.as_deref(), prov),
    }
}

/// Le corps rendu par les deux routes `/bio` quand la bio vient du proxy
/// communautaire (`/api/v1/artists/bio`, `/api/v1/albums/bio` du site).
///
/// `cle` vaut `"artist"` ou `"album"`, `nom` le nom ou le titre.
///
/// `source` reste à plat, comme avant. S'y ajoute `bio_provenance`, sous la
/// MÊME forme que celle des bios stockées par `bio_sync`
/// (`{source, source_url, license, lang, fetched_at}`, cf.
/// `ArtistRepo::bio_provenance`) : sans elle, un extrait Wikipédia servi par
/// ce chemin s'affichait sans le lien vers son article, alors que sa licence
/// (CC BY-SA 4.0) exige l'attribution.
///
/// - `source` : la règle de `bio_sync::bio_source`, `"community"` à défaut ;
/// - `source_url`, `license`, `lang` : relayés tels que le site les donne,
///   comme `bio_sync` les range en base. La licence n'est pas réécrite : le
///   site écrit `CC BY-SA 4.0`, l'enrichissement local `CC-BY-SA-4.0`, et le
///   client lit les deux graphies ;
/// - `fetched_at` : l'heure de la réponse du site.
///
/// Rien n'est écrit en base : seule l'entrée de cache (24 h) garde ce corps,
/// provenance comprise. Pas de bio, pas de provenance.
pub(super) fn reponse_du_proxy(cle: &str, nom: &str, data: &Value) -> Value {
    let bio = data.get("bio").cloned().unwrap_or(Value::Null);
    let mut out = json!({
        cle: nom,
        "bio": bio,
        "source": data.get("source").cloned().unwrap_or(Value::Null),
    });
    if bio.as_str().is_some_and(|b| !b.trim().is_empty()) {
        let texte = |champ: &str| {
            data.get(champ)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        };
        out["bio_provenance"] = json!({
            "source": tune_core::cloud::bio_sync::bio_source(&texte("source")),
            "source_url": texte("source_url"),
            "license": texte("license"),
            "lang": texte("lang"),
            "fetched_at": now_iso_utc(),
        });
    }
    out
}

/// Quelle langue la requête demande-t-elle, pour les deux routes « bio » ?
///
/// Précédence, dans cet ordre :
/// 1. le paramètre `?lang=` explicite — un appel qui nomme sa langue gagne ;
/// 2. l'en-tête `Accept-Language`, via [`crate::i18n::lang_from_header`], la
///    MÊME lecture que `browse.rs` et `system/enrich.rs` ;
/// 3. `fr`, repli déjà porté par `lang_from_header`.
///
/// Sans l'étape 2, les deux routes repliaient droit sur `"fr"` (#1849) : le
/// client web n'envoie `?lang=` sur aucune des deux, il transmet sa locale par
/// l'en-tête sur CHAQUE requête (`tune-web-client/src/lib/api.ts`). Un
/// utilisateur en interface anglaise obtenait donc `lang = "fr"`,
/// `langue_convient(Some("fr"), "fr")` était vrai, et la bio française lui
/// était resservie — le garde-fou de langue livré au-dessus n'ayant jamais
/// l'occasion de jouer.
///
/// Un `?lang=` vide (`?lang=`) est traité comme absent : il ne nomme aucune
/// langue, et le laisser passer donnerait `lang = ""`, qui ne convient à
/// aucune bio estampillée et déclencherait un appel réseau pour rien.
///
/// La résolution elle-même vit dans [`crate::i18n::lang_from_request`] depuis
/// #3089 : les notes de version la partagent, une seule implémentation.
pub(super) fn langue_demandee(param: Option<&str>, headers: &HeaderMap) -> String {
    crate::i18n::lang_from_request(param, headers)
}

/// La langue d'une bio stockée convient-elle à celle qu'on demande ?
///
/// Comparaison sur la base seule : `fr` et `fr-FR` désignent la même chose, et
/// une bio Wikipédia française n'est pas moins française parce qu'un client
/// annonce `fr-CA`.
///
/// ⚠️ Une langue INCONNUE — colonne vide sur une ligne ancienne — est acceptée.
/// C'est délibéré : refuser déclencherait un appel réseau pour chaque artiste
/// dont la provenance n'a jamais été renseignée, à la première ouverture de
/// chaque fiche. On ne dispose d'aucune preuve que ces bios soient dans la
/// mauvaise langue ; un ré-enrichissement renseignera `bio_lang` et la
/// comparaison redeviendra exacte.
pub(super) fn langue_convient(stockee: Option<&str>, demandee: &str) -> bool {
    let Some(stockee) = stockee.map(str::trim).filter(|s| !s.is_empty()) else {
        return true;
    };
    let base = |s: &str| {
        s.split(['-', '_'])
            .next()
            .unwrap_or("")
            .to_ascii_lowercase()
    };
    base(stockee) == base(demandee)
}

/// Ressert la bio stockée quand la langue demandée n'a rien donné.
///
/// « Une biographie en français vaut mieux qu'un panneau vide » — le ticket le
/// dit, et c'est juste : refuser une bio dans la mauvaise langue serait
/// remplacer un défaut par un pire. La provenance part avec elle, donc le
/// client sait dans quelle langue elle est et peut le signaler.
fn repli_sur_la_bio_stockee(
    nom: &str,
    bio: Option<&str>,
    prov: Option<Value>,
) -> axum::response::Response {
    match bio.filter(|b| !b.is_empty()) {
        Some(b) => Json(json!({
            "artist": nom,
            "bio": b,
            "bio_provenance": prov,
        }))
        .into_response(),
        None => Json(json!({"artist": nom, "bio": null})).into_response(),
    }
}

// ── #6555 — les similaires d'un artiste de la BIBLIOTHÈQUE (fil 2341) ──────
//
// Bilou : « lorsque on sélectionne un artiste et que l'on clique sur un des
// choix d'artistes similaires […] il faut remonter à l'artiste initial puis
// redescendre ». La fiche d'un artiste de la bibliothèque lit ses
// similaires (« À propos ») par `GET /library/artists/{id}/metadata`. Cette
// route, et sa sœur `/similar`, interrogeaient `https://mozaiklabs.fr/api/
// {mbid}` : une adresse MORTE (404), le cloud vit sous `/api/v1/artists`.
// Elles retombaient sur la fiche locale, SANS similaires. La fiche n'en
// montrait donc qu'après « Enrichir » (Last.fm), en mémoire seulement : ouvrir
// un voisin rechargeait la fiche, et ses propres similaires n'y étaient pas.
//
// Même correction que #1730 pour `/metadata/artists/{id}/similar` : le
// client du cloud (`ArtistEnrichmentClient`), qui porte l'adresse de
// référence et lit le réglage `artist_enrichment_api` ; et, pour un artiste
// sans MBID (neuf sur dix en bibliothèque), la résolution EXACTE par le nom.

/// Délai du cloud pour une fiche : elle charge ses similaires sans bloquer
/// la discographie, mais une fiche ouverte ne doit pas attendre une minute.
const DELAI_CLOUD_FICHE_SECS: u64 = 10;

pub(super) fn client_cloud(
    state: &AppState,
) -> tune_core::metadata::artist_enrichment::ArtistEnrichmentClient {
    let base = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone())
        .get("artist_enrichment_api")
        .ok()
        .flatten();
    tune_core::metadata::artist_enrichment::ArtistEnrichmentClient::new(
        base.as_deref(),
        DELAI_CLOUD_FICHE_SECS,
    )
}

/// Le MBID sous lequel le cloud connaît l'artiste : le sien, ou celui que la
/// recherche EXACTE par le nom rend. La résolution est gardée 24 h, un échec
/// aussi : une fiche ouverte dix fois ne coûte qu'une recherche.
pub(super) async fn mbid_cloud(
    state: &AppState,
    client: &mut tune_core::metadata::artist_enrichment::ArtistEnrichmentClient,
    artiste: &tune_core::db::models::Artist,
) -> Option<String> {
    if let Some(m) = artiste
        .musicbrainz_id
        .as_deref()
        .map(str::trim)
        .filter(|m| !m.is_empty())
    {
        return Some(m.to_string());
    }
    // #6463 — une identité détachée par l'utilisateur ne se retrouve pas par
    // le nom : la fiche du cloud serait celle de l'homonyme qu'il a écarté.
    if let Some(id) = artiste.id
        && ArtistRepo::with_backend(state.backend.clone())
            .identite_detachee(id)
            .unwrap_or(false)
    {
        return None;
    }
    let cle = format!("cache:meta-nom:{}", artiste.name.trim().to_lowercase());
    if let Some(v) = api_cache_get(&state.backend, &cle)
        && v.is_object()
    {
        return v.get("mbid").and_then(|m| m.as_str()).map(str::to_owned);
    }
    let trouve = client.resolve_mbid(&artiste.name).await;
    api_cache_set(&state.backend, &cle, &json!({ "mbid": trouve }));
    trouve
}

/// Une fiche du cloud digne de ce nom : `{"data": {…}}`. Un `{}` (ancienne
/// réponse illisible gardée en cache) n'en est pas une.
fn fiche_cloud_valide(v: &Value) -> bool {
    v.get("data").is_some_and(|d| d.is_object())
}

pub(super) async fn artist_similar(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let repo = ArtistRepo::with_backend(state.backend.clone());
    let artist = repo.get(id).ok().flatten();
    let Some(artist) = artist else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let mut client = client_cloud(&state);
    let Some(mbid) = mbid_cloud(&state, &mut client, &artist).await else {
        return Json(json!({"artist": artist.name, "artists": []})).into_response();
    };
    let cache_key = format!("cache:similar:{mbid}");
    if let Some(cached) = api_cache_get(&state.backend, &cache_key)
        && cached
            .get("artists")
            .is_some_and(|a| a.as_array().is_some_and(|a| !a.is_empty()))
    {
        return Json(cached).into_response();
    }
    let voisins = client.get_similar(&mbid).await;
    let data = json!({"artist": artist.name, "mbid": mbid, "artists": voisins});
    if !voisins.is_empty() {
        api_cache_set(&state.backend, &cache_key, &data);
    }
    Json(data).into_response()
}

pub(super) async fn artist_metadata(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let repo = ArtistRepo::with_backend(state.backend.clone());
    let artist = repo.get(id).ok().flatten();
    let Some(artist) = artist else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let mut client = client_cloud(&state);
    let Some(mbid) = mbid_cloud(&state, &mut client, &artist).await else {
        return Json(json!(artist)).into_response();
    };
    let cache_key = format!("cache:meta:{mbid}");
    if let Some(cached) = api_cache_get(&state.backend, &cache_key)
        && fiche_cloud_valide(&cached)
    {
        return Json(cached).into_response();
    }
    match client.get_artist(&mbid).await {
        Some(fiche) if fiche_cloud_valide(&fiche.fields) => {
            api_cache_set(&state.backend, &cache_key, &fiche.fields);
            Json(fiche.fields).into_response()
        }
        _ => Json(json!(artist)).into_response(),
    }
}

#[cfg(test)]
#[path = "similaires_en_chaine_tests_6555.rs"]
mod similaires_en_chaine_tests_6555;

/// Paramètres de `GET /library/artists/{id}/albums`.
#[derive(Deserialize)]
pub(super) struct ArtistAlbumsQuery {
    /// `sections=1` : rendre l'objet à sections de #4767 au lieu du tableau
    /// nu. La valeur par défaut garde le tableau : la route est lue par le
    /// client web, les clients natifs et le serveur média, et changer la
    /// forme pour tout le monde casserait ce qui marche.
    sections: Option<String>,
}

use tune_core::metadata::release_type;

pub(super) async fn artist_albums(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Query(q): Query<ArtistAlbumsQuery>,
) -> Json<Value> {
    let repo = AlbumRepo::with_backend(state.backend.clone());
    let mut items = repo.list_by_artist(id).unwrap_or_default();
    // La fiche artiste trie ses albums par date d'ajout (Bertrand,
    // 16/09/2026) ; `select_album()` ne porte pas la colonne.
    repo.attacher_added_at(&mut items);
    // #5616 — le type DÉDUIT des disques sans type explicite (règle pistes et
    // durée de `release_type`), publié À CÔTÉ de `release_type`, jamais à sa
    // place. Clé absente quand rien n'est déduit. Champ additif : le tableau
    // nu des clients natifs le porte aussi, sans changer de forme.
    let deduits = repo.types_deduits(&items);
    // Section « Live » (05/10/2026) — les types SECONDAIRES MusicBrainz
    // (`albums.release_secondary_types`), publiés en tableau sous
    // `release_secondary_types`. Clé absente quand rien n'est connu.
    let ids: Vec<i64> = items.iter().filter_map(|a| a.id).collect();
    let secondaires = repo.types_secondaires_par_album(&ids).unwrap_or_else(|e| {
        tracing::warn!(error = %e, "types_secondaires_par_album a échoué — fiche sans section Live");
        Default::default()
    });
    let items: Vec<(bool, Value)> = items
        .iter()
        .map(|a| {
            let mut v = a.to_json();
            let types = a.id.and_then(|id| secondaires.get(&id));
            if let Some(o) = v.as_object_mut() {
                if let Some(t) = a.id.and_then(|id| deduits.get(&id)) {
                    o.insert("inferred_release_type".into(), json!(t.as_str()));
                }
                if let Some(types) = types {
                    o.insert("release_secondary_types".into(), json!(types));
                }
            }
            // #6464 — règle PROVISOIRE par le titre (« Live », « unplugged »),
            // seulement quand aucun type secondaire n'est connu : une réponse
            // explicite (`live` ou non) n'est jamais contredite par le titre.
            let par_le_type = types.is_some_and(|t| release_type::est_live(t));
            let par_le_titre = types.is_none() && release_type::titre_annonce_un_live(&a.title);
            if par_le_titre && let Some(o) = v.as_object_mut() {
                o.insert("live_by_title".into(), json!(true));
            }
            (par_le_type || par_le_titre, v)
        })
        .collect();
    // #6738 — les écoutes de la communauté de chaque album de la fiche.
    let (lives, mut valeurs): (Vec<bool>, Vec<Value>) = items.into_iter().unzip();
    ecoutes_communaute::annoter(
        &state.backend,
        &state.http_client,
        Genre::Album,
        &mut valeurs,
    )
    .await;
    let items: Vec<(bool, Value)> = lives.into_iter().zip(valeurs).collect();

    // Même lecture du drapeau que `proposals.rs` et `reports.rs`.
    if !q
        .sections
        .as_deref()
        .is_some_and(|v| v == "true" || v == "1")
    {
        // Le tableau nu garde TOUS les albums, lives compris : sa forme et son
        // contenu ne changent pas pour les clients natifs.
        let items: Vec<Value> = items.into_iter().map(|(_, v)| v).collect();
        return Json(json!(items));
    }

    // Section « Live » : un disque dont les types secondaires portent `live`
    // quitte « albums » pour « live », QUEL QUE SOIT son type primaire (un EP
    // live est un live). Clé absente quand la section est vide, comme les
    // autres.
    let (live, items): (Vec<_>, Vec<_>) = items.into_iter().partition(|(live, _)| *live);
    let items: Vec<Value> = items.into_iter().map(|(_, v)| v).collect();
    let live: Vec<Value> = live.into_iter().map(|(_, v)| v).collect();

    // #4767 — deux sections de plus, à partir de l'artiste de CHAQUE piste.
    // Une section vide est ABSENTE de la réponse : le client ne doit jamais
    // avoir à décider s'il affiche un titre au-dessus de rien.
    let mut reponse = json!({ "albums": items });
    let obj = reponse.as_object_mut().expect("objet");
    if !live.is_empty() {
        obj.insert("live".into(), json!(live));
    }
    for (cle, albums) in [
        (
            "compilations",
            repo.list_compilations_with_artist_track(id)
                .unwrap_or_default(),
        ),
        (
            "appearances",
            repo.list_appearances_of_artist(id).unwrap_or_default(),
        ),
    ] {
        if albums.is_empty() {
            continue;
        }
        let mut albums = albums;
        repo.attacher_added_at(&mut albums);
        let albums: Vec<Value> = albums.iter().map(|a| a.to_json()).collect();
        obj.insert(cle.to_string(), json!(albums));
    }
    inserer_sections_de_credits(&repo, &state, id, obj);
    Json(reponse)
}

/// #4767 — « Collaborations » et « Reprises », lues dans `track_credits`
/// (rempli par `POST /system/enrich-credits`).
///
/// Ne regarde que les disques d'AUTRUI où l'artiste n'est l'artiste d'aucune
/// piste : ceux-là sont déjà dans la discographie, « Compilations » ou
/// « Apparitions », et un album ne s'affiche jamais deux fois.
///
/// Forme, pour chaque album : l'objet album habituel, plus
/// - `focus_track_ids` : les pistes où l'artiste est crédité au titre de la
///   section (musicien pour « Collaborations », auteur pour « Reprises ») —
///   c'est le FOCUS : `tracks.artist_id` n'y désigne pas l'artiste, le client
///   filtre donc sur ces identifiants ;
/// - `credit_roles` : ce qu'il y fait (instruments, `vocals`, `composer`,
///   `writer`…), sans doublon.
///
/// `collaborations` est groupée par ARTISTE PRINCIPAL de l'album :
/// `[{artist_id, artist_name, albums: [...]}]`, groupes par nom, albums par
/// année. `covers` est une liste d'albums par année. Chaque clé est ABSENTE
/// quand sa section est vide, comme `compilations` et `appearances`.
fn inserer_sections_de_credits(
    repo: &AlbumRepo,
    state: &AppState,
    artist_id: i64,
    obj: &mut serde_json::Map<String, Value>,
) {
    use tune_core::metadata::credits_release::{AlbumCredite, classer, credits_hors_discographie};

    let (credits, disques) = credits_hors_discographie(&state.backend, artist_id);
    if credits.is_empty() {
        return;
    }
    let classement = classer(&credits, &disques);

    // Les albums d'une section, triés par année puis titre, avec leur focus.
    let charger = |entrees: &[AlbumCredite]| -> Vec<(tune_core::db::models::Album, Value)> {
        let (mut seuls, trouvees): (Vec<tune_core::db::models::Album>, Vec<&AlbumCredite>) =
            entrees
                .iter()
                .filter_map(|e| repo.get(e.album_id).ok().flatten().map(|a| (a, e)))
                .unzip();
        repo.attacher_added_at(&mut seuls);
        let mut albums: Vec<(tune_core::db::models::Album, &AlbumCredite)> =
            seuls.into_iter().zip(trouvees).collect();
        albums.sort_by(|(a, _), (b, _)| {
            a.year
                .unwrap_or(i32::MAX)
                .cmp(&b.year.unwrap_or(i32::MAX))
                .then_with(|| a.title.to_lowercase().cmp(&b.title.to_lowercase()))
        });
        albums
            .into_iter()
            .map(|(a, e)| {
                let mut v = a.to_json();
                if let Some(o) = v.as_object_mut() {
                    o.insert("focus_track_ids".into(), json!(e.track_ids));
                    o.insert("credit_roles".into(), json!(e.roles));
                }
                (a, v)
            })
            .collect()
    };

    let collaborations = charger(&classement.collaborations);
    if !collaborations.is_empty() {
        // Groupes dans l'ordre des noms ; albums déjà triés par année.
        let mut groupes: Vec<(Option<i64>, String, Vec<Value>)> = Vec::new();
        for (album, v) in collaborations {
            let nom = album.artist_name.clone().unwrap_or_default();
            match groupes
                .iter_mut()
                .find(|(id, n, _)| *id == album.artist_id && *n == nom)
            {
                Some((_, _, albums)) => albums.push(v),
                None => groupes.push((album.artist_id, nom, vec![v])),
            }
        }
        groupes.sort_by_key(|g| g.1.to_lowercase());
        let groupes: Vec<Value> = groupes
            .into_iter()
            .map(|(id, nom, albums)| json!({ "artist_id": id, "artist_name": nom, "albums": albums }))
            .collect();
        obj.insert("collaborations".into(), json!(groupes));
    }

    let reprises: Vec<Value> = charger(&classement.reprises)
        .into_iter()
        .map(|(_, v)| v)
        .collect();
    if !reprises.is_empty() {
        obj.insert("covers".into(), json!(reprises));
    }
}

pub(super) async fn artist_tracks(
    State(state): State<AppState>,
    profile: crate::routes::active_profile::ActiveProfile,
    Path(id): Path<i64>,
) -> Json<Value> {
    let repo = TrackRepo::with_backend(state.backend.clone());
    let items = dedup_display_tracks(repo.list_by_artist(id).unwrap_or_default());
    // #4806 — `banned` ici aussi : la liste ne change pas, elle s'annote.
    // Passer par `Vec<Value>` lui donne au passage `channel_badge` (additif).
    let mut items: Vec<Value> = items.iter().map(|t| t.to_json()).collect();
    super::albums::attacher_banni(&state, profile.id(), &mut items);
    // #6738 — « So What (128) » : les écoutes de la communauté.
    ecoutes_communaute::annoter(&state.backend, &state.http_client, Genre::Piste, &mut items).await;
    Json(json!(items))
}

pub(super) async fn artist_timeline(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let repo = AlbumRepo::with_backend(state.backend.clone());
    let artist_repo = ArtistRepo::with_backend(state.backend.clone());
    let artist = match artist_repo.get(id) {
        Ok(Some(a)) => a,
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    let mut albums = repo.list_by_artist(id).unwrap_or_default();
    albums.sort_by_key(|a| a.year.unwrap_or(0));

    let years: Vec<i32> = albums.iter().filter_map(|a| a.year).collect();
    let mut gaps = Vec::new();
    for w in years.windows(2) {
        if w[1] - w[0] > 1 {
            gaps.push(json!({"from": w[0], "to": w[1], "years": w[1] - w[0]}));
        }
    }

    let items: Vec<Value> = albums.iter().map(|a| a.to_json()).collect();
    Json(json!({
        "artist": artist.name,
        "artist_id": id,
        "albums": items,
        "gaps": gaps,
        "career_span": if years.len() >= 2 { Some(json!({"first": years[0], "last": years[years.len()-1], "years": years[years.len()-1] - years[0]})) } else { None },
    }))
    .into_response()
}

pub(super) async fn artist_image(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let repo = ArtistRepo::with_backend(state.backend.clone());
    let artist = match repo.get(id) {
        Ok(Some(a)) => a,
        _ => return StatusCode::NOT_FOUND.into_response(),
    };

    let Some(ref image_path) = artist.image_path else {
        return StatusCode::NOT_FOUND.into_response();
    };

    if image_path.starts_with("http") {
        return axum::response::Redirect::temporary(image_path).into_response();
    }

    // If it's already a hex hash, use it directly; otherwise hash it
    let hash = if super::artwork_is_hex_hash(image_path) {
        image_path.to_string()
    } else {
        tune_core::library::artwork::artwork_hash(image_path)
    };

    axum::response::Redirect::temporary(&format!("/api/v1/library/artwork/{hash}")).into_response()
}

/// Taille maximale d'une image d'artiste deposee a la main.
///
/// Le `DefaultBodyLimit` global (50 Mo) laisse passer n'importe quelle photo
/// d'appareil. Elle serait ecrite dans le cache, puis relue entierement a
/// chaque vignette. Au-dela de cette borne la route le DIT, au lieu de
/// repondre 200 (#3102).
pub(super) const IMAGE_ARTISTE_MAX_OCTETS: usize = 10 * 1024 * 1024;

/// Le coeur de [`artist_image_upload`], repertoire de cache donne.
///
/// Separe du gestionnaire pour etre eprouve sans variable d'environnement :
/// `artwork_cache_dir()` lit `TUNE_ARTWORK_DIR`, commun a tout le processus et
/// donc inutilisable depuis des essais paralleles. Meme decoupage que
/// `serve_artwork_from`, juste en face.
///
/// La reponse porte l'ARTISTE RELU EN BASE — `image_path` compris. Elle ne
/// portait que `artist_id`, `hash` et `size` : le client la lit pourtant comme
/// un artiste complet (`api.ts:1390` la type `Promise<Artist>`,
/// `ArtistEditModal.svelte:49` fait `updated.image_path ?? null`), donc
/// `image_path` valait `undefined`, l'apercu retombait sur le gabarit vide, et
/// le testeur voyait « enregistre » avec une vignette vide (#3102, Fuccaro).
pub(super) fn enregistrer_image_artiste(
    artist_repo: &ArtistRepo,
    id: i64,
    data: &[u8],
    cache_dir: &std::path::Path,
) -> Response {
    let mut artist = match artist_repo.get(id) {
        Ok(Some(a)) => a,
        _ => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "artist not found"})),
            )
                .into_response();
        }
    };

    if data.len() > IMAGE_ARTISTE_MAX_OCTETS {
        return (
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(json!({
                "error": "image too large",
                "size": data.len(),
                "max_size": IMAGE_ARTISTE_MAX_OCTETS,
            })),
        )
            .into_response();
    }

    // Le format vient des OCTETS, jamais du `content-type` annonce par le
    // client : celui-ci n'etait consulte que pour y chercher « png », et tout
    // le reste — WebP de Discogs compris — etait ecrit sous `.jpg` puis servi
    // `image/jpeg`. Un format que la lecture ne sait pas resservir est refuse
    // EN LE DISANT (#3102).
    let Some(ext) = tune_core::library::artwork::sniff_image_ext(data) else {
        return (
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            Json(json!({
                "error": "unsupported image format",
                "supported": tune_core::library::artwork::FORMATS_IMAGE_SERVABLES,
            })),
        )
            .into_response();
    };

    // Condensat de CONTENU, plus d'identite figee (#1444) : sous
    // `artwork_hash("artist-{id}")`, remplacer l'image gardait la meme URL
    // servie `immutable` — l'ancienne image restait affichee. Voir le
    // commentaire complet dans `artwork.rs::upload_album_artwork`.
    let hash = tune_core::library::artwork::content_hash(data);
    if tune_core::library::artwork::save_to_cache(data, cache_dir, &hash, ext).is_none() {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "failed to save image"})),
        )
            .into_response();
    }

    artist.image_path = Some(hash.clone());
    artist.image_source = Some("upload".into());
    // L'ecriture etait jetee par un `.ok()` : une base en echec repondait
    // quand meme 200 avec le meme corps, et le prochain chargement perdait
    // l'image sans que rien ne l'ait dit.
    if let Err(e) = artist_repo.update(&artist) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("update failed: {e}")})),
        )
            .into_response();
    }

    // On rend l'artiste RELU, pas celui qu'on vient de construire : la reponse
    // dit alors ce que le prochain chargement lira, et non ce qu'on esperait
    // avoir ecrit.
    let enregistre = match artist_repo.get(id) {
        Ok(Some(a)) if a.image_path.as_deref() == Some(hash.as_str()) => a,
        _ => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "image saved but not indexed"})),
            )
                .into_response();
        }
    };

    let mut corps = json!(enregistre);
    if let Some(obj) = corps.as_object_mut() {
        // Les trois cles historiques restent : d'autres clients les lisent.
        obj.insert("artist_id".into(), json!(id));
        obj.insert("hash".into(), json!(hash));
        obj.insert("size".into(), json!(data.len()));
    }
    Json(corps).into_response()
}

pub(super) async fn artist_image_upload(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    mut multipart: Multipart,
) -> impl IntoResponse {
    let mut image_data: Option<Vec<u8>> = None;
    while let Ok(Some(field)) = multipart.next_field().await {
        let name = field.name().unwrap_or("").to_string();
        if name == "image" || name == "file" {
            image_data = field.bytes().await.ok().map(|b| b.to_vec());
        }
    }

    let Some(data) = image_data else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "no image provided"})),
        )
            .into_response();
    };

    let artist_repo = ArtistRepo::with_backend(state.backend.clone());
    enregistrer_image_artiste(&artist_repo, id, &data, &artwork_cache_dir())
}

pub(super) async fn artist_image_report(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    // Optional body: the web flag button POSTs with no JSON payload, so a
    // required `Json<ImageReportBody>` extractor rejected it with 422 and the
    // UI showed "erreur lors du signalement" (Jean Valjean #1096). Accept a
    // missing/empty body; `reason` stays None.
    body: Option<Json<ImageReportBody>>,
) -> impl IntoResponse {
    let reason = body.and_then(|Json(b)| b.reason);
    // Recorded in metadata_reports like every other report. It used to be
    // stashed as a settings key (reported_artist_image_{id}), which no code
    // could list or aggregate and which never reached the community backend.
    // Le signalement garde l'empreinte de l'image et l'efface (Jean Valjean
    // #1096) ; l'enrichissement suivant ne la repose plus (#4837) — même
    // chemin que `POST /library/reports`.
    let cleared = tune_core::library::artwork::signaler_image_artiste(
        &state.backend,
        &artwork_cache_dir(),
        id,
        None,
        reason.as_deref().unwrap_or("incorrect_image"),
        None,
        &now_iso_utc(),
    )
    .unwrap_or(false);
    Json(json!({"reported": true, "artist_id": id, "image_cleared": cleared}))
}

// ---------------------------------------------------------------------------
// PUT /artists/{id} — update artist metadata (name, sort_name, bio)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub(super) struct ArtistUpdate {
    name: Option<String>,
    sort_name: Option<String>,
    bio: Option<String>,
    musicbrainz_id: Option<String>,
}

/// Un MBID vide ou réduit à des espaces n'en est pas un.
fn mbid_utilisable(v: Option<&str>) -> Option<String> {
    v.map(str::trim)
        .filter(|m| !m.is_empty())
        .map(str::to_string)
}

/// `POST /library/artists/{id}/detacher-identite` — « Détacher l'identité »
/// (#6463, décision de Bertrand du 23/10).
///
/// « Ange » (fil 2394) portait le MBID d'une chanteuse homonyme, et avec lui
/// son portrait. Le geste retire le MBID, marque la provenance `manuel`
/// (détachée par l'utilisateur : aucune passe automatique — scan, recherche
/// par le nom, pressage, enrichissement — ne le réattache), efface le
/// portrait et la bio que ce MBID avait rapportés, et relance la recherche
/// d'image de CET artiste quand son portrait est parti (même garde que le
/// bouton « Enrichir les images d'artistes » : Premium ou quota du jour).
///
/// Pour lui redonner une identité : `PUT /library/artists/{id}` avec un
/// `musicbrainz_id` (provenance `manuel` aussi).
pub(super) async fn detacher_identite(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let repo = ArtistRepo::with_backend(state.backend.clone());
    let artiste = match repo.get(id) {
        Ok(Some(a)) => a,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": e.to_string()})),
            )
                .into_response();
        }
    };
    let mbid_retire = mbid_utilisable(artiste.musicbrainz_id.as_deref());
    if let Err(e) = repo.poser_mbid_manuel(id, None) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response();
    }
    let (image_effacee, bio_effacee) = if mbid_retire.is_some() {
        match repo.oublier_ce_que_le_mbid_a_apporte(id) {
            Ok(effacees) => effacees,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": e.to_string()})),
                )
                    .into_response();
            }
        }
    } else {
        (false, false)
    };
    tracing::info!(
        artist_id = id,
        artist = %artiste.name,
        mbid = mbid_retire.as_deref().unwrap_or(""),
        image_effacee,
        bio_effacee,
        "artiste_identite_detachee"
    );

    let enrichissement_image = if !image_effacee {
        "inutile"
    } else {
        match crate::routes::system::gate_enrichment(&state).await {
            Ok(_) => {
                let db = state.backend.clone();
                let portee = tune_core::metadata::enrich_scope::EnrichScope {
                    dir: format!("artiste:{id}"),
                    album_ids: Default::default(),
                    artist_ids: [id].into_iter().collect(),
                    track_count: 0,
                };
                tokio::spawn(async move {
                    tune_core::library::artwork::batch_enrich_artist_artwork_scoped(
                        db,
                        artwork_cache_dir(),
                        Some(portee),
                    )
                    .await;
                });
                "lance"
            }
            Err(_) => "refuse",
        }
    };

    let mut corps = match repo.get(id) {
        Ok(Some(a)) => json!(a),
        _ => json!({"id": id}),
    };
    if let Some(obj) = corps.as_object_mut() {
        obj.insert("musicbrainz_id_source".into(), json!("manuel"));
        obj.insert("mbid_retire".into(), json!(mbid_retire));
        obj.insert("image_effacee".into(), json!(image_effacee));
        obj.insert("bio_effacee".into(), json!(bio_effacee));
        obj.insert("enrichissement_image".into(), json!(enrichissement_image));
    }
    Json(corps).into_response()
}

pub(super) async fn update_artist(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<ArtistUpdate>,
) -> impl IntoResponse {
    let repo = ArtistRepo::with_backend(state.backend.clone());
    let mut artist = match repo.get(id) {
        Ok(Some(a)) => a,
        _ => return StatusCode::NOT_FOUND.into_response(),
    };

    if let Some(ref v) = body.name {
        let trimmed = v.trim();
        if trimmed.is_empty() {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "artist name cannot be empty"})),
            )
                .into_response();
        }
        artist.name = trimmed.to_string();
    }
    if let Some(ref v) = body.sort_name {
        artist.sort_name = Some(v.clone());
    }
    let bio_avant = artist.bio.clone();
    if let Some(ref v) = body.bio {
        artist.bio = if v.is_empty() { None } else { Some(v.clone()) };
    }
    // #6463 — un MBID saisi, ou effacé, est une décision de l'utilisateur :
    // provenance `manuel`. Seulement s'il CHANGE : renvoyer le même MBID avec
    // un autre champ ne fige pas une provenance `nom` à vérifier.
    let mbid_avant = mbid_utilisable(artist.musicbrainz_id.as_deref());
    let mbid_saisi = body
        .musicbrainz_id
        .as_deref()
        .map(|v| mbid_utilisable(Some(v)))
        .filter(|nouveau| *nouveau != mbid_avant);
    if let Some(ref nouveau) = mbid_saisi {
        artist.musicbrainz_id = nouveau.clone();
    }

    if let Err(e) = repo.update(&artist) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("update failed: {e}")})),
        )
            .into_response();
    }
    if let Some(ref nouveau) = mbid_saisi {
        if let Err(e) = repo.poser_mbid_manuel(id, nouveau.as_deref()) {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("update failed: {e}")})),
            )
                .into_response();
        }
        // Le portrait et la bio de l'ANCIEN MBID ne décrivent plus l'artiste.
        if mbid_avant.is_some()
            && let Err(e) = repo.oublier_ce_que_le_mbid_a_apporte(id)
        {
            tracing::warn!(artist_id = id, error = %e, "artist_mbid_images_bio_oubli_echec");
        }
    }
    if let Err(e) =
        repo.oublier_provenance_si_bio_reecrite(id, bio_avant.as_deref(), artist.bio.as_deref())
    {
        tracing::warn!(artist_id = id, error = %e, "artist_bio_provenance_clear_failed");
    }

    if body.name.is_some() {
        let db = state.backend.clone();
        let new_name = artist.name.clone();
        use tune_core::db::backend::ToSqlValue;
        use tune_core::db::engine::Engine;
        let p1 = if db.engine() == Engine::Postgres {
            "$1"
        } else {
            "?1"
        };
        let p2 = if db.engine() == Engine::Postgres {
            "$2"
        } else {
            "?2"
        };
        let _ = db.execute(
            &format!("UPDATE tracks SET artist_name = {p1} WHERE artist_id = {p2}"),
            &[&new_name as &dyn ToSqlValue, &id as &dyn ToSqlValue],
        );
        let _ = db.execute(
            &format!("UPDATE albums SET artist_name = {p1} WHERE artist_id = {p2}"),
            &[&new_name as &dyn ToSqlValue, &id as &dyn ToSqlValue],
        );
    }

    Json(json!(artist)).into_response()
}

#[cfg(test)]
mod tests_proxy_bio {
    use super::reponse_du_proxy;
    use serde_json::json;

    /// Le cas de site-mozaiklabs#278 : un extrait Wikipédia servi par le
    /// proxy part avec l'URL de l'article, la licence et la langue.
    #[test]
    fn un_extrait_wikipedia_garde_son_attribution() {
        let data = json!({
            "bio": "Kind of Blue est un album de Miles Davis.",
            "source": "wikipedia",
            "source_url": "https://fr.wikipedia.org/wiki/Kind_of_Blue",
            "license": "CC BY-SA 4.0",
            "lang": "fr",
        });
        let out = reponse_du_proxy("album", "Kind of Blue", &data);

        assert_eq!(out["album"], "Kind of Blue");
        assert_eq!(out["bio"], data["bio"]);
        assert_eq!(out["source"], "wikipedia", "`source` reste à plat");
        let prov = &out["bio_provenance"];
        assert_eq!(prov["source"], "wikipedia");
        assert_eq!(
            prov["source_url"],
            "https://fr.wikipedia.org/wiki/Kind_of_Blue"
        );
        assert_eq!(
            prov["license"], "CC BY-SA 4.0",
            "licence relayée telle quelle"
        );
        assert_eq!(prov["lang"], "fr");
        assert!(prov["fetched_at"].as_str().is_some_and(|s| !s.is_empty()));
    }

    /// Même forme que `ArtistRepo::bio_provenance` : les cinq clés, toujours.
    #[test]
    fn la_provenance_a_la_forme_des_bios_stockees() {
        let out = reponse_du_proxy("artist", "Miles Davis", &json!({"bio": "Trompettiste."}));
        let prov = out["bio_provenance"].as_object().expect("provenance");
        let mut cles: Vec<&str> = prov.keys().map(String::as_str).collect();
        cles.sort_unstable();
        assert_eq!(
            cles,
            ["fetched_at", "lang", "license", "source", "source_url"]
        );
        // Sans source, l'étiquette de `bio_sync` ; champs absents = null.
        assert_eq!(prov["source"], "community");
        assert!(prov["source_url"].is_null());
        assert!(prov["license"].is_null());
        assert!(prov["lang"].is_null());
        assert!(out["source"].is_null(), "`source` à plat inchangé : null");
    }

    #[test]
    fn une_bio_d_ia_garde_sa_source() {
        let out = reponse_du_proxy(
            "artist",
            "Miles Davis",
            &json!({"bio": "Bio.", "source": "claude"}),
        );
        assert_eq!(out["bio_provenance"]["source"], "claude");
        assert!(out["bio_provenance"]["license"].is_null());
    }

    /// Pas de bio, pas de provenance : rien à attribuer.
    #[test]
    fn sans_bio_aucune_provenance() {
        for data in [
            json!({"bio": null, "source": null}),
            json!({}),
            json!({"bio": "  "}),
        ] {
            let out = reponse_du_proxy("artist", "Miles Davis", &data);
            assert!(out.get("bio_provenance").is_none(), "{data}");
        }
    }

    /// Des champs vides ou d'un mauvais type ne passent pas pour une valeur.
    #[test]
    fn les_champs_vides_sont_nuls() {
        let data =
            json!({"bio": "Bio.", "source": "", "source_url": " ", "license": 4, "lang": ""});
        let prov = &reponse_du_proxy("album", "X", &data)["bio_provenance"];
        assert_eq!(prov["source"], "community");
        assert!(prov["source_url"].is_null());
        assert!(prov["license"].is_null());
        assert!(prov["lang"].is_null());
    }
}

#[cfg(test)]
mod tests_langue_bio {
    use super::langue_convient;

    /// Le cas de Dimitri (#1849) : bibliothèque enrichie en français, lecteur
    /// anglophone. La bio stockée ne doit PAS être resservie telle quelle.
    #[test]
    fn une_bio_francaise_ne_convient_pas_a_une_demande_anglaise() {
        assert!(!langue_convient(Some("fr"), "en"));
    }

    #[test]
    fn la_meme_langue_convient() {
        assert!(langue_convient(Some("fr"), "fr"));
        assert!(langue_convient(Some("en"), "en"));
    }

    /// `fr` et `fr-FR` désignent la même chose : une bio Wikipédia française
    /// n'est pas moins française parce qu'un client annonce `fr-CA`.
    #[test]
    fn seule_la_base_de_la_langue_compte() {
        assert!(langue_convient(Some("fr"), "fr-FR"));
        assert!(langue_convient(Some("fr-FR"), "fr"));
        assert!(langue_convient(Some("fr_CA"), "fr-BE"));
        assert!(langue_convient(Some("EN"), "en-GB"));
        assert!(!langue_convient(Some("fr-FR"), "en-GB"));
    }

    /// Une langue inconnue est ACCEPTÉE, délibérément. Refuser déclencherait un
    /// appel réseau pour chaque artiste dont la provenance n'a jamais été
    /// renseignée, à la première ouverture de chaque fiche — et rien ne dit que
    /// ces bios soient dans la mauvaise langue.
    #[test]
    fn une_langue_inconnue_est_acceptee() {
        assert!(langue_convient(None, "en"));
        assert!(langue_convient(Some(""), "en"));
        assert!(langue_convient(Some("   "), "en"));
    }
}

// ---------------------------------------------------------------------------
// #3102 — l'image d'artiste posee a la main.
// ---------------------------------------------------------------------------
//
// Fuccaro, fil forum 1317, 01/09/2026, PC sous Windows 11 : « J'essaye de les
// ajouter manuellement via le bouton "editer l'artiste", en telechargeant une
// image sur Discogs. Le message est : "enregistre" mais je n'ai pas l'image
// qui s'affiche. »
//
// Aucun essai ici ne se contente d'un code HTTP — le symptome est justement
// que le 200 mentait. Chacun va chercher les OCTETS que la route de service
// rend a l'adresse que la reponse annonce.
#[cfg(test)]
mod tests_image_artiste_televersee {
    use super::*;
    use std::sync::Arc;
    use tune_core::db::backend::DbBackend;
    use tune_core::db::models::Artist;

    fn base_memoire() -> Arc<dyn DbBackend> {
        let db = tune_core::db::sqlite::SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        tune_core::db::migrations::run_migrations(&db).unwrap();
        Arc::new(db)
    }

    fn artiste(repo: &ArtistRepo, nom: &str) -> i64 {
        repo.create(&Artist::new(nom.into()))
            .expect("insert artist")
    }

    /// Un PNG 1x1 VALIDE, fabrique ici octet par octet. Aucune image de
    /// testeur n'entre dans ce depot.
    fn png_1x1() -> Vec<u8> {
        vec![
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00,
            0x00, 0x1F, 0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78,
            0x9C, 0x63, 0x00, 0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00,
            0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
        ]
    }

    /// Le format que Discogs sert de plus en plus, et que l'ancien
    /// gestionnaire rangeait sous `.jpg`.
    fn webp_minimal() -> Vec<u8> {
        let mut v = b"RIFF".to_vec();
        v.extend_from_slice(&[0x14, 0, 0, 0]);
        v.extend_from_slice(b"WEBPVP8 ");
        v.extend_from_slice(b"octets-webp");
        v
    }

    async fn json_de(reponse: Response) -> (StatusCode, Value) {
        let statut = reponse.status();
        let octets = axum::body::to_bytes(reponse.into_body(), usize::MAX)
            .await
            .unwrap();
        (
            statut,
            serde_json::from_slice(&octets).unwrap_or(Value::Null),
        )
    }

    /// Ce que le navigateur obtient reellement a l'adresse annoncee : le code,
    /// le type MIME, et les octets. C'est la route de service de production
    /// (`serve_artwork_from`), pas une relecture de fichier a la main.
    async fn servi(cache: &std::path::Path, hash: &str) -> (StatusCode, String, Vec<u8>) {
        let reponse = super::super::artwork::serve_artwork_from(cache, hash, None).await;
        let statut = reponse.status();
        let mime = reponse
            .headers()
            .get("Content-Type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let octets = axum::body::to_bytes(reponse.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec();
        (statut, mime, octets)
    }

    /// L'EPREUVE QUI TRANCHE.
    ///
    /// Apres televersement, l'adresse rendue par la reponse sert reellement
    /// l'image televersee — et un second chargement, qui repart de la base
    /// comme le fait l'ecran apres un F5, la rend encore.
    ///
    /// Avant correctif la reponse ne portait que `artist_id`, `hash` et
    /// `size` : `body["image_path"]` etait absent, l'assertion tombe des la
    /// premiere ligne. Remettre ces trois cles seules dans
    /// `enregistrer_image_artiste` la fait retomber.
    #[tokio::test]
    async fn l_adresse_rendue_sert_l_image_televersee_et_la_rend_encore() {
        let cache = tune_core::test_scratch::scratch_dir("tune-3102-adresse");
        let repo = ArtistRepo::with_backend(base_memoire());
        let id = artiste(&repo, "Taj Mahal");
        let octets = png_1x1();

        let (statut, corps) =
            json_de(enregistrer_image_artiste(&repo, id, &octets, cache.path())).await;
        assert_eq!(statut, StatusCode::OK, "corps: {corps}");

        let adresse = corps["image_path"]
            .as_str()
            .unwrap_or_else(|| panic!("la reponse ne porte pas l'adresse de l'image : {corps}"))
            .to_string();

        // Premier chargement : ce que l'apercu du modal demande sur-le-champ.
        let (code, mime, servis) = servi(cache.path(), &adresse).await;
        assert_eq!(code, StatusCode::OK, "l'adresse annoncee n'est pas servie");
        assert_eq!(mime, "image/png");
        assert_eq!(servis, octets, "ce ne sont pas les octets televerses");

        // Second chargement : la fiche relue en base, comme apres un F5.
        let relu = repo.get(id).unwrap().expect("artiste present");
        assert_eq!(
            relu.image_path.as_deref(),
            Some(adresse.as_str()),
            "la base ne connait pas l'adresse que la reponse a annoncee"
        );
        assert_eq!(relu.image_source.as_deref(), Some("upload"));
        let (code2, _, servis2) = servi(cache.path(), relu.image_path.as_deref().unwrap()).await;
        assert_eq!(code2, StatusCode::OK);
        assert_eq!(servis2, octets, "le second chargement perd l'image");
    }

    /// Le format se lit dans les octets : un WebP est servi `image/webp`, pas
    /// range sous `.jpg` et annonce `image/jpeg`.
    #[tokio::test]
    async fn un_webp_est_servi_comme_un_webp() {
        let cache = tune_core::test_scratch::scratch_dir("tune-3102-webp");
        let repo = ArtistRepo::with_backend(base_memoire());
        let id = artiste(&repo, "Stretch");
        let octets = webp_minimal();

        let (statut, corps) =
            json_de(enregistrer_image_artiste(&repo, id, &octets, cache.path())).await;
        assert_eq!(statut, StatusCode::OK, "corps: {corps}");
        let adresse = corps["image_path"].as_str().expect("adresse").to_string();

        let (code, mime, servis) = servi(cache.path(), &adresse).await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(mime, "image/webp", "l'extension ecrite ment sur le contenu");
        assert_eq!(servis, octets);

        // TEMOIN de l'enrichissement automatique : c'est le predicat exact sur
        // lequel `batch_enrich_artist_artwork_inner` decide de remettre un
        // artiste dans sa file — donc d'ecraser l'image posee a la main.
        assert!(
            tune_core::library::artwork::cached_artwork_exists(cache.path(), &adresse),
            "la passe automatique croirait le cache vide et ecraserait l'image"
        );
    }

    /// TEMOIN : un artiste sans image n'en a toujours aucune, proprement.
    /// Aucune entree fantome, aucun condensat annonce dans le vide.
    #[tokio::test]
    async fn un_artiste_sans_image_n_en_annonce_aucune() {
        let cache = tune_core::test_scratch::scratch_dir("tune-3102-vide");
        let repo = ArtistRepo::with_backend(base_memoire());
        let id = artiste(&repo, "Charades");

        let relu = repo.get(id).unwrap().expect("artiste present");
        assert_eq!(relu.image_path, None);
        assert!(!tune_core::library::artwork::cached_artwork_exists(
            cache.path(),
            "ffffffffffffffffffffffffffffffff"
        ));
        let (code, _, _) = servi(cache.path(), "ffffffffffffffffffffffffffffffff").await;
        assert_eq!(code, StatusCode::NOT_FOUND);
    }

    /// TEMOIN : un fichier refuse echoue EN LE DISANT. Pas de 200, et surtout
    /// pas d'adresse annoncee en base pour un fichier que la lecture ne
    /// saurait pas resservir.
    #[tokio::test]
    async fn un_fichier_qui_n_est_pas_une_image_est_refuse_en_le_disant() {
        let cache = tune_core::test_scratch::scratch_dir("tune-3102-format");
        let repo = ArtistRepo::with_backend(base_memoire());
        let id = artiste(&repo, "Sam Cooke");

        let (statut, corps) = json_de(enregistrer_image_artiste(
            &repo,
            id,
            b"<!doctype html><title>page Discogs</title>",
            cache.path(),
        ))
        .await;
        assert_eq!(statut, StatusCode::UNSUPPORTED_MEDIA_TYPE);
        assert_eq!(corps["error"], "unsupported image format");
        assert_eq!(
            repo.get(id).unwrap().unwrap().image_path,
            None,
            "un refus ne doit rien indexer"
        );
    }

    /// TEMOIN : trop gros echoue en le disant, avec la borne dans le corps.
    #[tokio::test]
    async fn une_image_trop_grosse_est_refusee_en_le_disant() {
        let cache = tune_core::test_scratch::scratch_dir("tune-3102-taille");
        let repo = ArtistRepo::with_backend(base_memoire());
        let id = artiste(&repo, "Quarterflash");

        let mut enorme = png_1x1();
        enorme.resize(IMAGE_ARTISTE_MAX_OCTETS + 1, 0);
        let (statut, corps) =
            json_de(enregistrer_image_artiste(&repo, id, &enorme, cache.path())).await;
        assert_eq!(statut, StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(corps["max_size"], IMAGE_ARTISTE_MAX_OCTETS);
        assert_eq!(repo.get(id).unwrap().unwrap().image_path, None);
        assert_eq!(
            std::fs::read_dir(cache.path())
                .into_iter()
                .flatten()
                .count(),
            0,
            "rien ne doit etre ecrit dans le cache"
        );
    }

    /// TEMOIN : un artiste qui n'existe pas reste un 404, et le cache reste
    /// vide — le refus precede toute ecriture.
    #[tokio::test]
    async fn un_artiste_inconnu_reste_un_404() {
        let cache = tune_core::test_scratch::scratch_dir("tune-3102-inconnu");
        let repo = ArtistRepo::with_backend(base_memoire());
        let (statut, corps) = json_de(enregistrer_image_artiste(
            &repo,
            4242,
            &png_1x1(),
            cache.path(),
        ))
        .await;
        assert_eq!(statut, StatusCode::NOT_FOUND);
        assert_eq!(corps["error"], "artist not found");
        assert_eq!(
            std::fs::read_dir(cache.path())
                .into_iter()
                .flatten()
                .count(),
            0
        );
    }

    /// Le contrat de compatibilite : les trois cles que la reponse portait
    /// avant #3102 sont toujours la, a cote de l'artiste.
    #[tokio::test]
    async fn les_trois_cles_historiques_restent_dans_la_reponse() {
        let cache = tune_core::test_scratch::scratch_dir("tune-3102-cles");
        let repo = ArtistRepo::with_backend(base_memoire());
        let id = artiste(&repo, "Jermaine Jackson");
        let octets = png_1x1();

        let (_, corps) = json_de(enregistrer_image_artiste(&repo, id, &octets, cache.path())).await;
        assert_eq!(corps["artist_id"], id);
        assert_eq!(corps["size"], octets.len());
        assert_eq!(corps["hash"], corps["image_path"]);
        assert_eq!(corps["name"], "Jermaine Jackson");
    }
}

#[cfg(test)]
mod tests_artistes_homographes {
    use super::{ArtisteVue, grouper_les_artistes_homographes};

    fn a(id: i64, name: &str, mbid: Option<&str>, albums: i64) -> ArtisteVue {
        ArtisteVue {
            id,
            name: name.into(),
            musicbrainz_id: mbid.map(str::to_string),
            albums,
        }
    }

    /// BIB-C1 : les paires mesurées sur .18 tombent dans le même groupe, le
    /// plus ancien d'abord ; deux MBID différents sont signalés ; un artiste
    /// seul n'apparaît pas ; les groupes sûrs et fournis viennent en premier.
    #[test]
    fn les_homographes_sont_groupes_et_les_mbid_contradictoires_signales() {
        let artistes = vec![
            a(12, "Étienne Daho", Some("mb-daho"), 6),
            a(3, "Etienne Daho", None, 1),
            a(40, "The Rolling Stones", Some("mb-stones"), 9),
            a(41, "Rolling Stones", Some("mb-autre"), 1),
            a(50, "J.J. Cale", None, 2),
            a(51, "JJ Cale", None, 1),
            a(60, "Anouar Brahem", Some("mb-brahem"), 4),
            a(70, "", None, 0),
        ];
        let g = grouper_les_artistes_homographes(&artistes);
        assert_eq!(g.len(), 3, "{g:#?}");
        // sûrs et fournis d'abord : Daho (7 albums, MBID non contradictoires), Cale (3), puis Stones (MBID contradictoires)
        assert_eq!(g[0]["cle"], "etienne daho");
        assert_eq!(g[0]["artistes"][0]["id"], 3, "le plus ancien d'abord");
        assert_eq!(g[0]["artistes"][1]["id"], 12);
        assert_eq!(g[0]["mbid_distincts"], false);
        assert_eq!(g[1]["cle"], "jj cale");
        assert_eq!(g[2]["cle"], "rolling stones");
        assert_eq!(g[2]["mbid_distincts"], true);
        let ids: Vec<i64> = g
            .iter()
            .flat_map(|x| {
                x["artistes"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|y| y["id"].as_i64().unwrap())
            })
            .collect();
        assert!(!ids.contains(&60) && !ids.contains(&70));
    }
}

#[cfg(test)]
mod tests_absorption;

#[cfg(test)]
#[path = "identite_artiste_tests_6463.rs"]
mod identite_artiste_tests_6463;
