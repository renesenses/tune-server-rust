use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use serde::Deserialize;
use serde_json::{Value, json};
use tune_http_types::panne_sql::OuDefautJournalise;

use crate::error::AppError;
use crate::routes::active_profile::ActiveProfile;
use crate::state::AppState;
use tune_core::db::album_distinct_repo::{AlbumDistinctRepo, DistinctPairSet};
use tune_core::db::album_repo::{AlbumRepo, DrRange};
use tune_core::db::artist_repo::ArtistRepo;
use tune_core::db::backend::ToSqlValue;
use tune_core::db::engine::{Engine, PostgresDialect, SqlDialect, SqliteDialect};
use tune_core::db::history_repo::HistoryRepo;
use tune_core::db::models::Album;
use tune_core::db::profile_repo::ProfileRepo;
use tune_core::db::rating_repo::RatingRepo;
use tune_core::db::track_metadata_repo::TrackMetadataRepo;
use tune_core::db::track_repo::{TrackRepo, dedup_display_tracks};

use super::{Pagination, refus};

#[derive(Deserialize)]
pub(super) struct AlbumFilters {
    limit: Option<i64>,
    offset: Option<i64>,
    quality: Option<String>,
    format: Option<String>,
    sort: Option<String>,
    order: Option<String>,
    /// `?compilation=true` ne rend que les compilations, `false` que le reste,
    /// absent = tout (#1957).
    compilation: Option<bool>,
    /// `?include_hidden=true` rend AUSSI les albums masqués (#1391). Absent
    /// ou `false` : ils sont exclus — un client qui ignore le paramètre voit
    /// simplement l'album disparaître, sans rien changer chez lui.
    include_hidden: Option<bool>,
    /// Tranche de Dynamic Range, bornes INCLUSES (#2144). Les deux sont
    /// facultatives et indépendantes : `?dr_min=14` = « DR14 et au-dessus »,
    /// `?dr_max=7` = « DR7 et en dessous », les deux = une tranche fermée.
    /// Aucune des deux = aucun filtre, réponse identique à avant.
    ///
    /// Le serveur ne connaît PAS de tranches nommées : l'issue n'en fixe
    /// aucune (voir `DrRange`). `GET /library/albums/filters` rend les valeurs
    /// réellement présentes, à charge du client de dessiner ses pastilles.
    dr_min: Option<i64>,
    dr_max: Option<i64>,
    /// Graine du tri aleatoire (#3074). N'a de sens qu'avec `sort=random`.
    ///
    /// Absente, le serveur en tire une et la RENVOIE dans la reponse
    /// (`"seed"`) : le client doit la repasser sur les pages suivantes, sinon
    /// chaque `offset` re-tire et la grille montre des albums en double tout
    /// en en cachant d'autres — la vue Bibliotheque charge ses albums en
    /// quatre requetes (offset 0/100, puis 0, 2000, 4000).
    ///
    /// Le « bouton de re-tirage » demande au fil 1635 n'est donc rien d'autre
    /// que « redemander la page 0 sans graine », ou avec une autre.
    seed: Option<i64>,
}

#[derive(Deserialize)]
pub(super) struct QuickFavQuery {
    profile_id: Option<i64>,
}

#[derive(Deserialize)]
pub(super) struct RateRequest {
    rating: i32,
    note: Option<String>,
    profile_id: Option<i64>,
}

#[derive(Deserialize)]
pub(super) struct RatingQuery {
    profile_id: Option<i64>,
}

#[derive(Deserialize)]
pub(super) struct LangQuery {
    lang: Option<String>,
}

pub(super) async fn list_albums(
    State(state): State<AppState>,
    Query(p): Query<AlbumFilters>,
) -> Json<Value> {
    let repo = AlbumRepo::with_backend(state.backend.clone());
    let limit = p.limit.unwrap_or(50);
    let offset = p.offset.unwrap_or(0);
    let sort = p.sort.as_deref().unwrap_or("added_at");
    let order = p.order.as_deref().unwrap_or("asc");
    let include_hidden = p.include_hidden.unwrap_or(false);
    let dr = DrRange::new(p.dr_min, p.dr_max);
    // Le total suit la même exclusion que la liste, sinon la grille pagine
    // faux (#1391) — et la même TRANCHE de DR, sinon elle pagine encore plus
    // faux (#2144) : le tag DR n'existe que sur une poignée d'albums, un
    // `total` de 45 000 sur une liste de douze donnerait des centaines de
    // pages vides.
    let total = match dr {
        Some(range) => repo.count_in_dr_range(range, include_hidden).unwrap_or(0),
        None if include_hidden => repo.count().unwrap_or(0),
        None => repo.count_visible().unwrap_or(0),
    };
    // #3074 — `sort=random` : la graine EST le contrat. Le serveur en tire une
    // quand le client n'en donne pas, et la lui rend toujours, pour que les
    // pages suivantes retombent sur le meme tirage. Hors tri aleatoire elle
    // reste `None` et la reponse ne bouge pas d'un octet pour les clients deja
    // livres (iOS, macOS, Android, client web, UPnP).
    let seed = (sort == "random").then(|| p.seed.unwrap_or_else(AlbumRepo::graine_aleatoire));
    let items = match repo.list_filtered_seeded(
        limit,
        offset,
        sort,
        order,
        p.format.as_deref(),
        p.quality.as_deref(),
        p.compilation,
        include_hidden,
        dr,
        seed,
    ) {
        Ok(albums) => albums,
        Err(e) => {
            tracing::error!(
                error = %e,
                sort,
                order,
                limit,
                offset,
                total,
                "list_albums_query_failed — stats show {total} albums but query returned error"
            );
            Vec::new()
        }
    };
    let items: Vec<Value> = items
        .iter()
        .map(|a| {
            let mut j = a.to_json();
            if let Some(obj) = j.as_object_mut() {
                obj.remove("bio");
            }
            j
        })
        .collect();
    let mut corps = json!({"items": items, "total": total, "limit": limit, "offset": offset});
    if let Some(graine) = seed {
        // Ajoutee SEULEMENT en tri aleatoire : un client qui ignore #3074 lit
        // exactement la reponse d'avant.
        corps["seed"] = json!(graine);
    }
    Json(corps)
}

pub(super) async fn album_count(State(state): State<AppState>) -> Json<Value> {
    let count = AlbumRepo::with_backend(state.backend.clone())
        .count()
        .unwrap_or(0);
    Json(json!({ "count": count }))
}

/// `POST /library/albums/{id}/hide` — masque l'album (#1391).
///
/// Masquer n'est PAS supprimer : les fichiers restent intacts, `GET
/// /albums/{id}`, ses pistes et la lecture restent opérants (files d'attente
/// et playlists continuent de jouer) ; l'album sort des vues de découverte
/// (grilles, pistes, recherche, facettes). Réversible par DELETE. Idempotent.
pub(super) async fn hide_album(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<Value>, AppError> {
    let repo = tune_core::db::hidden_repo::HiddenRepo::with_backend(state.backend.clone());
    match repo.hide_album(id) {
        Ok(true) => Ok(Json(json!({"album_id": id, "hidden": true}))),
        Ok(false) => Err(AppError::not_found(format!("album {id} not found"))),
        Err(e) => Err(AppError::internal(e)),
    }
}

/// `DELETE /library/albums/{id}/hide` — démasque. Idempotent : démasquer un
/// album non masqué rend simplement `hidden: false`.
pub(super) async fn unhide_album(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<Value>, AppError> {
    let repo = tune_core::db::hidden_repo::HiddenRepo::with_backend(state.backend.clone());
    match repo.unhide_album(id) {
        Ok(_) => Ok(Json(json!({"album_id": id, "hidden": false}))),
        Err(e) => Err(AppError::internal(e)),
    }
}

/// `GET /library/albums/hidden` — la liste de révision : tout ce qui est
/// masqué, y compris les marqueurs momentanément orphelins (racine démontée),
/// rendus avec l'instantané d'identité pour rester démasquables.
pub(super) async fn list_hidden_albums(
    State(state): State<AppState>,
) -> Result<Json<Value>, AppError> {
    let repo = tune_core::db::hidden_repo::HiddenRepo::with_backend(state.backend.clone());
    let items = repo.list_hidden_albums().map_err(AppError::internal)?;
    Ok(Json(json!({"total": items.len(), "items": items})))
}

#[derive(Deserialize)]
pub(super) struct CreateAlbumRequest {
    title: String,
    artist_id: Option<i64>,
}

/// Create an album by title (used by MetadataView when assigning tracks to a
/// new album name). Reuses an existing album with the same title if one exists.
pub(super) async fn create_album(
    State(state): State<AppState>,
    Json(body): Json<CreateAlbumRequest>,
) -> Result<impl IntoResponse, AppError> {
    let title = body.title.trim();
    if title.is_empty() {
        return Err(AppError::bad_request("title is required"));
    }
    let repo = AlbumRepo::with_backend(state.backend.clone());
    if let Ok(Some(existing)) = repo.get_by_title(title) {
        return Ok(Json(json!({ "id": existing.id, "title": existing.title })));
    }
    let album = Album {
        id: None,
        title: title.to_string(),
        artist_id: body.artist_id,
        artist_name: None,
        year: None,
        original_year: None,
        genre: None,
        genres: None,
        disc_count: None,
        track_count: None,
        cover_path: None,
        source: "local".to_string(),
        source_id: None,
        label: None,
        catalog_number: None,
        barcode: None,
        format: None,
        sample_rate: None,
        bit_depth: None,
        bio: None,
        musicbrainz_release_id: None,
        musicbrainz_release_group_id: None,
        release_date: None,
        original_date: None,
        added_at: None,
        // Un album créé à la main n'est pas une compilation : c'est le scan
        // qui lève ce drapeau, d'après les tags (#1957).
        is_compilation: false,
    };
    let id = repo
        .create(&album)
        .map_err(|e| AppError::internal(e.to_string()))?;
    Ok(Json(json!({ "id": id, "title": title })))
}

pub(super) async fn album_filters(State(state): State<AppState>) -> Result<Json<Value>, AppError> {
    // `LOWER(TRIM(...))`, sinon deux valeurs qui ne diffèrent que par la casse
    // font deux entrées — et l'écran les rend IDENTIQUES, puisqu'il met tout en
    // majuscules à l'affichage (`LibraryView.svelte`, `toUpperCase()`).
    //
    // C'est ainsi que « DSD » apparaissait deux fois dans les types de fichiers,
    // en deux lignes visuellement indiscernables : `dsd` et `DSD` en base
    // (Cyrille Moutia, #1612). Le chemin de scan actuel écrit bien en
    // minuscules, mais toute valeur venue d'ailleurs — une version antérieure,
    // un import — traverse sans être repliée.
    //
    // Le repli se fait ICI et pas dans `normalize_format` : le passthrough
    // sensible à la casse de cette fonction est délibéré et figé par un test
    // (`normalize_format_case_sensitivity` : « MPEG » ne doit pas devenir
    // « mp3 »). Le lever changerait le format écrit pour d'autres fichiers.
    //
    // `TRIM` en plus de `LOWER` : un espace de fin produit exactement le même
    // doublon invisible, pour la même raison.
    let formats: Vec<String> = state
        .backend
        .query_many(
            "SELECT DISTINCT LOWER(TRIM(format)) FROM albums \
             WHERE format IS NOT NULL AND TRIM(format) != '' \
             ORDER BY LOWER(TRIM(format))",
            &[],
        )
        .ou_defaut_journalise()
        .into_iter()
        .filter_map(|row| row.into_iter().next()?.as_string())
        .collect();
    let sample_rates: Vec<i64> = state
        .backend
        .query_many(
            "SELECT DISTINCT sample_rate FROM albums WHERE sample_rate IS NOT NULL ORDER BY sample_rate",
            &[],
        )
        .ou_defaut_journalise()
        .into_iter()
        .filter_map(|row| row.into_iter().next()?.as_i64())
        .collect();
    // Dynamic Range (#2144) : les valeurs RÉELLEMENT présentes, croissantes.
    // C'est la matière des tranches, et la mesure que l'issue réclamait — un
    // tableau vide dit qu'aucun album n'est tagué, et l'écran n'a alors aucune
    // facette DR à proposer plutôt qu'une facette qui ne rendrait rien.
    // La clé s'ajoute au JSON existant : un client qui l'ignore ne voit aucun
    // changement.
    let dynamic_ranges = AlbumRepo::with_backend(state.backend.clone())
        .dynamic_range_values()
        .unwrap_or_default();
    Ok(Json(
        json!({ "formats": formats, "sample_rates": sample_rates, "dynamic_ranges": dynamic_ranges }),
    ))
}

pub(super) async fn recent_albums(
    State(state): State<AppState>,
    Query(p): Query<Pagination>,
) -> Json<Value> {
    let limit = p.limit.unwrap_or(50);
    let repo = AlbumRepo::with_backend(state.backend.clone());
    let items = repo.list_recent(limit).unwrap_or_default();
    let items: Vec<Value> = items.iter().map(|a| a.to_json()).collect();
    Json(json!(items))
}

pub(super) async fn get_album(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let repo = AlbumRepo::with_backend(state.backend.clone());
    match repo.get(id) {
        Ok(Some(album)) => {
            let mut j = album.to_json();
            if let (Some(obj), Ok(Some(prov))) = (j.as_object_mut(), repo.bio_provenance(id)) {
                obj.insert("bio_provenance".into(), prov);
            }
            // Dynamic Range, when the files carry the tag (#303, #1418). Absent
            // from the payload rather than null when untagged, so a client can
            // simply test for the key instead of distinguishing "no tag" from
            // "measured zero" — DR0 is a real value.
            //
            // `dynamic_range_source` dit d'OÙ sort la valeur (#1388) :
            // `album_tag` quand une piste porte `ALBUM DYNAMIC RANGE`,
            // `track_average` quand Tune l'a déduite de la moyenne arrondie des
            // `dr_track` des pistes.
            //
            // ⚠️ `track_average` ne dit RIEN de la provenance de ces pistes :
            // depuis la v0.9.145 un `dr_track` peut venir du tag du fichier ou
            // du calcul de la passe d'analyse. Cette seconde question se lit
            // piste par piste, sur les listes de pistes, sous la clé de même
            // nom `dynamic_range_source` (#3924). Les deux clés apparaissent et
            // disparaissent ENSEMBLE : un client qui ne connaît que la première
            // ne voit aucun changement, celui qui lit la seconde peut annoncer
            // une mesure ou une déduction plutôt que de les confondre.
            if let (Some(obj), Ok(Some(dr))) = (j.as_object_mut(), repo.dynamic_range_detail(id)) {
                obj.insert("dynamic_range".into(), Value::String(dr.valeur.to_string()));
                obj.insert(
                    "dynamic_range_source".into(),
                    Value::String(dr.source().into()),
                );
            }
            Json(j).into_response()
        }
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

pub(super) async fn album_tracks(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Query(f): Query<AlbumFilters>,
) -> Json<Value> {
    let repo = TrackRepo::with_backend(state.backend.clone());
    // When the library grid has an active quality/format filter, the client
    // forwards it here so the album detail shows only the matching tracks
    // (Sergio: a Hi-Res/FLAC filter must not reveal the album's MP3/44.1
    // tracks). With no filter this is identical to list_by_album.
    //
    // `ou_defaut_journalise` et non `unwrap_or_default` (#2861) : une panne de
    // base rendait ici une liste VIDE, indiscernable d'un album sans piste, et
    // sans une ligne de journal. Le cas n'est pas theorique sur cette route —
    // le rapport de jfpaquet du 02/09 (0.9.130, PostgreSQL, fil 1642) porte
    // trois `panne_sql_avalee` de l'accueil dans la MEME fenetre de journal
    // (#3181) : sur cette installation, une requete qui echoue en silence est
    // l'ordinaire, pas l'exception. La reponse HTTP ne bouge pas.
    let items = dedup_display_tracks(
        repo.list_by_album_filtered(id, f.format.as_deref(), f.quality.as_deref())
            .ou_defaut_journalise(),
    );

    // GROUPING (#2130) : lu au scan et rangé dans `track_metadata`, il n'était
    // jusqu'ici ressorti par aucune route. Une seule requête indexée par album
    // suffit ; le tag étant absent de l'immense majorité des bibliothèques,
    // elle ne ramène le plus souvent aucune ligne et la clé `grouping` reste
    // absente du JSON — ce qui laisse le contrat inchangé pour les clients qui
    // ne la connaissent pas.
    let track_ids: Vec<i64> = items.iter().filter_map(|t| t.id).collect();
    let meta_repo = TrackMetadataRepo::with_backend(state.backend.clone());
    let grouping = meta_repo
        .get_key_for_tracks("grouping", &track_ids)
        .ou_defaut_journalise();

    // Dynamic Range par piste (#1388) : le tag `DYNAMIC RANGE` est lu au scan
    // (#1806, `track_metadata['dr_track']`) mais n'était ressorti par aucune
    // liste de pistes — seul l'agrégat album sortait sur la fiche (#1809).
    // Même clé de sortie `dynamic_range` que sur l'album, même contrat :
    // absente quand la piste n'a pas de tag, présente pour DR0 qui est une
    // vraie mesure (celle d'un master saturé).
    let dynamic_range = meta_repo
        .get_key_for_tracks("dr_track", &track_ids)
        .ou_defaut_journalise();

    // #3924 — et D'OU elle sort. Voir `provenance_du_dr`.
    let dr_source = provenance_du_dr(&meta_repo, &track_ids, &dynamic_range);
    let mut items = attach_track_tags(
        items,
        &[
            ("grouping", &grouping),
            ("dynamic_range", &dynamic_range),
            ("dynamic_range_source", &dr_source),
        ],
    );
    // #3518 — « # Plays » et « Last Played », deux colonnes de la maquette V1
    // (Levente, 07/09/2026) que la route ne portait pas. C'est LE SITE D'APPEL :
    // `plays_for_tracks` peut être parfaite dans `history_repo`, sans cette
    // ligne la route rend les mêmes 31 champs qu'avant et les colonnes restent
    // `indisponible` côté client.
    attacher_ecoutes(&state, &mut items);
    Json(json!(items))
}

/// La PROVENANCE du Dynamic Range par piste, appariée à sa valeur (#3924).
///
/// `track_metadata['dr_source']` dit laquelle des deux fabriques a produit le
/// `dr_track` de la ligne : `"tag"` quand le scan l'a lu dans le fichier,
/// `"analysis"` quand la passe d'analyse l'a calculé sur les échantillons
/// (`tune-core/src/audio/replaygain.rs`). Patatorz, fil 1683, pose exactement
/// cette question — « mesurés, calculés ou juste reportés » — et aucune route
/// ne portait la réponse : la clef existait en base et n'était servie nulle
/// part.
///
/// # Pourquoi un appariement, et pas la carte brute
///
/// La clef de sortie `dynamic_range_source` ne doit JAMAIS apparaître sans
/// `dynamic_range` : une provenance seule décrirait une valeur que la charge
/// utile ne porte pas, et l'écran afficherait « calculé par Tune » à côté
/// d'un champ vide. C'est le même contrat que la fiche d'album, dont les deux
/// clefs apparaissent et disparaissent ensemble (`album_detail`).
///
/// L'inverse reste permis et ne se comble PAS : un `dr_track` écrit avant que
/// `dr_source` existe (toute base antérieure à la v0.9.145) n'a pas de
/// provenance connue, et en inventer une — « tag », puisque c'était le seul
/// producteur d'alors — serait un affichage fabriqué sur une base qu'un
/// rattrapage a pu depuis recalculer. La valeur sort, la provenance se tait.
///
/// ⚠️ Fonction partagée, et non deux lectures écrites côte à côte : les deux
/// surfaces de pistes (`albums::album_tracks` et `tracks::joindre_dr_par_piste`)
/// servent le MÊME champ sous le MÊME nom, et c'est exactement l'argument que
/// porte déjà `attach_track_tags` juste en dessous.
pub(super) fn provenance_du_dr(
    repo: &TrackMetadataRepo,
    track_ids: &[i64],
    dr: &std::collections::HashMap<i64, String>,
) -> std::collections::HashMap<i64, String> {
    // `ou_defaut_journalise` et non `unwrap_or_default` (#2861) : une panne de
    // base rendrait ici une carte VIDE, indiscernable d'une bibliothèque sans
    // provenance connue, et sans une ligne de journal.
    let mut source = repo
        .get_key_for_tracks("dr_source", track_ids)
        .ou_defaut_journalise();
    source.retain(|track_id, _| dr.contains_key(track_id));
    source
}

/// Recopie des tags étendus (`track_metadata`) sur les pistes sérialisées
/// d'un album : GROUPING (#2130) et Dynamic Range (#1388).
///
/// Une clé n'est ajoutée que pour les pistes qui en portent réellement une
/// (`get_key_for_tracks` a déjà écarté les valeurs vides) : une piste sans
/// tag sort exactement comme avant, sans champ supplémentaire.
///
/// ⚠️ `pub(super)` et non privée : les AUTRES surfaces de pistes
/// (`super::tracks`) ressortent le MÊME champ sous le MÊME nom (#1388). Un
/// second recopieur écrit à côté aurait fini par diverger — l'écran aurait vu
/// `dynamic_range` sur les pistes d'un album et rien, ou autre chose, dans la
/// table des titres.
/// Recopie l'écoute par piste sur des pistes sérialisées : `play_count` et
/// `last_played_at` (#3518).
///
/// # Un contrat différent de celui de `attach_track_tags`
///
/// Les tags étendus sont ABSENTS quand la piste n'en porte pas. Ici les deux
/// clés sont TOUJOURS présentes quand la lecture a réussi : `play_count` vaut
/// `0` et `last_played_at` vaut `null` pour une piste jamais jouée. L'issue le
/// demande en toutes lettres — « ici un zéro est une information, pas une
/// absence » —, et un client ne peut pas afficher « jamais joué » sur une clé
/// qui manque : il ne saurait pas si la route ignore la colonne ou si la piste
/// n'a jamais tourné.
///
/// # Ce qui se passe quand la base échoue
///
/// Aucune des deux clés n'est posée. Pas `0` : un zéro dû à une panne se lit
/// « jamais jouée » et ment. « Une colonne qui se trompe est pire qu'une
/// colonne absente » est l'argument même du ticket ; il vaut aussi contre nous.
/// L'échec laisse une trace dans le journal, comme #2861 l'exige pour les
/// favoris.
///
/// Une seule requête pour toute la page, et aucune du tout sur une page vide.
pub(super) fn attacher_ecoutes(state: &AppState, items: &mut [Value]) {
    let ids: Vec<i64> = items
        .iter()
        .filter_map(|v| v.get("id").and_then(Value::as_i64))
        .collect();
    if ids.is_empty() {
        return;
    }
    let stats = match HistoryRepo::with_backend(state.backend.clone()).plays_for_tracks(&ids) {
        Ok(stats) => stats,
        Err(e) => {
            tracing::error!(error = %e, "ecoutes_par_piste_illisibles");
            return;
        }
    };
    for item in items.iter_mut() {
        let Some(id) = item.get("id").and_then(Value::as_i64) else {
            continue;
        };
        let Some(obj) = item.as_object_mut() else {
            continue;
        };
        let (compte, derniere) = stats.get(&id).cloned().unwrap_or((0, None));
        obj.insert("play_count".into(), json!(compte));
        obj.insert("last_played_at".into(), json!(derniere));
    }
}

pub(super) fn attach_track_tags(
    items: Vec<tune_core::db::models::Track>,
    tags: &[(&str, &std::collections::HashMap<i64, String>)],
) -> Vec<Value> {
    items
        .into_iter()
        .map(|t| {
            let track_id = t.id;
            // `Track::to_json` et non `serde_json::to_value` : le modèle
            // ajoute `channel_badge`, un champ CALCULÉ à partir de `channels`
            // (5.1, 7.1.4 Atmos…). `/library/search` le servait — elle
            // sérialisait par `to_json` — et les listes qui passent ici ne le
            // servaient pas : la même piste portait sa pastille sur un écran
            // et pas sur l'autre. Brancher la recherche sur ce seam (#3789)
            // sans cette ligne aurait RETIRÉ la pastille de la recherche, une
            // régression pour fermer un trou. La correction est additive : les
            // autres surfaces gagnent le champ qu'elles auraient dû porter.
            let mut v = t.to_json();
            if let (Some(track_id), Some(obj)) = (track_id, v.as_object_mut()) {
                for (key, map) in tags {
                    if let Some(val) = map.get(&track_id) {
                        obj.insert((*key).into(), json!(val));
                    }
                }
            }
            v
        })
        .collect()
}

pub(super) async fn quick_fav_album(
    State(state): State<AppState>,
    profile: ActiveProfile,
    Path(id): Path<i64>,
    Query(q): Query<QuickFavQuery>,
) -> Json<Value> {
    let profile_id = q.profile_id.unwrap_or_else(|| profile.id());
    let repo = ProfileRepo::with_backend(state.backend.clone());
    let is_fav = repo.is_favorite(profile_id, "album", id).unwrap_or(false);
    if is_fav {
        repo.remove_favorite(profile_id, "album", id).ok();
    } else {
        repo.add_favorite(profile_id, "album", id).ok();
    }
    Json(json!({"is_favorite": !is_fav, "album_id": id}))
}

pub(super) async fn rate_album(
    State(state): State<AppState>,
    profile: ActiveProfile,
    Path(id): Path<i64>,
    Json(body): Json<RateRequest>,
) -> impl IntoResponse {
    let repo = RatingRepo::with_backend(state.backend.clone());
    let profile_id = body.profile_id.unwrap_or_else(|| profile.id());
    match repo.rate_album(id, profile_id, body.rating, body.note.as_deref()) {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    }
}

pub(super) async fn get_album_rating(
    State(state): State<AppState>,
    profile: ActiveProfile,
    Path(id): Path<i64>,
    Query(q): Query<RatingQuery>,
) -> impl IntoResponse {
    let repo = RatingRepo::with_backend(state.backend.clone());
    let profile_id = q.profile_id.unwrap_or_else(|| profile.id());
    match repo.get_rating(id, profile_id) {
        Ok(Some(r)) => Json(json!(r)).into_response(),
        Ok(None) => Json(json!({ "rating": null, "album_id": id })).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

pub(super) async fn top_rated_albums(
    State(state): State<AppState>,
    Query(p): Query<Pagination>,
) -> Json<Value> {
    let limit = p.limit.unwrap_or(20);
    let repo = RatingRepo::with_backend(state.backend.clone());
    let album_repo = AlbumRepo::with_backend(state.backend.clone());
    let top = repo.top_rated(limit).unwrap_or_default();

    let items: Vec<Value> = top
        .iter()
        .filter_map(|(album_id, avg_rating, count)| {
            let album = album_repo.get(*album_id).ok()??;
            Some(json!({
                "album": album,
                "avg_rating": avg_rating,
                "rating_count": count,
            }))
        })
        .collect();

    Json(json!(items))
}

pub(super) async fn recommendations(
    State(state): State<AppState>,
    Query(p): Query<Pagination>,
) -> Json<Value> {
    // Return recently added albums the user hasn't listened to
    let limit = p.limit.unwrap_or(20);
    let repo = AlbumRepo::with_backend(state.backend.clone());
    let items = repo.list_recent(limit).unwrap_or_default();
    let items: Vec<Value> = items.iter().map(|a| a.to_json()).collect();
    Json(json!({ "albums": items }))
}

pub(super) async fn album_bio(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Query(q): Query<LangQuery>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let album_repo = AlbumRepo::with_backend(state.backend.clone());
    let album = match album_repo.get(id) {
        Ok(Some(a)) => a,
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    // Même précédence que la route artiste : `?lang=` explicite, puis
    // `Accept-Language`, puis `fr`. Cf. `super::artists::langue_demandee`.
    let lang = super::artists::langue_demandee(q.lang.as_deref(), &headers);
    let lang = lang.as_str();

    // Prefer a locally-enriched bio (with provenance/attribution) over the
    // community proxy.
    //
    // MAIS seulement si elle est dans la bonne langue (#1849, Dimitri). La
    // route artiste a recu ce garde-fou en #2126 ; la route album, que le
    // ticket cite pourtant dans sa portee, ne l'a jamais eu. Elle rendait donc
    // la bio stockee sans regarder ni sa langue ni celle qu'on demande, et le
    // `lang` ci-dessus ne servait qu'a nommer l'entree de cache -- il n'etait
    // meme JAMAIS atteint pour un album qui possedait deja une bio.
    //
    // L'enrichissement des bios d'album prend la langue de CELUI QUI LE LANCE
    // (`routes/system/enrich.rs`, `lang_from_header` puis
    // `batch_enrich_album_bios(.., &lang)`) : une bibliotheque enrichie depuis
    // une interface en francais stocke tout en francais, et le ressert ensuite
    // a tout le monde.
    let prov = album_repo.bio_provenance(id).ok().flatten();
    let bio_lang = prov
        .as_ref()
        .and_then(|p| p.get("lang").and_then(|v| v.as_str()));
    let stored_ok = super::artists::langue_convient(bio_lang, lang);

    if let Some(ref bio) = album.bio {
        if !bio.is_empty() && stored_ok {
            return Json(json!({
                "album": album.title,
                "bio": bio,
                "bio_provenance": prov,
            }))
            .into_response();
        }
    }
    // Community album-bio API is keyed by NAME (title + artist) and generated
    // on demand by the cloud — NO MusicBrainz id required, so it works for
    // every album. The old code proxied to the artist-MBID endpoint (wrong URL
    // too) and failed for the whole library, which has no MBIDs (#bios).
    let artist_name = album.artist_name.clone().or_else(|| {
        album.artist_id.and_then(|aid| {
            ArtistRepo::with_backend(state.backend.clone())
                .get(aid)
                .ok()
                .flatten()
                .map(|a| a.name)
        })
    });
    let artist_q = artist_name.as_deref().unwrap_or("");
    let cache_key = format!("cache:albumbio:{}:{artist_q}:{lang}", album.title);
    if let Some(cached) = super::api_cache_get(&state.backend, &cache_key) {
        return Json(cached).into_response();
    }
    match state
        .http_client
        .get("https://mozaiklabs.fr/api/v1/albums/bio")
        // `lang` est transmis au site, qui ne le recevait pas : il ne servait
        // qu'a nommer l'entree de cache. Meme reserve que pour la route
        // artiste (#2126) : l'effet depend de `site-mozaiklabs`. S'il ignore le
        // parametre, il rendra la meme langue qu'avant -- sans regression, mais
        // sans gain non plus.
        .query(&[
            ("title", album.title.as_str()),
            ("artist", artist_q),
            ("lang", lang),
        ])
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            let data: Value = resp.json().await.unwrap_or(json!({}));
            let out = json!({
                "album": album.title,
                "bio": data.get("bio").cloned().unwrap_or(Value::Null),
                "source": data.get("source").cloned().unwrap_or(Value::Null),
            });
            if out.get("bio").map(|b| !b.is_null()).unwrap_or(false) {
                super::api_cache_set(&state.backend, &cache_key, &out);
            }
            Json(out).into_response()
        }
        _ => repli_sur_la_bio_stockee(&album.title, album.bio.as_deref(), prov),
    }
}

/// Ressert la bio d'album stockee quand la langue demandee n'a rien donne.
///
/// « Une biographie en francais vaut mieux qu'un panneau vide » -- #1849 le dit,
/// et c'est juste : refuser une bio dans la mauvaise langue serait remplacer un
/// defaut par un pire. La provenance part avec elle, donc le client sait dans
/// quelle langue elle est et peut le signaler. Pendant du
/// `repli_sur_la_bio_stockee` de la route artiste, dont seule la cle JSON
/// (`album` au lieu de `artist`) differe.
fn repli_sur_la_bio_stockee(
    titre: &str,
    bio: Option<&str>,
    prov: Option<Value>,
) -> axum::response::Response {
    match bio.filter(|b| !b.is_empty()) {
        Some(b) => Json(json!({
            "album": titre,
            "bio": b,
            "bio_provenance": prov,
        }))
        .into_response(),
        None => Json(json!({"album": titre, "bio": null})).into_response(),
    }
}

pub(super) async fn album_similar(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let album_repo = AlbumRepo::with_backend(state.backend.clone());
    let album = match album_repo.get(id) {
        Ok(Some(a)) => a,
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    let mbid = if let Some(aid) = album.artist_id {
        let artist_repo = ArtistRepo::with_backend(state.backend.clone());
        artist_repo
            .get(aid)
            .ok()
            .flatten()
            .and_then(|a| a.musicbrainz_id)
    } else {
        None
    };
    let Some(mbid) = mbid else {
        return Json(json!({"album": album.title, "artists": []})).into_response();
    };
    match state
        .http_client
        .get(format!("https://mozaiklabs.fr/api/{mbid}/similar"))
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            let data: Value = resp.json().await.unwrap_or(json!({}));
            Json(data).into_response()
        }
        _ => Json(json!({"mbid": mbid, "artists": []})).into_response(),
    }
}

/// `POST /library/albums/{id}/distinct/{other_id}` — « ces deux albums ne sont
/// pas des doublons » (#1276).
///
/// L'arbitrage vaut pour les DEUX chemins qui rapprochent des albums :
/// `GET /library/albums/grouped` cesse de signaler la paire, et
/// `POST /library/albums/merge-duplicates` refuse de la fusionner. Il est
/// persisté par identité (titre + artiste des deux côtés) et réconcilié aux
/// mêmes ancrages que les masquages : il survit au rescan, au déplacement de
/// racine et à la mort/renaissance d'une ligne `albums`.
///
/// Idempotent, et insensible à l'ordre des deux ids.
pub(super) async fn declare_albums_distinct(
    State(state): State<AppState>,
    Path((id, other_id)): Path<(i64, i64)>,
) -> Result<Json<Value>, AppError> {
    let repo = AlbumDistinctRepo::with_backend(state.backend.clone());
    match repo.declarer_distincts(id, other_id) {
        Ok(true) => Ok(Json(
            json!({"album_a_id": id.min(other_id), "album_b_id": id.max(other_id), "distinct": true}),
        )),
        Ok(false) if id == other_id => Err(AppError::bad_request(
            "un album n'est pas un doublon de lui-même",
        )),
        Ok(false) => Err(AppError::not_found(format!(
            "album {id} ou {other_id} not found"
        ))),
        Err(e) => Err(AppError::internal(e)),
    }
}

/// `DELETE /library/albums/{id}/distinct/{other_id}` — révoque l'arbitrage :
/// la paire redevient candidate au rapprochement. Idempotent.
pub(super) async fn revoke_albums_distinct(
    State(state): State<AppState>,
    Path((id, other_id)): Path<(i64, i64)>,
) -> Result<Json<Value>, AppError> {
    let repo = AlbumDistinctRepo::with_backend(state.backend.clone());
    match repo.revoquer(id, other_id) {
        Ok(_) => Ok(Json(
            json!({"album_a_id": id.min(other_id), "album_b_id": id.max(other_id), "distinct": false}),
        )),
        Err(e) => Err(AppError::internal(e)),
    }
}

/// `GET /library/albums/distinct` — la liste de révision : toutes les paires
/// arbitrées, y compris celles momentanément orphelines (racine démontée),
/// rendues avec leur instantané d'identité pour rester révocables.
pub(super) async fn list_distinct_pairs(
    State(state): State<AppState>,
) -> Result<Json<Value>, AppError> {
    let repo = AlbumDistinctRepo::with_backend(state.backend.clone());
    let items = repo.lister().map_err(AppError::internal)?;
    Ok(Json(json!({"total": items.len(), "items": items})))
}

/// Les paires que l'utilisateur a déclarées distinctes (#1276), chargées en
/// UNE requête pour interrogation en mémoire.
///
/// Un échec de lecture rend l'ensemble VIDE, donc le comportement d'avant
/// #1276 : le rapprochement continue de fonctionner. Le sens inverse (tout
/// bloquer) transformerait une base en défaut en fonctionnalité muette.
fn paires_distinctes(state: &AppState) -> DistinctPairSet {
    AlbumDistinctRepo::with_backend(state.backend.clone())
        .charger_ensemble()
        .unwrap_or_else(|e| {
            tracing::warn!(error = %e, "album_distinct_pairs_load_failed");
            DistinctPairSet::default()
        })
}

pub(super) async fn merge_duplicate_albums_route(
    State(state): State<AppState>,
) -> Result<Json<Value>, AppError> {
    // Pick engine-specific aggregate and placeholder helpers.
    let (group_concat_expr, p1, p2) = match state.backend.engine() {
        Engine::Postgres => (
            PostgresDialect.group_concat(&PostgresDialect.placeholder(1), ","),
            PostgresDialect.placeholder(1),
            PostgresDialect.placeholder(2),
        ),
        Engine::Sqlite => (
            SqliteDialect.group_concat("id", ","),
            SqliteDialect.placeholder(1),
            SqliteDialect.placeholder(2),
        ),
    };

    // Case-insensitive grouping: LOWER(title) catches duplicates that differ
    // only by case (e.g. "The Dark Side of the Moon" vs "The Dark Side Of The Moon").
    let dupes_sql = format!(
        "SELECT LOWER(title), {group_concat_expr} FROM albums WHERE source = 'local' GROUP BY LOWER(title) HAVING COUNT(id) > 1"
    );
    let dupes: Vec<(String, String)> = state
        .backend
        .query_many(&dupes_sql, &[])
        .ou_defaut_journalise()
        .into_iter()
        .filter_map(|row| {
            let title = row.first()?.as_string()?;
            let ids = row.get(1)?.as_string()?;
            Some((title, ids))
        })
        .collect();

    // #1276 : l'utilisateur a pu déclarer que deux de ces albums sont des
    // releases DIFFÉRENTES. Une requête, un `HashSet` — le coût par candidat
    // reste nul, et aucun `LOWER` n'est ajouté à ce chemin (#2848 y a mesuré
    // ×4000 pour un `LOWER` non indexé).
    let distinctes = paires_distinctes(&state);

    let mut deleted = 0i64;
    let mut protegees = 0i64;
    for (_title, ids_str) in &dupes {
        let ids: Vec<i64> = ids_str.split(',').filter_map(|s| s.parse().ok()).collect();
        if ids.len() < 2 {
            continue;
        }
        let mut best_id = ids[0];
        let mut best_count = 0i64;
        let count_sql = format!("SELECT COUNT(id) FROM tracks WHERE album_id = {p1}");
        for &aid in &ids {
            let cnt: i64 = state
                .backend
                .query_one(&count_sql, &[&aid as &dyn ToSqlValue])
                .ok()
                .flatten()
                .and_then(|row| row.into_iter().next()?.as_i64())
                .unwrap_or(0);
            if cnt > best_count {
                best_count = cnt;
                best_id = aid;
            }
        }
        let update_sql = format!("UPDATE tracks SET album_id = {p1} WHERE album_id = {p2}");
        let delete_sql = format!("DELETE FROM albums WHERE id = {p1}");
        for &aid in &ids {
            if aid == best_id {
                continue;
            }
            // L'arbitrage de l'utilisateur prime sur le rapprochement par
            // titre : la fusion SUPPRIME la ligne perdante, elle ne se répare
            // pas. Un album protégé reste simplement à part (#1276).
            if distinctes.contains(best_id, aid) {
                protegees += 1;
                tracing::info!(
                    conserve = best_id,
                    protege = aid,
                    "album_merge_ignoree_paire_declaree_distincte"
                );
                continue;
            }
            state
                .backend
                .execute(
                    &update_sql,
                    &[&best_id as &dyn ToSqlValue, &aid as &dyn ToSqlValue],
                )
                .ok();
            state
                .backend
                .execute(&delete_sql, &[&aid as &dyn ToSqlValue])
                .ok();
            deleted += 1;
        }
    }
    state
        .backend
        .execute_batch(&format!(
            "UPDATE albums SET track_count = {}",
            tune_core::db::track_repo::sql_compte_pistes_visibles("albums.id")
        ))
        .ok();
    Ok(Json(json!({ "merged": deleted, "protected": protegees })))
}

const VARIANT_PATTERNS: &[&str] = &[
    "deluxe",
    "remastered",
    "remaster",
    "anniversary",
    "expanded",
    "special edition",
    "collector",
    "bonus track",
    "super deluxe",
    "legacy edition",
    "platinum edition",
];

fn strip_variant_suffix(title: &str) -> String {
    let lower = title.to_lowercase();
    for pat in VARIANT_PATTERNS {
        if let Some(pos) = lower.find(pat) {
            let prefix = title[..pos]
                .trim_end_matches(|c: char| c == '(' || c == '[' || c == '-' || c == ' ');
            if !prefix.is_empty() {
                return prefix.to_string();
            }
        }
    }
    title.to_string()
}

/// Écarte du groupe les variantes que l'utilisateur a déclarées distinctes de
/// l'original (#1276), et rend `None` si le groupe n'a plus de variante — il
/// ne signale alors plus rien.
///
/// La comparaison se fait contre l'ORIGINAL du groupe, celui que l'écran
/// propose de garder. Un groupe de trois où une seule paire est arbitrée garde
/// donc les deux autres membres rapprochés : on retire la paire nommée, on
/// n'invente pas de nouveau rapprochement.
fn variantes_retenues<'a, I>(
    original: &Album,
    variantes: I,
    distinctes: &DistinctPairSet,
) -> Option<Vec<&'a Album>>
where
    I: IntoIterator<Item = &'a Album>,
{
    let retenues: Vec<&Album> = match original.id {
        Some(oid) if !distinctes.is_empty() => variantes
            .into_iter()
            .filter(|a| a.id.is_none_or(|vid| !distinctes.contains(oid, vid)))
            .collect(),
        _ => variantes.into_iter().collect(),
    };
    if retenues.is_empty() {
        None
    } else {
        Some(retenues)
    }
}

pub(super) async fn albums_grouped(State(state): State<AppState>) -> Result<Json<Value>, AppError> {
    let repo = AlbumRepo::with_backend(state.backend.clone());

    // #1276 : les paires que l'utilisateur a déclarées « pas des doublons ».
    // Chargées en UNE requête, interrogées en mémoire — aucun coût par
    // candidat, aucun `LOWER` ajouté au rapprochement.
    let distinctes = paires_distinctes(&state);

    // Group by MusicBrainz release group ID
    let mbid_groups = repo.list_release_groups().unwrap_or_default();

    let mut groups: Vec<Value> = mbid_groups
        .iter()
        .filter_map(|(gid, albums)| {
            let original = &albums[0];
            let variants = variantes_retenues(original, &albums[1..], &distinctes)?;
            Some(json!({
                "group_id": gid,
                "method": "musicbrainz",
                "original": original.to_json(),
                "variants": variants.iter().map(|a| a.to_json()).collect::<Vec<_>>(),
                "count": variants.len() + 1,
            }))
        })
        .collect();

    // Group by title similarity (regex) for albums without MBID
    let all_albums = repo.list(5000, 0).unwrap_or_default();
    let grouped_ids: std::collections::HashSet<i64> = mbid_groups
        .iter()
        .flat_map(|(_, albums)| albums.iter().filter_map(|a| a.id))
        .collect();

    let ungrouped: Vec<_> = all_albums
        .iter()
        .filter(|a| a.id.is_some() && !grouped_ids.contains(&a.id.unwrap()))
        .collect();

    let mut title_map: std::collections::HashMap<String, Vec<&tune_core::db::models::Album>> =
        std::collections::HashMap::new();
    for album in &ungrouped {
        let base = strip_variant_suffix(&album.title);
        title_map.entry(base).or_default().push(album);
    }

    for (base_title, albums) in &title_map {
        if albums.len() > 1 {
            // Même arbitrage que pour les groupes MusicBrainz (#1276) : une
            // variante déclarée distincte de l'original sort du groupe, et un
            // groupe vidé de ses variantes n'est plus signalé du tout.
            let Some(variants) =
                variantes_retenues(albums[0], albums[1..].iter().copied(), &distinctes)
            else {
                continue;
            };
            groups.push(json!({
                "group_id": base_title,
                "method": "title_similarity",
                "original": albums[0].to_json(),
                "variants": variants.iter().map(|a| a.to_json()).collect::<Vec<_>>(),
                "count": variants.len() + 1,
            }));
        }
    }

    Ok(Json(json!({
        "groups": groups,
        "total_groups": groups.len(),
    })))
}

// ── BIB-A2, phase 0 — albums éclatés présumés, en LECTURE SEULE ─────────────

/// Une piste vue par le rapprochement : de quel album elle est, où elle est.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PisteVue {
    pub(crate) album_id: i64,
    pub(crate) album_title: String,
    pub(crate) artist_name: String,
    pub(crate) year: Option<i64>,
    pub(crate) file_path: String,
    pub(crate) track_number: i64,
    /// `albums.cover_path` — un CONDENSAT, pas un chemin (#1444) : le SHA-256
    /// des octets de l'image pour toute écriture récente, l'ancien MD5 du
    /// chemin de la pochette pour les entrées héritées. Vide quand l'album n'a
    /// pas de pochette.
    pub(crate) cover_path: String,
}

/// Le dossier d'un chemin, séparateurs `/` et `\` confondus (bibliothèques
/// Windows et SMB), sans le nom de fichier.
pub(crate) fn dossier_de(chemin: &str) -> &str {
    match chemin.rfind(['/', '\\']) {
        Some(i) => &chemin[..i],
        None => "",
    }
}

/// Le titre d'album réduit à ce qui se compare : accents pliés, minuscules,
/// ponctuation en espaces, espaces réduits. Deux albums éclatés portent
/// presque toujours le MÊME titre, à la casse ou à un point près.
pub(crate) fn cle_titre(titre: &str) -> String {
    let plat = tune_core::db::engine::fold_diacritics(titre).to_lowercase();
    let mut cle = String::with_capacity(plat.len());
    let mut espace = false;
    for c in plat.chars() {
        if c.is_alphanumeric() {
            if espace && !cle.is_empty() {
                cle.push(' ');
            }
            espace = false;
            cle.push(c);
        } else {
            espace = true;
        }
    }
    cle
}

/// Le numéro de TRANCHE que porte un mot de titre : `cd2`, `disc 3`,
/// `disque 1`, `part 2`, `partie 1`.
///
/// Le vocabulaire des **disques** n'est pas redéfini ici : il vient de
/// [`tune_core::metadata::numero_de_disque`], la fonction que le scan emploie
/// déjà pour reconnaître un dossier de disque (#1656). Une seconde liste
/// aurait dérivé de la première au premier ajout.
///
/// Seuls `part` / `partie` sont ajoutés, et **uniquement ici** : un dossier
/// nommé « Part 1 » n'est pas un CD, et l'élargissement du vocabulaire des
/// dossiers changerait la déduction d'album au scan. Dans un TITRE, en
/// revanche, « (Part 1) » est bien une tranche — c'est l'un des quatre cas
/// que #3396 nomme.
///
/// `Vol. 2` en est volontairement absent, et cela CONTREDIT le corps de
/// l'issue, qui le cite parmi les suffixes à retirer. Le commentaire de
/// `numero_de_disque` tranche l'inverse, avec sa raison : « c'est presque
/// toujours un vrai titre d'album (« Greatest Hits Vol. 2 »), et le confondre
/// avec un disque effacerait un album entier ». Fusionner « Greatest Hits
/// Vol. 1 » et « Greatest Hits Vol. 2 » détruirait une distinction voulue —
/// exactement le dégât contre lequel l'issue met elle-même en garde deux
/// paragraphes plus bas.
fn numero_de_tranche(mot: &str) -> Option<u32> {
    if let Some(n) = tune_core::metadata::numero_de_disque(mot) {
        return Some(n);
    }
    let mot = mot.trim().to_lowercase();
    // « partie » avant « part », sinon « partie 2 » se lirait « part » + « ie 2 ».
    for prefixe in ["partie", "part"] {
        if let Some(reste) = mot.strip_prefix(prefixe) {
            return reste
                .trim_start_matches([' ', '-', '_', '.', '#'])
                .parse::<u32>()
                .ok()
                .filter(|&n| n > 0);
        }
    }
    None
}
/// [`cle_titre`], puis retrait d'un marqueur de TRANCHE en fin de titre.
///
/// ## Ce qui manquait (#3396)
///
/// `cle_titre` ne retire rien : `cle_titre("Sgt. Pepper — Disc 1")` rend
/// `"sgt pepper disc 1"` et `cle_titre("Sgt. Pepper — Disc 2")` rend
/// `"sgt pepper disc 2"`. Les deux clés diffèrent, donc
/// [`grouper_les_albums_eclates`] ne les met JAMAIS dans le même faisceau —
/// alors que c'est le cas d'école du ticket : « un « Disc 1 » / « Disc 2 »
/// séparés ». Le mécanisme BIB-A2 existait, la clé le rendait aveugle à la
/// moitié des éclatements qu'il devait voir.
///
/// ## Ce que le retrait NE fait pas
///
/// - il n'agit qu'en **fin** de titre : « CD1 Sessions » garde sa clé ;
/// - il exige qu'il RESTE quelque chose : un album qui ne s'appelle que
///   « CD 2 » garde sa clé entière, sinon tous les albums ainsi nommés
///   convergeraient sur la clé vide ;
/// - il ne fusionne rien par lui-même. Le faisceau reste soumis à la même
///   règle qu'avant — même dossier d'album, deux lignes `albums` — et
///   l'absorption reste un geste manuel, refusable et annulable par
///   `album_distinct_pairs`.
pub(crate) fn cle_titre_sans_tranche(titre: &str) -> String {
    let cle = cle_titre(titre);
    let mots: Vec<&str> = cle.split(' ').filter(|m| !m.is_empty()).collect();
    // « … disc 2 » : le marqueur et son nombre sont deux mots. Il doit rester
    // au moins un mot de titre devant.
    if mots.len() >= 3
        && numero_de_tranche(&format!(
            "{} {}",
            mots[mots.len() - 2],
            mots[mots.len() - 1]
        ))
        .is_some()
    {
        return mots[..mots.len() - 2].join(" ");
    }
    // « … cd2 » : marqueur et nombre collés, un seul mot.
    if mots.len() >= 2 && numero_de_tranche(mots[mots.len() - 1]).is_some() {
        return mots[..mots.len() - 1].join(" ");
    }
    cle
}
/// Le dossier qui porte l'ALBUM : [`dossier_de`], puis un cran de plus quand
/// le dernier segment n'est QU'un numéro de disque.
///
/// ## Ce qui manquait (#3396)
///
/// `.../Sgt. Pepper/CD1/01.flac` et `.../Sgt. Pepper/CD2/01.flac` donnaient
/// deux dossiers différents, donc deux faisceaux distincts, donc aucun
/// rapprochement — et l'absorption les refusait avec `409
/// dossiers_differents`. C'est le premier cas que #3396 cite : « un dossier
/// par CD ».
///
/// La reconnaissance du segment est celle du scan
/// ([`tune_core::metadata::numero_de_disque`], #1656) : `CD1`, `CD 2`,
/// `Disc-3`, `Disque 1`, `disk04` et rien d'autre. Un dossier `Bonus` ou
/// `Disco` ne remonte pas.
pub(crate) fn dossier_de_l_album(chemin: &str) -> &str {
    let dossier = dossier_de(chemin);
    let dernier = match dossier.rfind(['/', '\\']) {
        Some(i) => &dossier[i + 1..],
        None => dossier,
    };
    if tune_core::metadata::numero_de_disque(dernier).is_some() {
        dossier_de(dossier)
    } else {
        dossier
    }
}
/// Ce qui a rapproché les albums d'un faisceau. Valeur STABLE : le client
/// l'affiche et le serveur la relit dans la garde de la phase 1.
pub(crate) const INDICE_DOSSIER_ET_TITRE: &str = "dossier_et_titre";
/// Le second indice, celui que #3396 met en avant : deux albums dont la
/// pochette est la MÊME, à l'octet près.
pub(crate) const INDICE_POCHETTE_IDENTIQUE: &str = "pochette_identique";
/// Le condensat d'une pochette, quand c'en est un.
///
/// `albums.cover_path` porte un condensat hexadécimal (#1444) : SHA-256 des
/// octets de l'image pour toute écriture récente, MD5 du chemin de la pochette
/// pour les entrées héritées. Les deux régimes sont SÛRS pour un rapprochement,
/// et pour des raisons différentes :
///
/// - par le CONTENU, deux albums qui partagent le condensat partagent
///   littéralement les mêmes octets d'image ;
/// - par le CHEMIN, ils partagent le même fichier `cover.jpg` sur le disque.
///   Deux copies du même octet dans deux dossiers rendent alors deux condensats
///   différents : l'indice perd en sensibilité, jamais en sûreté.
///
/// Tout ce qui n'est pas un condensat (vide, `NULL`, chemin relatif d'une
/// version ancienne) est écarté : la valeur ne serait plus une identité
/// d'image, et c'est exactement là que naissent les rapprochements faux.
fn condensat_de_pochette(valeur: &str) -> Option<&str> {
    let valeur = valeur.trim();
    super::artwork::is_hex_hash(valeur).then_some(valeur)
}
/// Les numéros de piste des membres d'un faisceau ne se recouvrent-ils pas ?
///
/// C'est le garde-fou que #3396 nomme lui-même : « deux tranches d'un même
/// disque se complètent au lieu de se recouvrir. C'est ce qui distingue un
/// album en deux CD d'un album et de sa réédition, que rien d'autre ne
/// sépare. » Une réédition et son original portent tous deux les pistes 1..n :
/// ils se recouvrent dès le numéro 1.
///
/// Le numéro `0` — piste non numérotée — ne compte pas : il est présent par
/// défaut et ferait échouer la complémentarité de tout faisceau non étiqueté.
fn numeros_complementaires<'a>(
    numeros_par_album: impl Iterator<Item = &'a std::collections::BTreeSet<i64>>,
) -> bool {
    let mut vus: std::collections::BTreeSet<i64> = std::collections::BTreeSet::new();
    let mut complementaires = true;
    for numeros in numeros_par_album {
        if numeros.iter().any(|n| *n > 0 && !vus.insert(*n)) {
            complementaires = false;
        }
    }
    complementaires
}
/// Les membres forment-ils des TRANCHES, au sens fort ?
///
/// [`numeros_complementaires`] seul ne suffit pas à FONDER un rapprochement :
/// il est vrai par vacuité quand rien n'est numéroté. Deux albums dont toutes
/// les pistes portent le numéro `0` — une bibliothèque sans étiquettes — « se
/// complètent » sans qu'aucune pièce ne le dise, et ils partageraient volontiers
/// la pochette générique d'un label. C'est la famille de faux rapprochements
/// que le faisceau par pochette ouvrirait en premier.
///
/// Chaque membre doit donc apporter **au moins un numéro réel**, en plus de ne
/// recouvrir aucun de ses voisins.
fn tranches_complementaires<'a>(
    numeros_par_album: impl Iterator<Item = &'a std::collections::BTreeSet<i64>> + Clone,
) -> bool {
    numeros_par_album
        .clone()
        .all(|numeros| numeros.iter().any(|n| *n > 0))
        && numeros_complementaires(numeros_par_album)
}
/// La fiche d'un membre de faisceau : titre, artiste, année, numéros de piste.
type FicheDEclat = (String, String, Option<i64>, std::collections::BTreeSet<i64>);
/// Les membres d'un faisceau, rangés par identifiant d'album.
type MembresDuFaisceau = std::collections::BTreeMap<i64, FicheDEclat>;
/// Les membres d'un faisceau qui RESTENT après l'arbitrage de l'utilisateur.
///
/// ## Ce qui manquait (#3396)
///
/// Le ticket demande « de quoi **écarter** un groupe, pour qu'un faux positif
/// ne revienne pas indéfiniment ». Le geste existait déjà — `POST
/// /library/albums/{id}/distinct/{autre}` (#1276), que
/// [`super::albums::absorber_album`] relit pour refuser l'absorption —, mais
/// la phase 0 ne le relisait PAS : le faux positif écarté était reproposé à
/// chaque analyse, et l'écran n'offrait que de le réécarter. `albums_grouped`,
/// `merge_duplicate_albums_route` et `album_editions` consultent tous les trois
/// `album_distinct_pairs` ; `albums_eclates` était le seul rapprochement à ne
/// pas le faire.
///
/// ## La règle
///
/// Un membre n'est retenu que s'il n'est déclaré distinct d'AUCUN membre déjà
/// retenu. L'arbitrage porte sur une PAIRE : aucune paire arbitrée ne doit
/// survivre dans un groupe rendu, et un groupe de trois dont une seule paire
/// est arbitrée continue d'exister, amputé de ce seul membre.
///
/// C'est un filtre d'AFFICHAGE, et rien d'autre : aucune écriture, aucune
/// ligne `albums` ne disparaît, aucune identité n'est perdue. `DELETE
/// /library/albums/{id}/distinct/{autre}` révoque l'arbitrage et le groupe
/// revient tel quel.
fn membres_retenus<'a>(
    albums: &'a MembresDuFaisceau,
    distinctes: &DistinctPairSet,
) -> Vec<(i64, &'a FicheDEclat)> {
    if distinctes.is_empty() {
        return albums.iter().map(|(id, fiche)| (*id, fiche)).collect();
    }
    let mut gardes: Vec<(i64, &FicheDEclat)> = Vec::new();
    for (id, fiche) in albums {
        if gardes
            .iter()
            .any(|(deja, _)| distinctes.contains(*deja, *id))
        {
            continue;
        }
        gardes.push((*id, fiche));
    }
    gardes
}
/// BIB-A2, phase 0 : les groupes d'albums qui sont PROBABLEMENT un seul album
/// éclaté. Deux faisceaux, jamais mélangés :
///
/// 1. **`dossier_et_titre`** (mesure du 30/08 sur .18 : 93 % des éclatements
///    viennent de l'enregistreur, qui écrit sous l'artiste de la PISTE) :
///    même dossier ET même titre normalisé, sur au moins deux albums.
/// 2. **`pochette_identique`** (#3396) : même condensat de pochette, où que
///    soient les fichiers et quel que soit le titre — MAIS seulement si les
///    numéros de piste se complètent.
///
/// Chaque groupe dit ensuite si les numéros de piste sont complémentaires
/// (aucun numéro commun : la signature d'un album coupé en deux) et si les
/// années concordent. Rien n'est fusionné : le rapport nomme, l'absorption
/// reste un geste manuel.
///
/// ## Pourquoi la complémentarité est EXIGÉE du second faisceau, pas du premier
///
/// Le faisceau par dossier est déjà borné par le disque : deux lignes `albums`
/// dans un même répertoire sont presque toujours un éclatement, complémentaires
/// ou non (l'enregistreur redouble parfois un numéro). La pochette, elle, ne
/// borne rien — une réédition, un pressage japonais, un `Greatest Hits` et son
/// volume 2 partagent souvent la même image. Sans la complémentarité, ce
/// second faisceau proposerait de fusionner des albums que l'utilisateur a
/// délibérément séparés : exactement le dégât contre lequel le ticket met en
/// garde. Avec elle, il faut que les deux albums ne partagent AUCUN numéro de
/// piste — ce que deux éditions du même disque ne font jamais, puisqu'elles
/// commencent toutes deux à 1.
pub(crate) fn grouper_les_albums_eclates(
    pistes: &[PisteVue],
    distinctes: &DistinctPairSet,
) -> Vec<Value> {
    use std::collections::{BTreeMap, BTreeSet};
    type Membres = MembresDuFaisceau;
    let mut faisceaux: BTreeMap<(String, String), Membres> = BTreeMap::new();
    // Condensat de pochette → membres. Renseigné en même temps que le premier
    // faisceau : une seule traversée des pistes.
    let mut par_pochette: BTreeMap<String, Membres> = BTreeMap::new();
    let fiche_neuve = |p: &PisteVue| {
        (
            p.album_title.clone(),
            p.artist_name.clone(),
            p.year,
            BTreeSet::new(),
        )
    };
    for p in pistes {
        if let Some(condensat) = condensat_de_pochette(&p.cover_path) {
            par_pochette
                .entry(condensat.to_string())
                .or_default()
                .entry(p.album_id)
                .or_insert_with(|| fiche_neuve(p))
                .3
                .insert(p.track_number);
        }
        // Le dossier de l'ALBUM, pas celui du fichier : un coffret rangé en
        // `.../Album/CD1` et `.../Album/CD2` doit tomber dans un seul
        // faisceau (#3396).
        let dossier = dossier_de_l_album(&p.file_path);
        if dossier.is_empty() {
            continue;
        }
        // La clé sans marqueur de tranche : « Album Disc 1 » et
        // « Album Disc 2 » sont le même album coupé en deux (#3396).
        let cle = cle_titre_sans_tranche(&p.album_title);
        if cle.is_empty() {
            continue;
        }
        faisceaux
            .entry((dossier.to_string(), cle))
            .or_default()
            .entry(p.album_id)
            .or_insert_with(|| fiche_neuve(p))
            .3
            .insert(p.track_number);
    }
    let fiches_json = |membres: &[(i64, &FicheDEclat)]| {
        membres
            .iter()
            .map(|(id, (titre, artiste, annee, numeros))| {
                json!({
                    "id": id,
                    "title": titre,
                    "artist": artiste,
                    "year": annee,
                    "track_count": numeros.len(),
                    "track_numbers": numeros,
                })
            })
            .collect::<Vec<_>>()
    };
    let mut groupes = Vec::new();
    // Les groupes déjà nommés par le dossier : le faisceau par pochette ne les
    // redit pas. Un même éclatement proposé deux fois ferait croire à deux
    // gestes à faire.
    let mut deja_vus: BTreeSet<Vec<i64>> = BTreeSet::new();
    for ((dossier, cle), albums) in &faisceaux {
        // #3396 : les paires que l'utilisateur a déclarées distinctes sortent
        // du faisceau AVANT tout calcul — la complémentarité, le total et
        // l'année ne doivent décrire que ce qui est rendu.
        let membres = membres_retenus(albums, distinctes);
        if membres.len() < 2 {
            continue;
        }
        let complementaires = numeros_complementaires(membres.iter().map(|(_, a)| &a.3));
        let annees: BTreeSet<Option<i64>> = membres.iter().map(|(_, a)| a.2).collect();
        let total: usize = membres.iter().map(|(_, a)| a.3.len()).sum();
        deja_vus.insert(membres.iter().map(|(id, _)| *id).collect());
        groupes.push(json!({
            "indice": INDICE_DOSSIER_ET_TITRE,
            "dossier": dossier,
            "titre_normalise": cle,
            "numeros_complementaires": complementaires,
            "meme_annee": annees.len() == 1,
            "pistes": total,
            "albums": fiches_json(&membres),
        }));
    }
    for (condensat, albums) in &par_pochette {
        // Même retrait qu'au-dessus, et pour la même raison — mais ici il doit
        // précéder `tranches_complementaires` : un faisceau amputé d'un membre
        // n'est plus le même faisceau, et le garde-fou doit porter sur ce qui
        // sera rendu.
        let membres = membres_retenus(albums, distinctes);
        if membres.len() < 2 {
            continue;
        }
        if !tranches_complementaires(membres.iter().map(|(_, a)| &a.3)) {
            continue;
        }
        let ids: Vec<i64> = membres.iter().map(|(id, _)| *id).collect();
        if !deja_vus.insert(ids) {
            continue;
        }
        let annees: BTreeSet<Option<i64>> = membres.iter().map(|(_, a)| a.2).collect();
        let total: usize = membres.iter().map(|(_, a)| a.3.len()).sum();
        groupes.push(json!({
            "indice": INDICE_POCHETTE_IDENTIQUE,
            "pochette": condensat,
            // Ces deux champs gardent la forme du rapport, mais un faisceau par
            // pochette traverse les dossiers et les titres : les renseigner
            // avec la valeur d'un seul membre serait faux.
            "dossier": "",
            "titre_normalise": "",
            "numeros_complementaires": true,
            "meme_annee": annees.len() == 1,
            "pistes": total,
            "albums": fiches_json(&membres),
        }));
    }
    // Les plus sûrs d'abord : complémentaires et de même année, puis les plus gros.
    groupes.sort_by(|a, b| {
        let sa = (
            a["numeros_complementaires"].as_bool().unwrap_or(false),
            a["meme_annee"].as_bool().unwrap_or(false),
            a["pistes"].as_u64().unwrap_or(0),
        );
        let sb = (
            b["numeros_complementaires"].as_bool().unwrap_or(false),
            b["meme_annee"].as_bool().unwrap_or(false),
            b["pistes"].as_u64().unwrap_or(0),
        );
        sb.cmp(&sa)
    });
    groupes
}

/// `GET /library/albums/eclates` — les albums éclatés présumés (BIB-A2, phase 0).
/// Lecture seule ; la bibliothèque entière est lue une fois (une requête).
pub(super) async fn albums_eclates(State(state): State<AppState>) -> Result<Json<Value>, AppError> {
    let sql = "SELECT t.album_id, al.title, ar.name, al.year, t.file_path, t.track_number, \
                      COALESCE(al.cover_path, '') \
               FROM tracks t \
               JOIN albums al ON al.id = t.album_id \
               LEFT JOIN artists ar ON ar.id = al.artist_id \
               WHERE t.file_path IS NOT NULL AND t.file_path <> ''";
    let pistes: Vec<PisteVue> = state
        .backend
        .query_many(sql, &[])
        .ou_defaut_journalise()
        .into_iter()
        .filter_map(|r| {
            Some(PisteVue {
                album_id: r.first().and_then(|v| v.as_i64())?,
                album_title: r.get(1).and_then(|v| v.as_string()).unwrap_or_default(),
                artist_name: r.get(2).and_then(|v| v.as_string()).unwrap_or_default(),
                year: r.get(3).and_then(|v| v.as_i64()),
                file_path: r.get(4).and_then(|v| v.as_string())?,
                track_number: r.get(5).and_then(|v| v.as_i64()).unwrap_or(0),
                cover_path: r.get(6).and_then(|v| v.as_string()).unwrap_or_default(),
            })
        })
        .collect();
    // #3396 : l'arbitrage de l'utilisateur (#1276) est lu ICI, comme
    // `albums_grouped` et `album_editions` le font déjà. Une requête, un
    // ensemble en mémoire : le coût par faisceau reste nul.
    let groupes = grouper_les_albums_eclates(&pistes, &paires_distinctes(&state));
    Ok(Json(json!({
        "count": groupes.len(),
        "albums_concernes": groupes.iter().map(|g| g["albums"].as_array().map(|a| a.len()).unwrap_or(0)).sum::<usize>(),
        "groups": groupes,
    })))
}

/// Les dossiers (au sens `dossier_de`) où vivent les pistes d'un album.
fn dossiers_de_l_album(state: &AppState, album_id: i64) -> std::collections::BTreeSet<String> {
    let p1 = match state.backend.engine() {
        Engine::Postgres => PostgresDialect.placeholder(1),
        Engine::Sqlite => SqliteDialect.placeholder(1),
    };
    state
        .backend
        .query_many(
            &format!(
                "SELECT file_path FROM tracks WHERE album_id = {p1} AND file_path IS NOT NULL"
            ),
            &[&album_id as &dyn ToSqlValue],
        )
        .ou_defaut_journalise()
        .into_iter()
        .filter_map(|r| r.first().and_then(|v| v.as_string()))
        // Même dossier d'album qu'en phase 0 : `.../Album/CD1` et
        // `.../Album/CD2` ont bien `.../Album` en commun (#3396).
        .map(|chemin| dossier_de_l_album(&chemin).to_string())
        .filter(|d| !d.is_empty())
        .collect()
}
/// Les numéros de piste d'un album, pour la garde de complémentarité du
/// faisceau « pochette identique » (#3396).
fn numeros_de_piste_de_l_album(state: &AppState, album_id: i64) -> std::collections::BTreeSet<i64> {
    let p1 = match state.backend.engine() {
        Engine::Postgres => PostgresDialect.placeholder(1),
        Engine::Sqlite => SqliteDialect.placeholder(1),
    };
    state
        .backend
        .query_many(
            &format!("SELECT track_number FROM tracks WHERE album_id = {p1}"),
            &[&album_id as &dyn ToSqlValue],
        )
        .ou_defaut_journalise()
        .into_iter()
        .filter_map(|r| r.first().and_then(|v| v.as_i64()))
        .collect()
}

/// `POST /library/albums/{cible}/absorber/{doublon}` — BIB-A2, phase 1.
///
/// La phase 0 (`GET /library/albums/eclates`) nomme les faisceaux : même
/// dossier, même titre normalisé, deux lignes `albums`. Ici l'utilisateur
/// tranche, un couple à la fois, et le serveur vérifie avant d'agir la règle
/// exacte de la phase 0 : les deux albums ont des pistes dans un **même
/// dossier** (`dossier_de`), leurs titres ont la **même clé** (`cle_titre`),
/// tous deux sont `local`, et la paire n'a pas été déclarée distincte
/// (`album_distinct_pairs`). Rien n'est automatique : ni scan, ni migration,
/// ni tâche de fond ne passent par ici.
///
/// Réponses : `200` avec le bilan de [`AlbumRepo::absorber`] (pistes,
/// marqueurs, collections, champs repris) ; `404` album inconnu ; `400` même
/// album des deux côtés ; `409 dossiers_differents`, `409 titres_differents`,
/// `409 source_non_locale`, `409 paire_declaree_distincte`.
pub(super) async fn absorber_album(
    State(state): State<AppState>,
    Path((cible, doublon)): Path<(i64, i64)>,
) -> axum::response::Response {
    if cible == doublon {
        return refus(
            StatusCode::BAD_REQUEST,
            "meme_album",
            "un album ne s'absorbe pas lui-même".to_string(),
        );
    }
    let repo = AlbumRepo::with_backend(state.backend.clone());
    let (Ok(Some(album_cible)), Ok(Some(album_doublon))) = (repo.get(cible), repo.get(doublon))
    else {
        return refus(
            StatusCode::NOT_FOUND,
            "album_inconnu",
            format!("albums {cible} / {doublon}"),
        );
    };
    let non_locales = {
        let (p1, p2) = match state.backend.engine() {
            Engine::Postgres => (
                PostgresDialect.placeholder(1),
                PostgresDialect.placeholder(2),
            ),
            Engine::Sqlite => (SqliteDialect.placeholder(1), SqliteDialect.placeholder(2)),
        };
        state
            .backend
            .query_one(
                &format!(
                    "SELECT COUNT(*) FROM albums WHERE id IN ({p1}, {p2}) AND COALESCE(source, 'local') <> 'local'"
                ),
                &[&cible as &dyn ToSqlValue, &doublon],
            )
            .ok()
            .flatten()
            .and_then(|r| r.first().and_then(|v| v.as_i64()))
            .unwrap_or(0)
    };
    if non_locales > 0 {
        return refus(
            StatusCode::CONFLICT,
            "source_non_locale",
            "seuls deux albums de la bibliothèque locale se regroupent".to_string(),
        );
    }
    // Le SECOND indice de la phase 0 (#3396) : même pochette à l'octet près et
    // numéros de piste complémentaires. Il traverse volontairement les dossiers
    // ET les titres — c'est tout son intérêt : une compilation éclatée par
    // artiste n'a ni l'un ni l'autre en commun. Il doit donc traverser aussi
    // les deux gardes qui les vérifient, sinon la phase 0 proposerait un geste
    // que la phase 1 décline, le défaut même que le retrait du marqueur de
    // tranche avait déjà corrigé une fois.
    //
    // Il ne DISPENSE de rien d'autre : source locale, paire non déclarée
    // distincte et albums différents restent exigés ci-dessus et ci-dessous.
    let meme_pochette = match (
        condensat_de_pochette(album_cible.cover_path.as_deref().unwrap_or_default()),
        condensat_de_pochette(album_doublon.cover_path.as_deref().unwrap_or_default()),
    ) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    };
    let par_la_pochette = meme_pochette
        && tranches_complementaires(
            [
                numeros_de_piste_de_l_album(&state, cible),
                numeros_de_piste_de_l_album(&state, doublon),
            ]
            .iter(),
        );
    // La MÊME clé que la phase 0, marqueur de tranche retiré. Sans cela, la
    // phase 0 proposerait « Album Disc 1 » + « Album Disc 2 » et la phase 1
    // les refuserait par `titres_differents` : une surface ouverte sur un
    // geste que le serveur décline (#3396).
    if !par_la_pochette
        && cle_titre_sans_tranche(&album_cible.title)
            != cle_titre_sans_tranche(&album_doublon.title)
    {
        return refus(
            StatusCode::CONFLICT,
            "titres_differents",
            format!(
                "« {} » et « {} » n'ont pas le même titre",
                album_cible.title, album_doublon.title
            ),
        );
    }
    let dossiers_cible = dossiers_de_l_album(&state, cible);
    let dossiers_doublon = dossiers_de_l_album(&state, doublon);
    if !par_la_pochette
        && dossiers_cible
            .intersection(&dossiers_doublon)
            .next()
            .is_none()
    {
        return refus(
            StatusCode::CONFLICT,
            "dossiers_differents",
            "les deux albums n'ont aucun dossier en commun : ce ne sont pas les éclats d'un même disque".to_string(),
        );
    }
    if paires_distinctes(&state).contains(cible, doublon) {
        return refus(
            StatusCode::CONFLICT,
            "paire_declaree_distincte",
            "ces deux albums ont été déclarés distincts : l'arbitrage prime".to_string(),
        );
    }
    match repo.absorber(cible, doublon) {
        Ok(rapport) => {
            state.event_bus.emit(
                tune_core::event_types::EventType::LibraryUpdated.as_str(),
                json!({ "source": "albums_absorber", "cible": cible, "doublon": doublon }),
            );
            (StatusCode::OK, Json(json!(rapport))).into_response()
        }
        Err(e) => refus(
            StatusCode::INTERNAL_SERVER_ERROR,
            "absorption_echouee",
            e.to_string(),
        ),
    }
}

/// BIB-B1 — les ÉDITIONS d'un album.
///
/// Les autres albums du MÊME artiste dont le titre est le même à un suffixe
/// d'édition près — « (Remastered 2009) », « [Super Deluxe] »,
/// « - 2009 Remaster » — rapprochés par la règle EXACTE des versions de
/// piste (#2372, [`crate::routes::versions::predicat_titres_equivalents`]) :
/// aucun rapprochement flou, mieux vaut ne rien proposer qu'un faux.
///
/// Pour un audiophile, deux qualités du même album ne sont PAS un doublon à
/// supprimer (140 groupes mesurés sur .18, 29 seulement à nombre de pistes
/// égal). La route les NOMME — édition, année, format, résolution, nombre de
/// pistes — et ne fusionne rien. Les paires que l'utilisateur a déclarées
/// distinctes (`/albums/{id}/distinct/{other_id}`) sont écartées.
pub(super) async fn album_editions(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<Value>, AppError> {
    let repo = AlbumRepo::with_backend(state.backend.clone());
    let album = repo
        .get(id)
        .ok()
        .flatten()
        .ok_or(AppError::not_found("album not found"))?;
    let Some(artist_id) = album.artist_id else {
        return Ok(Json(json!({
            "album_id": id,
            "album_title": album.title,
            "base_title": album.title.trim(),
            "edition": Value::Null,
            "editions": [],
            "count": 0,
            "reason": "album sans artiste",
        })));
    };
    let lignes = albums_au_titre_equivalent(&state, artist_id, id, &album.title);
    // La règle exacte de #2372 ne voit, depuis une édition, que sa base : deux
    // éditions sœurs ne se voient pas l'une l'autre. On établit donc la base
    // (le titre le plus court du groupe) et, si l'album interrogé n'est pas
    // la base, on relit le groupe depuis elle.
    let titres_directs: Vec<String> = std::iter::once(album.title.clone())
        .chain(
            lignes
                .iter()
                .filter_map(|r| r.get(1).and_then(|v| v.as_string())),
        )
        .collect();
    let base = titre_de_base(&titres_directs);
    let lignes = if base.to_lowercase() == album.title.trim().to_lowercase() {
        lignes
    } else {
        albums_au_titre_equivalent(&state, artist_id, id, &base)
    };
    let distincts = AlbumDistinctRepo::with_backend(state.backend.clone())
        .charger_ensemble()
        .ok();
    let retenues: Vec<&Vec<tune_core::db::backend::SqlValue>> = lignes
        .iter()
        .filter(|r| {
            let autre = r.first().and_then(|v| v.as_i64()).unwrap_or(0);
            !distincts.as_ref().is_some_and(|d| d.contains(id, autre))
        })
        .collect();
    let pistes_album = album.track_count.map(i64::from);
    let editions: Vec<Value> = retenues
        .iter()
        .map(|r| {
            let titre = r.get(1).and_then(|v| v.as_string()).unwrap_or_default();
            let pistes = r.get(4).and_then(|v| v.as_i64());
            let autre = r.first().and_then(|v| v.as_i64()).unwrap_or(0);
            json!({
                "id": r.first().and_then(|v| v.as_i64()).unwrap_or(0),
                "title": titre,
                "edition": etiquette_d_edition(&base, &titre),
                "year": r.get(2).and_then(|v| v.as_i64()),
                "original_year": r.get(3).and_then(|v| v.as_i64()),
                "track_count": pistes,
                "disc_count": r.get(5).and_then(|v| v.as_i64()),
                "format": r.get(6).and_then(|v| v.as_string()),
                "sample_rate": r.get(7).and_then(|v| v.as_i64()),
                "bit_depth": r.get(8).and_then(|v| v.as_i64()),
                "source": r.get(9).and_then(|v| v.as_string()),
                "cover_path": r.get(10).and_then(|v| v.as_string()),
                "musicbrainz_release_id": r.get(11).and_then(|v| v.as_string()),
                "same_track_count": pistes.is_some() && pistes == pistes_album,
                // BIB-B2 (phase C) : le CONTENU des pistes, quand les empreintes
                // existent des deux côtés — `null` tant qu'on ne sait pas.
                "same_content": match contenu_commun(&state, id, autre) {
                    (0, _) => Value::Null,
                    (comparees, memes) => json!(memes * 5 >= comparees * 4),
                },
                "content_compared": contenu_commun(&state, id, autre).0,
                "content_same": contenu_commun(&state, id, autre).1,
            })
        })
        .collect();
    Ok(Json(json!({
        "album_id": id,
        "album_title": album.title,
        "base_title": base,
        "edition": etiquette_d_edition(&base, &album.title),
        "count": editions.len(),
        "editions": editions,
    })))
}

/// Les albums d'un artiste au titre équivalent à `titre` (règle exacte de
/// #2372), l'album `id` exclu, les plus anciens d'abord.
/// BIB-B2 (phase C) : les empreintes de contenu des pistes d'un album, par
/// (disque, numéro). Base antérieure à la colonne : rien.
fn empreintes_de_l_album(
    state: &AppState,
    album_id: i64,
) -> std::collections::BTreeMap<(i64, i64), tune_core::audio::empreinte::Empreinte> {
    let p1 = match state.backend.engine() {
        Engine::Postgres => PostgresDialect.placeholder(1),
        Engine::Sqlite => SqliteDialect.placeholder(1),
    };
    let sql = format!(
        "SELECT COALESCE(disc_number, 1), COALESCE(track_number, 0), audio_fingerprint \
         FROM tracks WHERE album_id = {p1} AND audio_fingerprint IS NOT NULL"
    );
    match state
        .backend
        .query_many(&sql, &[&album_id as &dyn ToSqlValue])
    {
        Ok(rows) => rows
            .iter()
            .filter_map(|r| {
                let disque = r.first()?.as_i64()?;
                let numero = r.get(1)?.as_i64()?;
                let empreinte =
                    tune_core::audio::empreinte::Empreinte::deserialiser(&r.get(2)?.as_string()?)?;
                Some(((disque, numero), empreinte))
            })
            .collect(),
        Err(_) => Default::default(),
    }
}

/// Pistes comparables entre deux albums (même disque, même numéro, une
/// empreinte de chaque côté) et, parmi elles, celles de même contenu.
fn contenu_commun(state: &AppState, a: i64, b: i64) -> (usize, usize) {
    let ea = empreintes_de_l_album(state, a);
    if ea.is_empty() {
        return (0, 0);
    }
    let eb = empreintes_de_l_album(state, b);
    let mut comparees = 0usize;
    let mut memes = 0usize;
    for (cle, x) in &ea {
        if let Some(y) = eb.get(cle) {
            comparees += 1;
            if tune_core::audio::empreinte::meme_contenu(x, y) {
                memes += 1;
            }
        }
    }
    (comparees, memes)
}

fn albums_au_titre_equivalent(
    state: &AppState,
    artist_id: i64,
    id: i64,
    titre: &str,
) -> Vec<Vec<tune_core::db::backend::SqlValue>> {
    let engine = state.backend.engine();
    let m = |i: usize| crate::routes::versions::marqueur(engine, i);
    let sql = format!(
        "SELECT a.id, a.title, a.year, a.original_year, a.track_count, a.disc_count, \
                a.format, a.sample_rate, a.bit_depth, a.source, a.cover_path, a.musicbrainz_release_id \
         FROM albums a \
         WHERE a.artist_id = {} AND a.id != {} AND {} \
         ORDER BY a.year, a.id",
        m(1),
        m(2),
        crate::routes::versions::predicat_titres_equivalents("a.title", &m(3))
    );
    // SQLite numérote ses marqueurs par position (`?`), PostgreSQL par nom
    // (`$3`) : le titre est lié autant de fois que le prédicat le répète.
    let repetitions = match engine {
        Engine::Sqlite => sql.matches('?').count().saturating_sub(2).max(1),
        Engine::Postgres => 1,
    };
    let titre = titre.to_string();
    let mut params: Vec<&dyn ToSqlValue> = vec![&artist_id, &id];
    params.extend(std::iter::repeat_n(&titre as &dyn ToSqlValue, repetitions));
    state
        .backend
        .query_many(&sql, &params)
        .ou_defaut_journalise()
}

/// Le titre de base d'un groupe d'éditions : le plus court (en caractères),
/// le premier à égalité. Avec la règle exacte de #2372 c'est nécessairement
/// le préfixe commun des autres.
fn titre_de_base(titres: &[String]) -> String {
    titres
        .iter()
        .map(|t| t.trim())
        .min_by_key(|t| t.chars().count())
        .unwrap_or_default()
        .to_string()
}

/// Ce qui distingue une édition de son titre de base : le suffixe après le
/// délimiteur, sans sa parenthèse ou son crochet fermant. `None` pour le
/// titre de base lui-même ou un titre qui n'en descend pas.
fn etiquette_d_edition(base: &str, titre: &str) -> Option<String> {
    let base = base.trim();
    let titre = titre.trim();
    if base.is_empty() || titre.len() <= base.len() {
        return None;
    }
    let debut = titre.get(..base.len())?;
    if debut.to_lowercase() != base.to_lowercase() {
        return None;
    }
    let reste = &titre[base.len()..];
    let delimiteur = crate::routes::versions::DELIMITEURS_D_EDITION
        .iter()
        .find(|d| reste.starts_with(**d))?;
    let corps = reste[delimiteur.len()..].trim();
    let corps = match *delimiteur {
        " (" => corps.strip_suffix(')').unwrap_or(corps),
        " [" => corps.strip_suffix(']').unwrap_or(corps),
        _ => corps,
    }
    .trim();
    (!corps.is_empty()).then(|| corps.to_string())
}

pub(super) async fn album_completeness(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<Value>, AppError> {
    let repo = AlbumRepo::with_backend(state.backend.clone());
    let album = repo
        .get(id)
        .ok()
        .flatten()
        .ok_or(AppError::not_found("album not found"))?;

    let p1 = match state.backend.engine() {
        Engine::Postgres => PostgresDialect.placeholder(1),
        Engine::Sqlite => SqliteDialect.placeholder(1),
    };

    // Le MÊME compte que `albums.track_count` (#1362) : les deux membres de la
    // comparaison ci-dessous doivent parler de la même chose, sinon un album
    // portant deux copies d'un morceau se déclarerait complet parce que ses
    // lignes dépassent ses présentations.
    let actual_tracks: i64 = state
        .backend
        .query_one(
            &format!(
                "SELECT {}",
                tune_core::db::track_repo::sql_compte_pistes_visibles(&p1)
            ),
            &[&id as &dyn ToSqlValue],
        )
        .ok()
        .flatten()
        .and_then(|row| row.into_iter().next()?.as_i64())
        .unwrap_or(0);
    let expected_tracks = album.track_count.unwrap_or(0) as i64;

    // Check total_tracks from metadata tags
    let max_tag_total: i64 = state
        .backend
        .query_one(
            &format!("SELECT COALESCE(MAX(CAST(track_number AS INTEGER)), 0) FROM tracks WHERE album_id = {p1}"),
            &[&id as &dyn ToSqlValue],
        )
        .ok()
        .flatten()
        .and_then(|row| row.into_iter().next()?.as_i64())
        .unwrap_or(0);

    let expected = if expected_tracks > 0 {
        expected_tracks
    } else {
        max_tag_total
    };
    let complete = expected > 0 && actual_tracks >= expected;
    let missing = if expected > actual_tracks {
        expected - actual_tracks
    } else {
        0
    };

    Ok(Json(json!({
        "album_id": id,
        "album_title": album.title,
        "actual_tracks": actual_tracks,
        "expected_tracks": expected,
        "missing": missing,
        "complete": complete,
        "completeness_pct": if expected > 0 { (actual_tracks as f64 / expected as f64 * 100.0).round() } else { 100.0 },
    })))
}

// ---------------------------------------------------------------------------
// PUT /albums/{id} — update album metadata (mirrors POST /metadata/albums/{id}/edit)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub(super) struct AlbumUpdate {
    title: Option<String>,
    artist_id: Option<i64>,
    artist_name: Option<String>,
    genre: Option<String>,
    year: Option<i32>,
    label: Option<String>,
}

pub(super) async fn update_album(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<AlbumUpdate>,
) -> impl IntoResponse {
    let repo = AlbumRepo::with_backend(state.backend.clone());
    let mut album = match repo.get(id) {
        Ok(Some(a)) => a,
        _ => return StatusCode::NOT_FOUND.into_response(),
    };

    if let Some(ref v) = body.title {
        album.title = v.clone();
    }
    if let Some(ref v) = body.genre {
        album.genre = Some(v.clone());
    }
    if let Some(v) = body.year {
        album.year = Some(v);
    }
    if let Some(ref v) = body.label {
        album.label = Some(v.clone());
    }
    // artist_id takes priority; fall back to artist_name resolution
    if let Some(aid) = body.artist_id {
        album.artist_id = Some(aid);
        // Refresh artist_name for the JSON response
        let artist_repo = ArtistRepo::with_backend(state.backend.clone());
        album.artist_name = artist_repo.get(aid).ok().flatten().map(|a| a.name);
    } else if let Some(ref name) = body.artist_name {
        let artist_repo = ArtistRepo::with_backend(state.backend.clone());
        if let Ok(Some(artist)) = artist_repo.get_by_name(name) {
            album.artist_id = artist.id;
            album.artist_name = Some(artist.name);
        } else if let Ok(artist) = artist_repo.get_or_create(name, None, None) {
            album.artist_id = artist.id;
            album.artist_name = Some(artist.name);
        }
    }

    repo.update(&album).ok();

    Json(album.to_json()).into_response()
}

// --- Album extended metadata endpoints ---

/// GET /api/v1/library/albums/{id}/metadata
/// Returns all extended metadata key-value pairs for an album.
pub(super) async fn album_metadata_get(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    use tune_core::db::album_metadata_repo::AlbumMetadataRepo;

    let repo = AlbumMetadataRepo::with_backend(state.backend.clone());
    match repo.get_all(id) {
        Ok(meta) => Json(json!(meta)).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

/// PUT /api/v1/library/albums/{id}/metadata
/// Batch-sets album-level extended metadata from a JSON object body.
/// DB only — propagating album fields into each track's file tags stays the
/// job of POST /library/write-tags {album_id}, so a save is never O(tracks).
pub(super) async fn album_metadata_put(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    use tune_core::db::album_metadata_repo::AlbumMetadataRepo;

    let album_repo = AlbumRepo::with_backend(state.backend.clone());
    match album_repo.get(id) {
        Ok(Some(_)) => {}
        Ok(None) => return (StatusCode::NOT_FOUND, "album not found").into_response(),
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }

    let repo = AlbumMetadataRepo::with_backend(state.backend.clone());
    if let Err(e) = repo.set_batch(id, &body) {
        return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
    }

    Json(json!({"status": "ok", "fields": body.len()})).into_response()
}

#[derive(Deserialize)]
pub(super) struct BatchAlbumUpdate {
    album_ids: Vec<i64>,
    genre: Option<String>,
    year: Option<i32>,
    artist_id: Option<i64>,
    artist_name: Option<String>,
    label: Option<String>,
}

pub(super) async fn batch_update_albums(
    State(state): State<AppState>,
    Json(body): Json<BatchAlbumUpdate>,
) -> impl IntoResponse {
    let repo = AlbumRepo::with_backend(state.backend.clone());
    let artist_repo = ArtistRepo::with_backend(state.backend.clone());
    let mut updated = 0i64;

    let resolved_artist_id = if let Some(aid) = body.artist_id {
        Some(aid)
    } else if let Some(ref name) = body.artist_name {
        artist_repo
            .get_by_name(name)
            .ok()
            .flatten()
            .and_then(|a| a.id)
            .or_else(|| {
                artist_repo
                    .get_or_create(name, None, None)
                    .ok()
                    .and_then(|a| a.id)
            })
    } else {
        None
    };

    for &id in &body.album_ids {
        let mut album = match repo.get(id) {
            Ok(Some(a)) => a,
            _ => continue,
        };
        if let Some(ref g) = body.genre {
            album.genre = Some(g.clone());
        }
        if let Some(y) = body.year {
            album.year = Some(y);
        }
        if let Some(ref l) = body.label {
            album.label = Some(l.clone());
        }
        if let Some(aid) = resolved_artist_id {
            album.artist_id = Some(aid);
        }
        if repo.update(&album).is_ok() {
            updated += 1;
        }
    }

    Json(serde_json::json!({ "updated": updated, "total": body.album_ids.len() })).into_response()
}

#[cfg(test)]
mod tests_grouping {
    use super::attach_track_tags;
    use std::collections::HashMap;
    use tune_core::db::models::Track;

    fn track(id: i64, title: &str) -> Track {
        let mut t = Track::new(title.to_string());
        t.id = Some(id);
        t
    }

    /// Sans GROUPING en base, la réponse d'un album est celle d'avant #2130 :
    /// aucune clé supplémentaire. C'est le cas mesuré sur les bibliothèques
    /// réelles, donc le cas qui doit rester gratuit.
    #[test]
    fn attach_grouping_leaves_tracks_untouched_when_absent() {
        let items = vec![track(1, "I. Allegro"), track(2, "II. Adagio")];
        let out = attach_track_tags(items, &[("grouping", &HashMap::new())]);
        assert_eq!(out.len(), 2);
        for v in &out {
            assert!(
                v.get("grouping").is_none(),
                "aucune clé grouping ne doit apparaître sans donnée"
            );
        }
        assert_eq!(out[0]["title"], "I. Allegro");
    }

    /// Une valeur en base ressort sur la piste concernée, et sur elle seule.
    #[test]
    fn attach_grouping_reports_the_value_on_the_right_track() {
        let items = vec![track(1, "I. Allegro"), track(2, "Bonus")];
        let mut map = HashMap::new();
        map.insert(2i64, "Titres bonus".to_string());
        let out = attach_track_tags(items, &[("grouping", &map)]);
        assert!(out[0].get("grouping").is_none());
        assert_eq!(out[1]["grouping"], "Titres bonus");
    }

    /// Une entrée pour une piste absente de l'album ne contamine personne.
    #[test]
    fn attach_grouping_ignores_unknown_track_ids() {
        let items = vec![track(1, "I. Allegro")];
        let mut map = HashMap::new();
        map.insert(99i64, "Autre album".to_string());
        let out = attach_track_tags(items, &[("grouping", &map)]);
        assert!(out[0].get("grouping").is_none());
    }

    /// DR par piste (#1388) : la valeur ressort sous `dynamic_range` sur la
    /// bonne piste, et une piste taguée DR0 la garde — c'est une vraie mesure,
    /// celle d'un master saturé, pas une absence.
    #[test]
    fn attach_dynamic_range_reports_value_and_keeps_dr0() {
        let items = vec![track(1, "Loud"), track(2, "Untagged"), track(3, "Quiet")];
        let mut dr = HashMap::new();
        dr.insert(1i64, "0".to_string());
        dr.insert(3i64, "14".to_string());
        let out = attach_track_tags(items, &[("dynamic_range", &dr)]);
        assert_eq!(out[0]["dynamic_range"], "0");
        assert!(
            out[1].get("dynamic_range").is_none(),
            "une piste sans tag DR sort sans la clé, pas avec null"
        );
        assert_eq!(out[2]["dynamic_range"], "14");
    }

    /// GROUPING et DR cohabitent sans se contaminer : chaque clé n'apparaît
    /// que sur les pistes qui la portent.
    #[test]
    fn attach_track_tags_keeps_keys_independent() {
        let items = vec![track(1, "I. Allegro"), track(2, "Bonus")];
        let mut grouping = HashMap::new();
        grouping.insert(1i64, "Sonates".to_string());
        let mut dr = HashMap::new();
        dr.insert(2i64, "11".to_string());
        let out = attach_track_tags(items, &[("grouping", &grouping), ("dynamic_range", &dr)]);
        assert_eq!(out[0]["grouping"], "Sonates");
        assert!(out[0].get("dynamic_range").is_none());
        assert!(out[1].get("grouping").is_none());
        assert_eq!(out[1]["dynamic_range"], "11");
    }
}

#[cfg(test)]
mod tests_editions {
    use super::*;
    use tune_core::db::album_distinct_repo::AlbumDistinctRepo;

    /// BIB-B1 : l'étiquette d'édition est le suffixe après le délimiteur,
    /// sans parenthèse ni crochet fermant ; le titre de base n'en a pas ; un
    /// titre qui ne descend pas de la base non plus.
    #[test]
    fn l_etiquette_d_edition_est_le_suffixe_nu() {
        assert_eq!(
            etiquette_d_edition("Abbey Road", "Abbey Road (Remastered 2009)").as_deref(),
            Some("Remastered 2009")
        );
        assert_eq!(
            etiquette_d_edition("Abbey Road", "abbey road [Super Deluxe]").as_deref(),
            Some("Super Deluxe")
        );
        assert_eq!(
            etiquette_d_edition("Abbey Road", "Abbey Road - 2019 Mix").as_deref(),
            Some("2019 Mix")
        );
        assert_eq!(etiquette_d_edition("Abbey Road", "Abbey Road"), None);
        assert_eq!(etiquette_d_edition("Abbey Road", "Abbey Roads"), None);
        assert_eq!(etiquette_d_edition("Abbey Road", "Let It Be"), None);
        assert_eq!(
            titre_de_base(&["Abbey Road (Remastered 2009)".into(), "Abbey Road".into()]),
            "Abbey Road"
        );
    }

    /// BIB-B1 : les éditions d'un album sont les albums du MÊME artiste au
    /// titre équivalent, nommées par leur suffixe, avec format et résolution ;
    /// un autre artiste, un autre titre n'y entrent pas ; une paire déclarée
    /// distincte en sort ; depuis une édition on retrouve la base.
    #[tokio::test]
    async fn les_editions_d_un_album_sont_nommees_jamais_fusionnees() {
        let state = AppState::new(":memory:", 0, Default::default()).unwrap();
        let b = &state.backend;
        let artiste = |nom: &str| {
            b.execute(
                "INSERT INTO artists (name) VALUES (?1)",
                &[&nom as &dyn ToSqlValue],
            )
            .unwrap();
            b.last_insert_rowid()
        };
        let album = |titre: &str,
                     artiste_id: i64,
                     annee: i64,
                     format: &str,
                     sr: i64,
                     bd: i64,
                     pistes: i64| {
            b.execute(
                "INSERT INTO albums (title, artist_id, year, format, sample_rate, bit_depth, track_count) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                &[
                    &titre as &dyn ToSqlValue,
                    &artiste_id as &dyn ToSqlValue,
                    &annee as &dyn ToSqlValue,
                    &format as &dyn ToSqlValue,
                    &sr as &dyn ToSqlValue,
                    &bd as &dyn ToSqlValue,
                    &pistes as &dyn ToSqlValue,
                ],
            )
            .unwrap();
            b.last_insert_rowid()
        };
        let beatles = artiste("The Beatles");
        let autre = artiste("Tribute Band");
        let base = album("Abbey Road", beatles, 1969, "FLAC", 44_100, 16, 17);
        let remaster = album(
            "Abbey Road (Remastered 2009)",
            beatles,
            2009,
            "FLAC",
            96_000,
            24,
            17,
        );
        let deluxe = album(
            "Abbey Road [Super Deluxe]",
            beatles,
            2019,
            "FLAC",
            96_000,
            24,
            40,
        );
        let _let_it_be = album("Let It Be", beatles, 1970, "FLAC", 44_100, 16, 12);
        let _hommage = album("Abbey Road", autre, 2001, "MP3", 44_100, 16, 17);

        let Json(v) = album_editions(State(state.clone()), Path(base))
            .await
            .ok()
            .expect("la route repond");
        assert_eq!(v["base_title"], "Abbey Road");
        assert_eq!(v["edition"], Value::Null);
        assert_eq!(v["count"], 2, "{v}");
        let eds = v["editions"].as_array().unwrap();
        assert_eq!(eds[0]["id"], remaster);
        assert_eq!(eds[0]["edition"], "Remastered 2009");
        assert_eq!(eds[0]["sample_rate"], 96_000);
        assert_eq!(eds[0]["bit_depth"], 24);
        assert_eq!(eds[0]["same_track_count"], true);
        assert_eq!(eds[1]["id"], deluxe);
        assert_eq!(eds[1]["edition"], "Super Deluxe");
        assert_eq!(eds[1]["same_track_count"], false);

        // Depuis une édition, la base et l'autre édition ressortent, et
        // l'album interrogé connaît sa propre étiquette.
        let Json(v) = album_editions(State(state.clone()), Path(deluxe))
            .await
            .ok()
            .expect("la route repond");
        assert_eq!(v["base_title"], "Abbey Road");
        assert_eq!(v["edition"], "Super Deluxe");
        assert_eq!(v["count"], 2, "{v}");

        // Une paire déclarée distincte par l'utilisateur sort de la liste.
        AlbumDistinctRepo::with_backend(state.backend.clone())
            .declarer_distincts(base, deluxe)
            .unwrap();
        let Json(v) = album_editions(State(state.clone()), Path(base))
            .await
            .ok()
            .expect("la route repond");
        assert_eq!(v["count"], 1, "{v}");
        assert_eq!(v["editions"][0]["id"], remaster);

        // Un album inconnu : 404, pas une liste vide.
        assert!(
            album_editions(State(state.clone()), Path(999_999))
                .await
                .is_err()
        );
    }
}

#[cfg(test)]
mod tests_albums_eclates {
    use super::{DistinctPairSet, PisteVue, cle_titre, dossier_de, grouper_les_albums_eclates};

    fn piste(
        album_id: i64,
        titre: &str,
        artiste: &str,
        annee: Option<i64>,
        chemin: &str,
        n: i64,
    ) -> PisteVue {
        piste_pochette(album_id, titre, artiste, annee, chemin, n, "")
    }

    #[allow(clippy::too_many_arguments)]
    fn piste_pochette(
        album_id: i64,
        titre: &str,
        artiste: &str,
        annee: Option<i64>,
        chemin: &str,
        n: i64,
        pochette: &str,
    ) -> PisteVue {
        PisteVue {
            album_id,
            album_title: titre.into(),
            artist_name: artiste.into(),
            year: annee,
            file_path: chemin.into(),
            track_number: n,
            cover_path: pochette.into(),
        }
    }

    /// Un condensat de pochette bien formé, distinct pour chaque graine.
    fn condensat(graine: u8) -> String {
        format!("{:02x}", graine).repeat(32)
    }

    /// BIB-A2 : le cas de l'enregistreur — un même dossier, un même titre, deux
    /// albums sous deux artistes, numéros complémentaires → un groupe sûr.
    /// Deux dossiers différents au même titre → pas un éclatement. Même
    /// dossier, numéros qui se recouvrent → groupe, mais signalé non
    /// complémentaire.
    #[test]
    fn l_enregistreur_eclate_un_album_et_le_faisceau_le_retrouve() {
        let pistes = vec![
            piste(
                1,
                "Abbey Road",
                "The Beatles",
                Some(1969),
                "/m/Beatles/Abbey Road/01.flac",
                1,
            ),
            piste(
                1,
                "Abbey Road",
                "The Beatles",
                Some(1969),
                "/m/Beatles/Abbey Road/02.flac",
                2,
            ),
            piste(
                2,
                "Abbey Road",
                "The Beatles feat. Billy Preston",
                Some(1969),
                "/m/Beatles/Abbey Road/03.flac",
                3,
            ),
            piste(
                3,
                "Abbey Road",
                "Tribute Band",
                Some(2001),
                "/m/Tribute/Abbey Road/01.flac",
                1,
            ),
            piste(
                4,
                "Kind of Blue",
                "Miles Davis",
                Some(1959),
                "/m/Miles/Kind of Blue/01.flac",
                1,
            ),
            piste(
                5,
                "Kind Of Blue.",
                "Miles Davis Sextet",
                Some(1997),
                "/m/Miles/Kind of Blue/01 (bis).flac",
                1,
            ),
        ];
        let g = grouper_les_albums_eclates(&pistes, &DistinctPairSet::default());
        assert_eq!(g.len(), 2, "{g:#?}");
        assert_eq!(g[0]["dossier"], "/m/Beatles/Abbey Road");
        assert_eq!(g[0]["numeros_complementaires"], true);
        assert_eq!(g[0]["meme_annee"], true);
        assert_eq!(g[0]["pistes"], 3);
        assert_eq!(g[0]["albums"].as_array().unwrap().len(), 2);
        assert_eq!(g[0]["albums"][0]["id"], 1);
        assert_eq!(g[0]["albums"][1]["id"], 2);
        assert_eq!(g[1]["dossier"], "/m/Miles/Kind of Blue");
        assert_eq!(g[1]["numeros_complementaires"], false);
        assert_eq!(g[1]["meme_annee"], false);
        let ids: Vec<i64> = g
            .iter()
            .flat_map(|x| {
                x["albums"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|a| a["id"].as_i64().unwrap())
            })
            .collect();
        assert!(
            !ids.contains(&3),
            "un autre dossier n'est pas un éclatement"
        );
    }

    /// Les deux briques : le dossier (séparateurs `/` et `\`), le titre normalisé.
    #[test]
    fn le_dossier_et_la_cle_de_titre() {
        assert_eq!(
            dossier_de("/m/Beatles/Abbey Road/01.flac"),
            "/m/Beatles/Abbey Road"
        );
        assert_eq!(
            dossier_de("C:\\Musique\\Abbey Road\\01.flac"),
            "C:\\Musique\\Abbey Road"
        );
        assert_eq!(dossier_de("01.flac"), "");
        assert_eq!(cle_titre("Kind Of Blue."), "kind of blue");
        assert_eq!(
            cle_titre("Été 85 (Bande originale)"),
            "ete 85 bande originale"
        );
        assert_eq!(cle_titre("..."), "");
    }

    /// #3396, l'indice que le ticket met en avant : la compilation éclatée par
    /// ARTISTE. Trois dossiers, trois titres différents, aucun dossier commun —
    /// donc le faisceau historique est aveugle — mais une seule et même
    /// pochette, et des numéros de piste qui se complètent (1-2, 3-4, 5).
    #[test]
    fn la_compilation_eclatee_par_artiste_se_retrouve_par_sa_pochette() {
        let p = condensat(0xab);
        let pistes = vec![
            piste_pochette(
                1,
                "Jazz 70 - Davis",
                "Miles Davis",
                Some(1970),
                "/m/A/1.flac",
                1,
                &p,
            ),
            piste_pochette(
                1,
                "Jazz 70 - Davis",
                "Miles Davis",
                Some(1970),
                "/m/A/2.flac",
                2,
                &p,
            ),
            piste_pochette(
                2,
                "Jazz 70 - Coltrane",
                "John Coltrane",
                Some(1970),
                "/m/B/1.flac",
                3,
                &p,
            ),
            piste_pochette(
                2,
                "Jazz 70 - Coltrane",
                "John Coltrane",
                Some(1970),
                "/m/B/2.flac",
                4,
                &p,
            ),
            piste_pochette(
                3,
                "Jazz 70 - Evans",
                "Bill Evans",
                Some(1970),
                "/m/C/1.flac",
                5,
                &p,
            ),
        ];
        let g = grouper_les_albums_eclates(&pistes, &DistinctPairSet::default());
        assert_eq!(g.len(), 1, "{g:#?}");
        assert_eq!(g[0]["indice"], super::INDICE_POCHETTE_IDENTIQUE);
        assert_eq!(g[0]["pochette"], p);
        assert_eq!(g[0]["pistes"], 5);
        let ids: Vec<i64> = g[0]["albums"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a["id"].as_i64().unwrap())
            .collect();
        assert_eq!(ids, vec![1, 2, 3]);
    }

    /// Le garde-fou du ticket : une RÉÉDITION partage la pochette de
    /// l'original, et ses numéros de piste se recouvrent. Elle ne doit JAMAIS
    /// être proposée à la fusion — c'est la distinction que l'utilisateur a
    /// voulue.
    #[test]
    fn une_reedition_partage_la_pochette_mais_se_recouvre_donc_rien() {
        let p = condensat(0x5c);
        let pistes = vec![
            piste_pochette(
                1,
                "Blue Train",
                "John Coltrane",
                Some(1957),
                "/m/Orig/1.flac",
                1,
                &p,
            ),
            piste_pochette(
                1,
                "Blue Train",
                "John Coltrane",
                Some(1957),
                "/m/Orig/2.flac",
                2,
                &p,
            ),
            piste_pochette(
                2,
                "Blue Train (Remaster)",
                "John Coltrane",
                Some(2003),
                "/m/Remaster/1.flac",
                1,
                &p,
            ),
            piste_pochette(
                2,
                "Blue Train (Remaster)",
                "John Coltrane",
                Some(2003),
                "/m/Remaster/2.flac",
                2,
                &p,
            ),
        ];
        assert!(
            grouper_les_albums_eclates(&pistes, &DistinctPairSet::default()).is_empty(),
            "une réédition ne se fusionne pas avec son original"
        );
    }

    /// Ce qui n'est pas un condensat ne rapproche rien : une valeur vide, un
    /// chemin relatif ou une chaîne hors alphabet hexadécimal partagés par
    /// deux albums ne sont pas une identité d'image.
    #[test]
    fn une_pochette_qui_n_est_pas_un_condensat_ne_rapproche_rien() {
        for valeur in ["", "covers/generique.jpg", "zzzz"] {
            let pistes = vec![
                piste_pochette(1, "Un", "A", Some(2000), "/m/A/1.flac", 1, valeur),
                piste_pochette(2, "Deux", "B", Some(2000), "/m/B/1.flac", 2, valeur),
            ];
            assert!(
                grouper_les_albums_eclates(&pistes, &DistinctPairSet::default()).is_empty(),
                "« {valeur} » n'est pas une identité d'image"
            );
        }
    }

    /// Sans numérotation, la complémentarité est vraie par VACUITÉ : deux
    /// albums dont toutes les pistes portent le numéro 0 se « complètent »
    /// sans qu'aucune pièce ne le dise. Une pochette générique de label
    /// suffirait alors à les rapprocher — c'est la première famille de faux
    /// que ce faisceau ouvrirait.
    #[test]
    fn sans_numerotation_la_pochette_partagee_ne_suffit_pas() {
        let p = condensat(0x33);
        let pistes = vec![
            piste_pochette(1, "Sans titre A", "A", None, "/m/A/1.flac", 0, &p),
            piste_pochette(1, "Sans titre A", "A", None, "/m/A/2.flac", 0, &p),
            piste_pochette(2, "Sans titre B", "B", None, "/m/B/1.flac", 0, &p),
        ];
        assert!(
            grouper_les_albums_eclates(&pistes, &DistinctPairSet::default()).is_empty(),
            "la complémentarité vide ne fonde rien"
        );

        // Et il suffit qu'UN membre ne soit pas numéroté pour que le faisceau
        // retombe : la preuve doit venir de chacun d'eux.
        let pistes = vec![
            piste_pochette(1, "Sans titre A", "A", None, "/m/A/1.flac", 1, &p),
            piste_pochette(1, "Sans titre A", "A", None, "/m/A/2.flac", 2, &p),
            piste_pochette(2, "Sans titre B", "B", None, "/m/B/1.flac", 0, &p),
        ];
        assert!(
            grouper_les_albums_eclates(&pistes, &DistinctPairSet::default()).is_empty(),
            "un membre non numéroté n'apporte aucune preuve"
        );
    }

    /// Un éclatement déjà nommé par le dossier et le titre n'est pas redit une
    /// seconde fois parce que les deux éclats partagent aussi leur pochette :
    /// un même geste proposé deux fois ferait croire à deux gestes à faire.
    #[test]
    fn un_eclatement_deja_nomme_par_le_dossier_n_est_pas_redit() {
        let p = condensat(0x11);
        let pistes = vec![
            piste_pochette(
                1,
                "Abbey Road",
                "The Beatles",
                Some(1969),
                "/m/AR/1.flac",
                1,
                &p,
            ),
            piste_pochette(
                2,
                "Abbey Road",
                "The Beatles feat. Billy Preston",
                Some(1969),
                "/m/AR/2.flac",
                2,
                &p,
            ),
        ];
        let g = grouper_les_albums_eclates(&pistes, &DistinctPairSet::default());
        assert_eq!(g.len(), 1, "{g:#?}");
        assert_eq!(g[0]["indice"], super::INDICE_DOSSIER_ET_TITRE);
    }
}

#[cfg(test)]
mod tests_regroupement;

#[cfg(test)]
mod tests_contenu;

/// #3397 — le témoin qui empêche les deux chemins de `GET /library/albums` de
/// diverger.
///
/// La route bâtit sa page en deux temps depuis #1269 : un tri sur des lignes
/// étroites, puis la matérialisation de la page. `added_at` n'était lu que
/// dans le PREMIER temps, et seulement quand le tri portait sur lui : dès
/// qu'un autre tri était demandé, les cinquante lignes sortaient avec
/// `added_at: null` — y compris sous `sort=added`, qui triait donc par une
/// date qu'il taisait, ce qui bloquait « trier par date d'ajout » (#3351).
///
/// Le témoin ne vérifie pas `added_at` seul : il compare les OBJETS ENTIERS
/// rendus par le chemin trié et par le chemin non trié pour le même album.
/// Tout futur champ qui n'existerait que d'un côté le fera tomber.
#[cfg(test)]
mod tests_tri_added_at {
    use axum::body::Body;
    use axum::http::Request;
    use serde_json::Value;
    use std::collections::HashMap;
    use tower::ServiceExt;
    use tune_core::db::backend::ToSqlValue;

    type Etat = crate::state::AppState;

    fn serveur() -> (axum::Router, Etat) {
        let state = Etat::new(":memory:", 0, Default::default()).unwrap();
        // `albums.artist_id` et `tracks.artist_id` référencent `artists(id)` :
        // sans cet artiste, l'insertion tombe sur « FOREIGN KEY constraint
        // failed » et le témoin serait vert contre une base vide.
        state
            .backend
            .execute(
                "INSERT INTO artists (id, name) VALUES (1, 'Radiohead')",
                &[],
            )
            .unwrap();
        let routeur = crate::routes::router(state.clone());
        (routeur, state)
    }

    fn album_avec_piste(state: &Etat, titre: &str, chemin: &str, mtime: f64) {
        state
            .backend
            .execute(
                "INSERT INTO albums (title, artist_id, source, track_count, year) \
                 VALUES (?, 1, 'local', 1, 2001)",
                &[&titre as &dyn ToSqlValue],
            )
            .unwrap();
        let album_id = state.backend.last_insert_rowid();
        state
            .backend
            .execute(
                "INSERT INTO tracks (title, album_id, artist_id, track_number, file_path, \
                 file_mtime, duration_ms) VALUES (?, ?, 1, 1, ?, ?, 30000)",
                &[&titre as &dyn ToSqlValue, &album_id, &chemin, &mtime],
            )
            .unwrap();
    }

    /// Les lignes de la réponse, indexées par identifiant d'album.
    async fn lignes_par_id(app: &axum::Router, requete: &str) -> HashMap<i64, Value> {
        let reponse = app
            .clone()
            .oneshot(Request::get(requete).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert!(
            reponse.status().is_success(),
            "{requete} : {}",
            reponse.status()
        );
        let corps = axum::body::to_bytes(reponse.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: Value = serde_json::from_slice(&corps).unwrap();
        json["items"]
            .as_array()
            .expect("items")
            .iter()
            .map(|a| (a["id"].as_i64().expect("id"), a.clone()))
            .collect()
    }

    #[tokio::test]
    async fn le_chemin_trie_rend_le_meme_objet_que_le_chemin_non_trie_3397() {
        let (app, state) = serveur();
        album_avec_piste(&state, "Amnesiac", "/a.flac", 1_000.0);
        album_avec_piste(&state, "Kid A", "/b.flac", 2_000.0);
        album_avec_piste(&state, "OK Computer", "/c.flac", 3_000.0);

        let sans_tri = lignes_par_id(&app, "/api/v1/library/albums?limit=50&offset=0").await;
        assert_eq!(sans_tri.len(), 3, "trois albums attendus : {sans_tri:?}");
        for (id, ligne) in &sans_tri {
            assert!(
                ligne["added_at"].as_f64().is_some(),
                "album {id} : le chemin non trié date déjà l'album — {ligne}"
            );
        }

        for tri in ["title", "artist", "year", "added", "added_date", "random"] {
            let trie = lignes_par_id(
                &app,
                &format!("/api/v1/library/albums?limit=50&offset=0&sort={tri}&order=asc"),
            )
            .await;
            assert_eq!(
                trie.len(),
                sans_tri.len(),
                "tri {tri} : la page doit rester servie"
            );
            for (id, ligne) in &trie {
                assert_eq!(
                    ligne,
                    sans_tri.get(id).expect("même album des deux côtés"),
                    "tri {tri}, album {id} : les deux chemins doivent rendre le MÊME objet"
                );
            }
        }
    }

    /// `sort=added` est le cas qui a fait ouvrir l'issue : la route triait par
    /// la date d'ajout sans la rendre. Contre-épreuve directe.
    #[tokio::test]
    async fn sort_added_rend_la_date_sur_laquelle_il_vient_de_trier_3397() {
        let (app, state) = serveur();
        album_avec_piste(&state, "Amnesiac", "/a.flac", 1_000.0);
        album_avec_piste(&state, "Kid A", "/b.flac", 2_000.0);

        let lignes = lignes_par_id(
            &app,
            "/api/v1/library/albums?limit=50&offset=0&sort=added&order=desc",
        )
        .await;
        assert_eq!(lignes.len(), 2);
        for (id, ligne) in &lignes {
            assert!(
                ligne["added_at"].as_f64().is_some(),
                "album {id} : sort=added doit rendre added_at — {ligne}"
            );
        }
    }
}
