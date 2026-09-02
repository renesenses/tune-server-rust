use crate::routes::panne_sql::OuDefautJournalise;
use axum::extract::{Query, State};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use tune_core::db::backend::ToSqlValue;
use tune_core::db::engine::{Engine, PostgresDialect, SqlDialect, SqliteDialect};
use tune_core::db::history_repo::HistoryRepo;
use tune_core::db::home_queries::{self, HISTORIQUE_VERS_ALBUM};
use tune_core::db::radio_repo::RadioRepo;
use tune_core::db::settings_repo::SettingsRepo;

use crate::error::AppError;
use crate::state::AppState;

#[derive(Deserialize)]
struct HomeParams {
    limit: Option<i64>,
    /// Optional zone filter: when provided, continue-listening only shows
    /// albums listened on this zone.  Clients should send the CURRENT active
    /// zone so the response is relevant (DEvir QA B-09: zone mismatch).
    zone_id: Option<i64>,
}

/// Les parametres d'« Ajoutes recemment » (#3039).
///
/// `days` est la nouveaute : la fenetre etait ecrite en dur a 7 jours dans une
/// fonction SANS argument, si bien que le testeur qui demande « les quinze
/// derniers jours » ou « le dernier mois » n'avait aucun moyen de l'obtenir.
#[derive(Deserialize)]
struct RecentlyAddedParams {
    limit: Option<i64>,
    /// Largeur de la fenetre en jours. Absent ⇒ [`FENETRE_JOURS_DEFAUT`].
    days: Option<i64>,
}

/// La fenetre servie quand le client n'en demande aucune.
///
/// TEMOIN DE NON-REGRESSION : c'est exactement ce que la route rendait avant
/// #3039. Un client deja deploye qui appelle `/home/recently-added` sans rien
/// doit voir la meme chose qu'hier ; la valeur ne se change pas sans changer
/// ce que voient tous les ecrans d'accueil du parc.
pub(crate) const FENETRE_JOURS_DEFAUT: i64 = 7;

/// Le plafond de la fenetre, en jours : deux ans.
///
/// Une fenetre non bornee est une invitation a `?days=100000`, c'est-a-dire a
/// un balayage de la bibliotheque entiere — et il n'y a AUCUN index sur
/// `tracks.file_mtime` ni sur `file_first_seen.first_seen_at` (verifie sur les
/// deux moteurs le 02/09/2026) : le filtre est deja une passe complete sur
/// `tracks`, que seul le petit nombre de lignes retenues rend supportable.
/// Au-dela, la vue « ajouts recents » ne veut de toute facon plus rien dire :
/// c'est la bibliotheque, et la grille des albums triee par date d'ajout la
/// sert deja avec sa pagination.
///
/// Au-dessus, une erreur qui DIT la borne — pas un silence, pas un ecretage
/// muet qui ferait croire au client que sa demande a ete honoree.
pub(crate) const FENETRE_JOURS_MAX: i64 = 730;

/// Traduit une largeur de fenetre en borne basse (secondes epoch), ou refuse.
///
/// Refuser plutot qu'ecreter : un client qui demande 5 000 jours et recoit
/// 730 jours de resultats croit avoir tout. Il vaut mieux qu'il l'apprenne.
fn borne_basse_de_fenetre(days: Option<i64>) -> Result<f64, AppError> {
    let jours = days.unwrap_or(FENETRE_JOURS_DEFAUT);
    if !(1..=FENETRE_JOURS_MAX).contains(&jours) {
        return Err(AppError::bad_request(format!(
            "days={jours} hors bornes : la fenetre des ajouts recents va de 1 a \
             {FENETRE_JOURS_MAX} jours (defaut {FENETRE_JOURS_DEFAUT})"
        )));
    }
    let maintenant = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64();
    Ok(maintenant - (jours as f64) * 24.0 * 3600.0)
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(home_page))
        .route("/continue-listening", get(continue_listening))
        .route("/recently-added", get(recently_added))
        .route("/recently-added/summary", get(recently_added_summary))
        .route("/recommendations", get(home_recommendations))
        .route("/top-mixes", get(top_mixes))
        .route("/new-in-library", get(new_in_library))
        .route("/other-versions", get(other_versions))
        .route(
            "/artist-releases",
            get(super::artist_releases::artist_releases),
        )
        .route("/radio-picks", get(radio_picks))
        .route("/streaming-highlights", get(streaming_highlights))
}

/// Returns a placeholder string appropriate for the engine.
fn ph(engine: Engine, idx: usize) -> String {
    match engine {
        Engine::Sqlite => SqliteDialect.placeholder(idx),
        Engine::Postgres => PostgresDialect.placeholder(idx),
    }
}

/// Les cinq genres les plus ecoutes : celui de la piste s'il est connu, sinon
/// celui de l'album. Partage entre les recommandations et les « top mixes »,
/// qui prenaient tous deux le genre d'un album homonyme (#2731).
///
/// ATTENTION : telle quelle, cette requete ECHOUE sur les deux moteurs —
/// `WHERE genre IS NOT NULL` est ambigu entre `t.genre` et `a.genre`, et
/// l'erreur est avalee par le `unwrap_or_default` des appelants. La jointure
/// est corrigee ici pour le jour ou la requete sera reveillee ; la reveiller
/// releve d'un arbitrage produit (cf. le test
/// `les_genres_les_plus_ecoutes_ne_rendent_rien_ambiguite_sur_genre`), pas du
/// defaut d'homonymie.
fn sql_top_genres() -> String {
    format!(
        "SELECT genre, COUNT(*) as cnt \
         FROM (SELECT COALESCE(t.genre, a.genre) as genre \
               FROM listen_history lh \
               LEFT JOIN tracks t ON lh.track_id = t.id \
               LEFT JOIN albums a ON {HISTORIQUE_VERS_ALBUM} \
               WHERE genre IS NOT NULL AND genre != '') \
         GROUP BY genre ORDER BY cnt DESC LIMIT 5"
    )
}

/// Aggregated home page: returns all sections in a single response.
async fn home_page(State(state): State<AppState>) -> Result<Json<Value>, AppError> {
    // No zone filter for the aggregated home page — show all zones.
    let continue_items = fetch_continue_listening(&state, 10, None)?;
    // L'accueil agrege garde SA fenetre de 7 jours : c'est une vignette, pas
    // l'onglet. La fenetre choisie vit sur `/home/recently-added` (#3039).
    let recent_items = fetch_recently_added(&state, 20, borne_basse_de_fenetre(None)?)?;
    let top_tracks = fetch_top_tracks(&state, 20);
    let radios = fetch_radio_picks(&state)?;
    let discover = fetch_recommendations(&state, 20)?;

    let mut sections = Vec::new();

    if !continue_items.is_empty() {
        sections.push(json!({
            "id": "continue",
            "title": "Continuer l'\u{00e9}coute",
            "type": "albums",
            "items": continue_items,
        }));
    }

    if !recent_items.is_empty() {
        sections.push(json!({
            "id": "recent",
            "title": "Ajout\u{00e9}s r\u{00e9}cemment",
            "type": "albums",
            "items": recent_items,
        }));
    }

    if !top_tracks.is_empty() {
        sections.push(json!({
            "id": "top",
            "title": "Les plus \u{00e9}cout\u{00e9}s",
            "type": "tracks",
            "items": top_tracks,
        }));
    }

    if !radios.is_empty() {
        sections.push(json!({
            "id": "radios",
            "title": "Radios favorites",
            "type": "radios",
            "items": radios,
        }));
    }

    if !discover.is_empty() {
        sections.push(json!({
            "id": "discover",
            "title": "\u{00c0} d\u{00e9}couvrir",
            "type": "albums",
            "items": discover,
        }));
    }

    Ok(Json(json!({ "sections": sections })))
}

/// Albums from listen history where the user hasn't finished the album
/// (listened tracks < total tracks).
async fn continue_listening(
    State(state): State<AppState>,
    Query(p): Query<HomeParams>,
) -> Result<Json<Value>, AppError> {
    let limit = p.limit.unwrap_or(10);
    // When zone_id is provided, filter continue-listening items to albums
    // that were played on that specific zone. This prevents zone mismatch:
    // the client sends the CURRENT active zone, not a stored zone from
    // history (DEvir QA B-09).
    let items = fetch_continue_listening(&state, limit, p.zone_id)?;
    Ok(Json(json!(items)))
}

/// Combien de contextes distincts on ramene avant de filtrer.
///
/// Un contexte `album` peut disparaitre a l'enrichissement (disque termine),
/// une playlist peut avoir ete supprimee : sans marge, la section rendrait
/// moins d'entrees que demande alors qu'il en existe. Quatre fois la demande,
/// borne a 80 — au-dela on paierait un tri pour des entrees que personne ne
/// verra jamais, la section n'en affichant qu'une poignee.
fn marge_de_contextes(limit: i64) -> i64 {
    limit.saturating_mul(4).clamp(1, 80)
}

/// « Continuer l'ecoute » : ce que l'auditeur a demande en dernier, et OU il
/// en etait — pas « les albums qu'il n'a pas finis ».
///
/// # Le defaut corrige (#2441, FabienM, fil forum 1557)
///
/// La requete partait de `albums`, groupait par `a.id` et se terminait par
/// `HAVING listened_tracks < a.track_count`. Elle ne POUVAIT rien rendre
/// d'autre qu'un album de la bibliotheque locale : « si je choisis de jouer
/// une playlist complete, je m'attends a voir cette playlist » n'avait aucune
/// chance d'etre satisfait, quelle que soit l'interface.
///
/// # L'arbitrage rendu — objet courant + position, pas l'instantane de la file
///
/// On stocke le TYPE du contexte, son IDENTIFIANT et le RANG (migrations 84 et
/// 94), et on rouvre l'objet en se placant au bon endroit. On n'enregistre
/// PAS la file entiere :
/// * pour un artiste ou un label, la file est batie par une requete qui change
///   d'un jour a l'autre — « conserver l'ordre » n'y a pas de sens ;
/// * ecrire la file a chaque ecoute couterait un facteur dix sur le volume de
///   `listen_history`, pour alimenter une section d'accueil.
///
/// En lecture aleatoire on RE-TIRE : le rang est laisse NULL a l'ecriture
/// (`rang_a_retenir`, tune-core/src/orchestrator.rs), et l'entree se rouvre au
/// debut plutot que de faire semblant de rejouer le meme tirage.
///
/// # Seconde decision — le filtre « album non fini » ne vaut que pour un album
///
/// `listened_tracks < track_count` disparait pour les quatre autres natures :
/// un titre isole ou une playlist n'a aucune notion d'« incomplet », et l'y
/// appliquer les aurait fait disparaitre aussitot ecrits.
///
/// # Les lignes SANS contexte ne sont pas perdues
///
/// Une base en service porte des milliers de lignes anterieures a la migration
/// 84, a `context_type` NULL. Ne rendre que des contextes VIDERAIT la section
/// le jour de la mise a jour. L'ancienne requete est donc conservee telle
/// quelle en second rang, et ses albums sont fusionnes avec les contextes —
/// dedoublonnes par identifiant d'album, le contexte primant puisque lui seul
/// porte le rang.
fn fetch_continue_listening(
    state: &AppState,
    limit: i64,
    zone_id: Option<i64>,
) -> Result<Vec<Value>, AppError> {
    let engine = state.backend.engine();
    // When a zone_id filter is provided, only show entries that were listened
    // to on that zone.  This ensures the "continue listening" section matches
    // the user's currently selected zone (B-09 fix).
    let zone_filter = match zone_id {
        Some(zid) => format!("AND lh.zone_id = {zid} "),
        None => String::new(),
    };

    let mut items = contextes_recents(state, limit, &zone_filter);

    // Second rang : les albums DEDUITS, pour les lignes qui ne disent rien de
    // leur contexte. Deux garde-fous contre le doublon :
    //
    // * en SQL, `SUM(CASE WHEN context_type IS NULL ...)` — un album dont
    //   toutes les lignes portent un contexte est deja rendu par le premier
    //   rang, en mieux (il a le rang) ;
    // * en Rust, `deja` — tout album deja represente, y compris derriere un
    //   contexte `track`. Une piste jouee seule ne doit pas faire remonter en
    //   plus son disque : « Tune doit refleter la realite de ce qu'a voulu
    //   faire l'auditeur », pas doubler chaque geste.
    //
    // Le comptage, lui, reste sur TOUTES les lignes (HISTORIQUE_VERS_ALBUM
    // inchange depuis #2731) : n'avancer que sur les lignes sans contexte
    // sous-estimerait un disque commence avant la mise a jour et poursuivi
    // apres.
    //
    // DEUX ecritures imposees par PostgreSQL, mesurees sur une base reelle
    // (#2860) — SQLite tolerait les deux, et l'erreur etait avalee par le
    // `unwrap_or_default()` ci-dessous, donc la section disparaissait sans un
    // seul message :
    //
    // * `GROUP BY a.id` seul ne suffit pas. La dependance fonctionnelle de
    //   PostgreSQL ne couvre que les colonnes de la table dont on groupe la
    //   cle primaire ; `ar.name` vient d'une AUTRE table :
    //     ERROR: column "ar.name" must appear in the GROUP BY clause
    //            or be used in an aggregate function
    //   C'est la meme correction que `resoudre_albums` porte deja.
    //
    // * `HAVING listened_tracks < ...` ne marche pas non plus. Un alias de la
    //   liste SELECT n'existe pas encore quand le HAVING est evalue :
    //     ERROR: column "listened_tracks" does not exist
    //   (l'alias reste legal en ORDER BY et en GROUP BY, lui.) On repete donc
    //   l'agregat.
    let deja: std::collections::HashSet<i64> = items
        .iter()
        .filter_map(|(_, item)| item["album_id"].as_i64())
        .collect();
    let sql = home_queries::continue_listening_albums_deduits(engine, &zone_filter);
    let marge = marge_de_contextes(limit);
    let params: [&dyn ToSqlValue; 1] = [&marge];
    for cols in state
        .backend
        .query_many(&sql, &params)
        .ou_defaut_journalise()
    {
        let album_id = cols.first().and_then(|v| v.as_i64()).unwrap_or(0);
        if deja.contains(&album_id) {
            continue;
        }
        let titre = cols.get(1).and_then(|v| v.as_string()).unwrap_or_default();
        let dernier = cols.get(8).and_then(|v| v.as_string()).unwrap_or_default();
        let ecoutees = cols.get(6).and_then(|v| v.as_i64()).unwrap_or(0);
        let total = cols.get(7).and_then(|v| v.as_i64());
        items.push((
            dernier,
            json!({
                "id": album_id,
                "album_id": album_id,
                // Ces lignes ne DISENT pas qu'il s'agissait d'un album : c'est
                // deduit de la jointure. On le declare quand meme « album »,
                // c'est ce que la section montrait deja — mais sans rang, qui
                // n'a jamais ete ecrit pour elles.
                "context_type": "album",
                "context_id": album_id.to_string(),
                "position": Value::Null,
                "title": titre.clone(),
                "album_title": titre,
                "artist_name": cols.get(2).and_then(|v| v.as_string()),
                "year": cols.get(3).and_then(|v| v.as_i64()),
                "cover_path": cols.get(4).and_then(|v| v.as_string()),
                "genre": cols.get(5).and_then(|v| v.as_string()),
                "listened_tracks": ecoutees,
                "track_count": total,
                // Ces lignes-la, elles, ont TOUJOURS leur avancement : le
                // `HAVING listened_tracks < a.track_count` de la requete
                // garantit un `track_count` non nul et strictement positif.
                "progress_percent": home_queries::progression_pourcent(Some(ecoutees), total),
                "source": "local",
            }),
        ));
    }

    // Le plus recent d'abord, toutes natures confondues : l'auditeur relit son
    // geste le plus recent, pas « les albums puis les playlists ».
    //
    // Le departage par nature puis identifiant rend l'ordre TOTAL. Sans lui,
    // deux entrees de la meme seconde — `listened_at` est au format seconde,
    // et les deux rangs sont fusionnes ici — se classaient selon leur ordre
    // d'insertion, c'est-a-dire selon l'ordre qu'un moteur avait rendu. Le
    // `truncate` juste apres decidait alors, en silence, laquelle des deux
    // disparaissait de la section.
    items.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| {
                a.1["context_type"]
                    .as_str()
                    .cmp(&b.1["context_type"].as_str())
            })
            .then_with(|| a.1["context_id"].as_str().cmp(&b.1["context_id"].as_str()))
    });
    items.truncate(limit.max(0) as usize);
    Ok(items.into_iter().map(|(_, item)| item).collect())
}

/// La derniere ecoute de chaque contexte distinct, enrichie de quoi l'afficher.
///
/// Rend `(date d'ecoute, entree)` pour que l'appelant fusionne avec le second
/// rang sans avoir a relire la date dans le JSON.
fn contextes_recents(state: &AppState, limit: i64, zone_filter: &str) -> Vec<(String, Value)> {
    let engine = state.backend.engine();
    let marge = marge_de_contextes(limit);
    // La requete vit dans `tune-core` : c'est le SEUL crate que le job
    // « Test (PostgreSQL) » compile. Redigee ici, elle n'etait jouee que sur
    // SQLite — le trou par lequel #2860 etait passe (cf. home_queries.rs).
    let sql = home_queries::continue_listening_contextes(engine, zone_filter);
    let params: [&dyn ToSqlValue; 1] = [&marge];
    let rows = state
        .backend
        .query_many(&sql, &params)
        .ou_defaut_journalise();

    // Deux lignes d'un meme contexte peuvent porter la MEME `listened_at` (la
    // seconde est a la seconde pres) : la jointure sur le MAX les rend toutes
    // les deux. On garde la premiere, l'ordre etant deja decroissant.
    let mut vues = std::collections::HashSet::new();
    let brut: Vec<_> = rows
        .iter()
        .filter_map(|cols| {
            let nature = cols.first().and_then(|v| v.as_string())?;
            let id = cols.get(1).and_then(|v| v.as_string())?;
            vues.insert((nature.clone(), id.clone()))
                .then_some((nature, id, cols))
        })
        .collect();

    let albums = resoudre_albums(state, &brut);
    let playlists = resoudre_par_id(state, &brut, "playlist", "playlists");
    let artistes = resoudre_par_id(state, &brut, "artist", "artists");

    brut.into_iter()
        .filter_map(|(nature, id, cols)| {
            let dernier = cols.get(2).and_then(|v| v.as_string()).unwrap_or_default();
            let rang = cols.get(3).and_then(|v| v.as_i64());
            let titre_piste = cols.get(4).and_then(|v| v.as_string()).unwrap_or_default();
            let artiste = cols.get(5).and_then(|v| v.as_string());
            let titre_album = cols.get(6).and_then(|v| v.as_string());
            let pochette = cols.get(7).and_then(|v| v.as_string());
            let album_id = cols.get(8).and_then(|v| v.as_i64());
            let source = cols
                .get(9)
                .and_then(|v| v.as_string())
                .unwrap_or_else(|| "local".into());

            // Socle commun : ce que TOUTES les natures portent. Les champs
            // d'album restent presents et nuls hors album — les clients
            // deployes lisent `album_id` sans savoir qu'un type existe, mieux
            // vaut un champ nul qu'un champ absent.
            let mut item = json!({
                "context_type": nature,
                "context_id": id,
                "position": rang,
                "source": source,
                "id": album_id.unwrap_or(0),
                "album_id": album_id,
                "artist_name": artiste,
                "album_title": titre_album,
                "cover_path": pochette,
                "year": Value::Null,
                "genre": Value::Null,
                "listened_tracks": Value::Null,
                "track_count": Value::Null,
                // Aucune des quatre autres natures n'a de notion de completude :
                // la barre reste absente plutot que de mentir a 0 %. Seule la
                // branche `album` ci-dessous la renseigne.
                "progress_percent": Value::Null,
                "title": titre_piste,
            });
            let o = item.as_object_mut()?;

            match nature.as_str() {
                "album" => {
                    // Album LOCAL : titre, artiste, pochette et avancement
                    // viennent de la bibliotheque. C'est la SEULE nature ou le
                    // filtre « pas encore fini » s'applique — un disque termine
                    // n'a plus rien a « continuer ».
                    if let Some(a) = albums.get(&id) {
                        if let (Some(lus), Some(total)) = (a.listened_tracks, a.track_count) {
                            if total > 0 && lus >= total {
                                return None;
                            }
                        }
                        o.insert("id".into(), json!(a.id));
                        o.insert("album_id".into(), json!(a.id));
                        o.insert("title".into(), json!(a.title));
                        o.insert("album_title".into(), json!(a.title));
                        o.insert("artist_name".into(), json!(a.artist_name));
                        o.insert("year".into(), json!(a.year));
                        o.insert("cover_path".into(), json!(a.cover_path));
                        o.insert("genre".into(), json!(a.genre));
                        o.insert("listened_tracks".into(), json!(a.listened_tracks));
                        o.insert("track_count".into(), json!(a.track_count));
                        // La barre que `HomeView.svelte` dessine sous la
                        // vignette. Calculee en Rust : en SQL, un album a
                        // `track_count` = 0 ferait LEVER PostgreSQL sur une
                        // division par zero et emporterait toute la section.
                        o.insert(
                            "progress_percent".into(),
                            json!(home_queries::progression_pourcent(
                                a.listened_tracks,
                                a.track_count
                            )),
                        );
                    } else {
                        // Album de STREAMING (`context_id` non numerique) ou
                        // disque disparu de la bibliotheque : la ligne
                        // d'historique porte son titre et sa pochette, elles
                        // suffisent a l'afficher. Pas d'avancement — on ne
                        // connait pas le nombre de pistes.
                        o.insert("title".into(), json!(titre_album.clone()));
                    }
                }
                "playlist" => {
                    // Playlist LOCALE : son nom fait foi. Une playlist de
                    // streaming n'a pas de ligne dans `playlists` et son nom
                    // n'est cache NULLE PART en base — l'entree part avec son
                    // `context_id` et sa `source`, a charge du client de la
                    // nommer. Mieux qu'un titre de piste presente pour un nom
                    // de playlist.
                    o.insert("title".into(), json!(playlists.get(&id)));
                    o.insert("album_id".into(), Value::Null);
                    o.insert("album_title".into(), Value::Null);
                }
                "artist" => {
                    // L'artiste demande, pas celui de la derniere piste jouee :
                    // sur une compilation ils different.
                    let nom = artistes.get(&id).cloned().or(artiste);
                    o.insert("title".into(), json!(nom.clone()));
                    o.insert("artist_name".into(), json!(nom));
                    o.insert("album_id".into(), Value::Null);
                    o.insert("album_title".into(), Value::Null);
                }
                "label" => {
                    // Un label n'a NI table NI identifiant : l'onglet Labels
                    // lit une facette et selectionne par CHAINE (meme constat
                    // qu'a la migration 85). `context_id` EST donc le nom du
                    // label, et il est son propre titre.
                    o.insert("title".into(), json!(id));
                    o.insert("album_id".into(), Value::Null);
                    o.insert("album_title".into(), Value::Null);
                }
                // "track" : le titre de la piste est deja en place, et
                // `album_id` / `album_title` restent renseignes pour que le
                // client puisse remonter au disque qui la porte.
                _ => {}
            }

            Some((dernier, item))
        })
        .collect()
}

/// Ce qu'un album de la bibliotheque apporte a une entree, une fois resolu.
struct AlbumResolu {
    id: i64,
    title: String,
    artist_name: Option<String>,
    year: Option<i64>,
    cover_path: Option<String>,
    genre: Option<String>,
    listened_tracks: Option<i64>,
    track_count: Option<i64>,
}

/// Resout d'UN COUP les contextes `album` dont l'identifiant designe un album
/// local, avec leur avancement. Une requete pour toute la section, pas une par
/// tuile : l'accueil est sur le chemin du premier affichage.
///
/// Les identifiants sont interpoles apres avoir ete convertis en `i64` — ce
/// sont des entiers, pas des chaines d'origine inconnue, exactement comme le
/// filtre de zone juste au-dessus.
fn resoudre_albums(
    state: &AppState,
    brut: &[(String, String, &Vec<tune_core::db::backend::SqlValue>)],
) -> std::collections::HashMap<String, AlbumResolu> {
    let ids: Vec<i64> = brut
        .iter()
        .filter(|(nature, _, _)| nature == "album")
        .filter_map(|(_, id, _)| id.parse::<i64>().ok())
        .collect();
    if ids.is_empty() {
        return std::collections::HashMap::new();
    }
    // GROUP BY exhaustif, et non `GROUP BY a.id` : la dependance fonctionnelle
    // de PostgreSQL ne couvre que les colonnes de la table dont on groupe la
    // cle primaire — `ar.name` vient d'une AUTRE table et ferait echouer la
    // requete sur ce moteur. La requete vit dans `tune-core` pour que le job
    // « Test (PostgreSQL) » l'execute vraiment (#2441).
    let sql = home_queries::continue_listening_albums_du_contexte(&ids);
    state
        .backend
        .query_many(&sql, &[])
        .ou_defaut_journalise()
        .iter()
        .filter_map(|cols| {
            let id = cols.first().and_then(|v| v.as_i64())?;
            Some((
                id.to_string(),
                AlbumResolu {
                    id,
                    title: cols.get(1).and_then(|v| v.as_string()).unwrap_or_default(),
                    artist_name: cols.get(2).and_then(|v| v.as_string()),
                    year: cols.get(3).and_then(|v| v.as_i64()),
                    cover_path: cols.get(4).and_then(|v| v.as_string()),
                    genre: cols.get(5).and_then(|v| v.as_string()),
                    listened_tracks: cols.get(6).and_then(|v| v.as_i64()),
                    track_count: cols.get(7).and_then(|v| v.as_i64()),
                },
            ))
        })
        .collect()
}

/// Le NOM des objets d'une nature donnee, resolus d'un coup dans leur table.
///
/// Sert `playlist` (table `playlists`) et `artist` (table `artists`), qui
/// portent toutes deux une colonne `name` et un identifiant entier. Un
/// identifiant non numerique — playlist de streaming — n'entre pas dans la
/// requete : il n'a rien a y trouver.
fn resoudre_par_id(
    state: &AppState,
    brut: &[(String, String, &Vec<tune_core::db::backend::SqlValue>)],
    nature: &str,
    table: &str,
) -> std::collections::HashMap<String, String> {
    let ids: Vec<i64> = brut
        .iter()
        .filter(|(n, _, _)| n == nature)
        .filter_map(|(_, id, _)| id.parse::<i64>().ok())
        .collect();
    if ids.is_empty() {
        return std::collections::HashMap::new();
    }
    let liste = ids
        .iter()
        .map(i64::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!("SELECT id, name FROM {table} WHERE id IN ({liste})");
    state
        .backend
        .query_many(&sql, &[])
        .ou_defaut_journalise()
        .iter()
        .filter_map(|cols| {
            let id = cols.first().and_then(|v| v.as_i64())?;
            let nom = cols.get(1).and_then(|v| v.as_string())?;
            Some((id.to_string(), nom))
        })
        .collect()
}

#[cfg(test)]
mod tests_homonymes {
    use super::*;

    /// Les deux disques de Tades : un « Live » de Police et un « Live » de
    /// Pulp, meme titre au caractere pres, cinq pistes chacun.
    /// Rend `(id du Live de Police, id du Live de Pulp)`.
    fn deux_live(state: &AppState, genre: Option<&str>) -> (i64, i64) {
        let b = &state.backend;
        let poser = |artiste: &str| -> i64 {
            b.execute(
                "INSERT INTO artists (name) VALUES (?1)",
                &[&artiste as &dyn ToSqlValue],
            )
            .unwrap();
            let artiste_id = b.last_insert_rowid();
            b.execute(
                "INSERT INTO albums (title, artist_id, track_count, genre) \
                 VALUES ('Live', ?1, 5, ?2)",
                &[&artiste_id as &dyn ToSqlValue, &genre as &dyn ToSqlValue],
            )
            .unwrap();
            b.last_insert_rowid()
        };
        let police = poser("The Police");
        let pulp = poser("Pulp");
        (police, pulp)
    }

    fn ecoute(state: &AppState, titre: &str, artiste: Option<&str>, album_id: Option<i64>) {
        state
            .backend
            .execute(
                "INSERT INTO listen_history \
                 (title, artist_name, album_title, album_id, listened_at) \
                 VALUES (?1, ?2, 'Live', ?3, '2026-08-28T22:45:00Z')",
                &[
                    &titre as &dyn ToSqlValue,
                    &artiste as &dyn ToSqlValue,
                    &album_id as &dyn ToSqlValue,
                ],
            )
            .unwrap();
    }

    /// Le defaut de Tades (#2731, fil 1600) : ecouter le « Live » de Pulp
    /// faisait remonter celui de Police, et le compteur d'avancement
    /// additionnait les pistes des deux.
    #[test]
    fn le_live_de_pulp_ne_fait_pas_remonter_celui_de_police() {
        let state = AppState::new(":memory:", 0, Default::default()).unwrap();
        let (police, pulp) = deux_live(&state, None);
        ecoute(&state, "Common People", Some("Pulp"), Some(pulp));
        ecoute(&state, "Disco 2000", Some("Pulp"), Some(pulp));

        let Ok(items) = fetch_continue_listening(&state, 10, None) else {
            panic!("la requete doit repondre")
        };

        assert_eq!(items.len(), 1, "albums rendus : {items:?}");
        assert_eq!(items[0]["album_id"].as_i64(), Some(pulp));
        assert_eq!(items[0]["artist_name"].as_str(), Some("Pulp"));
        assert_eq!(
            items[0]["listened_tracks"].as_i64(),
            Some(2),
            "le compteur ne doit compter que les pistes de CET album"
        );
        assert_ne!(items[0]["album_id"].as_i64(), Some(police));
    }

    /// `record_listen` ne connait l'album que par la piste locale : une ecoute
    /// en flux (`track_id` absent) et toute ligne anterieure a la migration
    /// `add_listen_history_source_id_album_id` ont `album_id` a NULL. Joindre
    /// sur le seul identifiant VIDERAIT la section pour ces gens-la — le repli
    /// titre + artiste doit tenir.
    #[test]
    fn une_ecoute_sans_album_id_reste_rattachee_par_titre_et_artiste() {
        let state = AppState::new(":memory:", 0, Default::default()).unwrap();
        let (_police, pulp) = deux_live(&state, None);
        ecoute(&state, "Common People", Some("Pulp"), None);

        let Ok(items) = fetch_continue_listening(&state, 10, None) else {
            panic!("la requete doit repondre")
        };

        assert_eq!(items.len(), 1, "albums rendus : {items:?}");
        assert_eq!(items[0]["album_id"].as_i64(), Some(pulp));
        assert_eq!(items[0]["artist_name"].as_str(), Some("Pulp"));
    }

    /// Sans artiste NI identifiant, un titre qui ne designe QU'UN album reste
    /// rattache : perdre l'entree serait pire que la garder. C'est la seconde
    /// branche de `find_album_by_identity` — titre seul, mais non ambigu.
    #[test]
    fn une_ecoute_sans_artiste_ni_identifiant_n_est_pas_perdue() {
        let state = AppState::new(":memory:", 0, Default::default()).unwrap();
        let b = &state.backend;
        b.execute("INSERT INTO artists (name) VALUES ('Pulp')", &[])
            .unwrap();
        let artiste_id = b.last_insert_rowid();
        b.execute(
            "INSERT INTO albums (title, artist_id, track_count) VALUES ('Live', ?1, 5)",
            &[&artiste_id as &dyn ToSqlValue],
        )
        .unwrap();
        let seul = b.last_insert_rowid();
        ecoute(&state, "Piste inconnue", None, None);

        let Ok(items) = fetch_continue_listening(&state, 10, None) else {
            panic!("la requete doit repondre")
        };

        assert_eq!(
            items.len(),
            1,
            "la section ne doit pas se vider : {items:?}"
        );
        assert_eq!(items[0]["album_id"].as_i64(), Some(seul));
    }

    /// Sans artiste NI identifiant, un titre PARTAGE par deux disques ne
    /// designe rien. Le rattacher aux deux, c'est le defaut de Tades par
    /// l'autre porte : le « Live » de Police remontait sans avoir ete joue.
    /// `find_album_by_identity` refuse ce repli depuis #1391 (« ok » de daoud
    /// vs « OK » de Talvin Singh) ; la section s'aligne.
    #[test]
    fn une_ecoute_sans_artiste_sur_un_titre_ambigu_ne_rattache_rien() {
        let state = AppState::new(":memory:", 0, Default::default()).unwrap();
        deux_live(&state, None);
        ecoute(&state, "Piste inconnue", None, None);

        let Ok(items) = fetch_continue_listening(&state, 10, None) else {
            panic!("la requete doit repondre")
        };

        assert!(
            items.is_empty(),
            "un titre partage par deux albums ne designe aucun des deux : {items:?}"
        );
    }

    /// Une chaine vide n'est pas un artiste. Traitee comme une valeur, elle ne
    /// s'egalait a rien : l'album disparaissait de la section au lieu de
    /// retomber sur le titre seul. `find_album_by_identity` teste
    /// `artist.is_empty()`, pas la nullite.
    #[test]
    fn un_artiste_vide_vaut_artiste_inconnu() {
        let state = AppState::new(":memory:", 0, Default::default()).unwrap();
        let b = &state.backend;
        b.execute("INSERT INTO artists (name) VALUES ('Pulp')", &[])
            .unwrap();
        let artiste_id = b.last_insert_rowid();
        b.execute(
            "INSERT INTO albums (title, artist_id, track_count) VALUES ('Live', ?1, 5)",
            &[&artiste_id as &dyn ToSqlValue],
        )
        .unwrap();
        let seul = b.last_insert_rowid();
        ecoute(&state, "Piste inconnue", Some(""), None);

        let Ok(items) = fetch_continue_listening(&state, 10, None) else {
            panic!("la requete doit repondre")
        };

        assert_eq!(items.len(), 1, "albums rendus : {items:?}");
        assert_eq!(items[0]["album_id"].as_i64(), Some(seul));
    }

    /// LIMITE FIGEE, pas un comportement souhaite. `record_listen` ecrit le
    /// titre d'album tel que le fournisseur de flux le rend ; `albums` le
    /// porte tel que le scanner l'a lu des etiquettes. Les favoris
    /// rapprochent « Live » et « LIVE » depuis #1391 ; cette section, non —
    /// elle compare octet a octet et perd l'ecoute.
    ///
    /// Ce n'est pas un oubli : le `LOWER` a ete ecrit, puis MESURE. Sur
    /// 45 000 albums et 5 000 lignes d'historique, `fetch_continue_listening`
    /// passe de **19 ms a 83 662 ms** — l'index
    /// `idx_albums_title ON albums(title COLLATE NOCASE)` ne sert plus la
    /// comparaison. Le rendre gratuit demande un index d'expression aux
    /// quatre endroits du schema, et PostgreSQL n'a pas de `COLLATE NOCASE`.
    /// C'est un chantier de schema, pas la confusion de deux albums (#2731).
    ///
    /// Si ce test se met a echouer, c'est que quelqu'un a rendu la casse
    /// indifferente : verifier d'abord qu'il a paye l'index, sinon l'accueil
    /// met une minute et demie a s'afficher.
    #[test]
    fn la_casse_separe_encore_une_ecoute_de_son_album() {
        let state = AppState::new(":memory:", 0, Default::default()).unwrap();
        let (_police, _pulp) = deux_live(&state, None);
        state
            .backend
            .execute(
                "INSERT INTO listen_history (title, artist_name, album_title, listened_at) \
                 VALUES ('Common People', 'pulp', 'LIVE', '2026-08-28T22:45:00Z')",
                &[],
            )
            .unwrap();

        let Ok(items) = fetch_continue_listening(&state, 10, None) else {
            panic!("la requete doit repondre")
        };

        assert!(
            items.is_empty(),
            "limite connue : « LIVE » ne rejoint pas « Live » — voir le cout \
             mesure sur HISTORIQUE_VERS_ALBUM avant de changer ceci : {items:?}"
        );
    }

    /// Constat de bordure, releve en voulant eprouver le meme defaut du cote
    /// des recommandations : `sql_top_genres` ne rend RIEN, sur les deux
    /// moteurs. `WHERE genre IS NOT NULL` se heurte a `t.genre` et `a.genre`
    /// — « ambiguous column name: genre » — et l'erreur est avalee par le
    /// `unwrap_or_default` de l'appelant. « A decouvrir » tire donc toujours
    /// au hasard, et « top mixes » est toujours vide.
    ///
    /// Consequence pour #2731 : la jointure corrigee dans `sql_top_genres` et
    /// le `NOT EXISTS` des recommandations sont ecrits juste, mais aucun test
    /// ne peut les atteindre tant que cette requete ne s'execute pas. Reveiller
    /// la requete change ce que l'accueil AFFICHE (le hasard cede la place aux
    /// albums d'un genre, qui peuvent etre zero) : c'est un arbitrage produit,
    /// pas le defaut de Tades. Il est laisse hors de ce correctif, et ce test
    /// le fige pour qu'on ne le decouvre pas deux fois.
    #[test]
    fn les_genres_les_plus_ecoutes_ne_rendent_rien_ambiguite_sur_genre() {
        let state = AppState::new(":memory:", 0, Default::default()).unwrap();
        let (_police, pulp) = deux_live(&state, Some("Rock"));
        ecoute(&state, "Common People", Some("Pulp"), Some(pulp));

        assert!(
            state.backend.query_many(&sql_top_genres(), &[]).is_err(),
            "si cette requete se met a repondre, les deux jointures corrigees \
             deviennent testables — et « A decouvrir » cesse d'etre aleatoire"
        );
    }
}

/// #2441 — « Continuer l'ecoute » doit refleter CE QUE l'auditeur a demande.
///
/// FabienM, fil forum 1557 : « si je choisis d'ecouter un titre alors je
/// m'attends a voir ce titre » ; « si je choisis de jouer une playlist
/// complete, je m'attends a voir cette playlist » ; « idem si je decide de
/// jouer un artiste ou un label ».
///
/// CONTRE-EPREUVE — chacun de ces tests devient ROUGE sur la requete d'avant
/// le correctif. Elle partait de `albums` (`JOIN albums a ON ...`,
/// `GROUP BY a.id`) : une playlist, un artiste, un label, un titre isole n'en
/// sortaient JAMAIS, et la section rendait `[]`. Les quatre premiers tests
/// echouent donc sur `items.len() == 1`, et le cinquieme sur `context_type`,
/// ce champ n'ayant jamais existe dans la charge utile.
///
/// Une nature par test, deliberement : un test qui ne couvrirait que l'album
/// laisserait les quatre autres nus — c'est exactement le defaut qu'on corrige.
#[cfg(test)]
mod tests_contextes {
    use super::*;

    /// Une ecoute qui DIT d'ou venait le geste. `rang` a `None` = tirage
    /// aleatoire ou ligne d'avant la migration 94.
    fn ecoute_avec_contexte(
        state: &AppState,
        titre: &str,
        artiste: Option<&str>,
        album: Option<&str>,
        album_id: Option<i64>,
        nature: &str,
        contexte_id: &str,
        rang: Option<i64>,
        quand: &str,
    ) {
        state
            .backend
            .execute(
                "INSERT INTO listen_history \
                 (title, artist_name, album_title, album_id, source, \
                  context_type, context_id, context_position, listened_at) \
                 VALUES (?1, ?2, ?3, ?4, 'local', ?5, ?6, ?7, ?8)",
                &[
                    &titre as &dyn ToSqlValue,
                    &artiste as &dyn ToSqlValue,
                    &album as &dyn ToSqlValue,
                    &album_id as &dyn ToSqlValue,
                    &nature as &dyn ToSqlValue,
                    &contexte_id as &dyn ToSqlValue,
                    &rang as &dyn ToSqlValue,
                    &quand as &dyn ToSqlValue,
                ],
            )
            .unwrap();
    }

    fn poser_album(state: &AppState, artiste: &str, titre: &str, pistes: i64) -> i64 {
        let b = &state.backend;
        b.execute(
            "INSERT INTO artists (name) VALUES (?1)",
            &[&artiste as &dyn ToSqlValue],
        )
        .unwrap();
        let artiste_id = b.last_insert_rowid();
        b.execute(
            "INSERT INTO albums (title, artist_id, track_count) VALUES (?1, ?2, ?3)",
            &[
                &titre as &dyn ToSqlValue,
                &artiste_id as &dyn ToSqlValue,
                &pistes as &dyn ToSqlValue,
            ],
        )
        .unwrap();
        b.last_insert_rowid()
    }

    // --- PLAYLIST ---------------------------------------------------------

    /// « Si je choisis de jouer une playlist complete, je m'attends a voir
    /// cette playlist » — et a la retrouver a la piste ou je l'avais laissee.
    #[test]
    fn une_playlist_remonte_comme_playlist_avec_son_nom_et_son_rang() {
        let state = AppState::new(":memory:", 0, Default::default()).unwrap();
        state
            .backend
            .execute("INSERT INTO playlists (name) VALUES ('Route de nuit')", &[])
            .unwrap();
        let playlist_id = state.backend.last_insert_rowid();
        ecoute_avec_contexte(
            &state,
            "So What",
            Some("Miles Davis"),
            Some("Kind of Blue"),
            None,
            "playlist",
            &playlist_id.to_string(),
            Some(6),
            "2026-08-28T22:45:00Z",
        );

        let Ok(items) = fetch_continue_listening(&state, 10, None) else {
            panic!("la requete doit repondre")
        };

        assert_eq!(
            items.len(),
            1,
            "la playlist n'apparait pas : la section repart-elle de `albums` ? {items:?}"
        );
        assert_eq!(items[0]["context_type"], "playlist");
        assert_eq!(items[0]["context_id"], playlist_id.to_string());
        assert_eq!(
            items[0]["title"], "Route de nuit",
            "c'est le NOM de la playlist qui doit s'afficher, pas le titre de \
             la derniere piste jouee"
        );
        assert_eq!(
            items[0]["position"].as_i64(),
            Some(6),
            "sans le rang, on rouvrirait la bonne playlist a sa premiere piste"
        );
    }

    /// Une playlist ecoutee EN ENTIER reste dans la section. C'est la seconde
    /// decision de l'arbitrage : le filtre « moins de pistes ecoutees que le
    /// total » ne vaut que pour un album. Une playlist n'a aucune notion
    /// d'« incomplet » — la lui appliquer l'aurait fait disparaitre aussitot
    /// ecrite.
    #[test]
    fn une_playlist_finie_reste_dans_la_section() {
        let state = AppState::new(":memory:", 0, Default::default()).unwrap();
        state
            .backend
            .execute("INSERT INTO playlists (name) VALUES ('Courte')", &[])
            .unwrap();
        let playlist_id = state.backend.last_insert_rowid();
        for (n, titre) in ["A", "B", "C"].iter().enumerate() {
            ecoute_avec_contexte(
                &state,
                titre,
                None,
                None,
                None,
                "playlist",
                &playlist_id.to_string(),
                Some(n as i64),
                &format!("2026-08-28T22:4{n}:00Z"),
            );
        }

        let Ok(items) = fetch_continue_listening(&state, 10, None) else {
            panic!("la requete doit repondre")
        };

        assert_eq!(items.len(), 1, "la playlist finie a disparu : {items:?}");
        assert_eq!(items[0]["context_type"], "playlist");
        assert_eq!(
            items[0]["position"].as_i64(),
            Some(2),
            "c'est le rang de la DERNIERE ecoute qui doit etre retenu"
        );
    }

    /// Une playlist de STREAMING n'a pas de ligne dans `playlists` et son nom
    /// n'est cache nulle part en base. LIMITE ASSUMEE : l'entree remonte avec
    /// son type, son identifiant et sa source, mais SANS titre — c'est au
    /// client de la nommer aupres du service. Afficher a la place le titre de
    /// la derniere piste serait un mensonge.
    #[test]
    fn une_playlist_de_streaming_remonte_sans_titre_mais_avec_sa_source() {
        let state = AppState::new(":memory:", 0, Default::default()).unwrap();
        state
            .backend
            .execute(
                "INSERT INTO listen_history \
                 (title, source, context_type, context_id, listened_at) \
                 VALUES ('So What', 'qobuz', 'playlist', 'qb-playlist-99', \
                         '2026-08-28T22:45:00Z')",
                &[],
            )
            .unwrap();

        let Ok(items) = fetch_continue_listening(&state, 10, None) else {
            panic!("la requete doit repondre")
        };

        assert_eq!(items.len(), 1, "l'entree a ete perdue : {items:?}");
        assert_eq!(items[0]["context_type"], "playlist");
        assert_eq!(items[0]["context_id"], "qb-playlist-99");
        assert_eq!(items[0]["source"], "qobuz");
        assert!(
            items[0]["title"].is_null(),
            "pas de nom en base : mieux vaut un titre nul que le titre de la \
             piste presente pour un nom de playlist — {items:?}"
        );
    }

    // --- ARTISTE ----------------------------------------------------------

    /// « Idem si je decide de jouer un artiste ». Le nom affiche est celui de
    /// l'ARTISTE DEMANDE, pas celui de la derniere piste jouee : sur une
    /// compilation les deux different.
    #[test]
    fn un_artiste_remonte_comme_artiste_avec_le_nom_demande() {
        let state = AppState::new(":memory:", 0, Default::default()).unwrap();
        state
            .backend
            .execute("INSERT INTO artists (name) VALUES ('Miles Davis')", &[])
            .unwrap();
        let artiste_id = state.backend.last_insert_rowid();
        ecoute_avec_contexte(
            &state,
            "Sur une compilation",
            Some("Artiste invite"),
            None,
            None,
            "artist",
            &artiste_id.to_string(),
            Some(3),
            "2026-08-28T22:45:00Z",
        );

        let Ok(items) = fetch_continue_listening(&state, 10, None) else {
            panic!("la requete doit repondre")
        };

        assert_eq!(items.len(), 1, "l'artiste n'apparait pas : {items:?}");
        assert_eq!(items[0]["context_type"], "artist");
        assert_eq!(items[0]["title"], "Miles Davis");
        assert_eq!(
            items[0]["artist_name"], "Miles Davis",
            "c'est l'artiste DEMANDE qui compte, pas « Artiste invite » lu sur \
             la derniere ligne d'historique"
        );
        assert_eq!(items[0]["position"].as_i64(), Some(3));
    }

    // --- LABEL ------------------------------------------------------------

    /// « Idem si je decide de jouer [...] un label ». Un label n'a NI table NI
    /// identifiant : l'onglet Labels lit une facette et selectionne par
    /// CHAINE. `context_id` EST donc le nom du label, et il est son propre
    /// titre.
    #[test]
    fn un_label_remonte_comme_label_et_est_son_propre_titre() {
        let state = AppState::new(":memory:", 0, Default::default()).unwrap();
        ecoute_avec_contexte(
            &state,
            "Take Five",
            Some("Dave Brubeck"),
            None,
            None,
            "label",
            "Blue Note",
            Some(11),
            "2026-08-28T22:45:00Z",
        );

        let Ok(items) = fetch_continue_listening(&state, 10, None) else {
            panic!("la requete doit repondre")
        };

        assert_eq!(items.len(), 1, "le label n'apparait pas : {items:?}");
        assert_eq!(items[0]["context_type"], "label");
        assert_eq!(items[0]["context_id"], "Blue Note");
        assert_eq!(items[0]["title"], "Blue Note");
        assert_eq!(items[0]["position"].as_i64(), Some(11));
    }

    // --- TITRE ISOLE ------------------------------------------------------

    /// « Si je choisis d'ecouter un titre alors je m'attends a voir ce titre ».
    /// L'album reste renseigne — il permet de remonter au disque qui porte la
    /// piste — mais ce n'est plus lui qu'on affiche.
    #[test]
    fn un_titre_isole_remonte_comme_titre_et_non_comme_son_album() {
        let state = AppState::new(":memory:", 0, Default::default()).unwrap();
        let album = poser_album(&state, "Pulp", "Different Class", 12);
        ecoute_avec_contexte(
            &state,
            "Common People",
            Some("Pulp"),
            Some("Different Class"),
            Some(album),
            "track",
            "7",
            Some(0),
            "2026-08-28T22:45:00Z",
        );

        let Ok(items) = fetch_continue_listening(&state, 10, None) else {
            panic!("la requete doit repondre")
        };

        assert_eq!(items.len(), 1, "le titre n'apparait pas : {items:?}");
        assert_eq!(items[0]["context_type"], "track");
        assert_eq!(
            items[0]["title"], "Common People",
            "c'est le TITRE qui doit s'afficher, pas « Different Class » — le \
             defaut exact releve par FabienM"
        );
        assert_eq!(
            items[0]["album_id"].as_i64(),
            Some(album),
            "l'album reste connu : le client doit pouvoir remonter au disque"
        );
    }

    // --- ALBUM ------------------------------------------------------------

    /// L'album n'a rien perdu : il garde son avancement, et gagne le rang.
    #[test]
    fn un_album_remonte_avec_son_avancement_et_son_rang() {
        let state = AppState::new(":memory:", 0, Default::default()).unwrap();
        let album = poser_album(&state, "Pulp", "Different Class", 12);
        ecoute_avec_contexte(
            &state,
            "Common People",
            Some("Pulp"),
            Some("Different Class"),
            Some(album),
            "album",
            &album.to_string(),
            Some(4),
            "2026-08-28T22:45:00Z",
        );

        let Ok(items) = fetch_continue_listening(&state, 10, None) else {
            panic!("la requete doit repondre")
        };

        assert_eq!(items.len(), 1, "albums rendus : {items:?}");
        assert_eq!(items[0]["context_type"], "album");
        assert_eq!(items[0]["album_id"].as_i64(), Some(album));
        assert_eq!(items[0]["title"], "Different Class");
        assert_eq!(items[0]["artist_name"], "Pulp");
        assert_eq!(items[0]["listened_tracks"].as_i64(), Some(1));
        assert_eq!(items[0]["track_count"].as_i64(), Some(12));
        assert_eq!(items[0]["position"].as_i64(), Some(4));
    }

    /// L'album, LUI, garde le filtre « pas encore fini » : un disque ecoute en
    /// entier n'a plus rien a « continuer ». C'est la contre-partie de la
    /// seconde decision — le filtre ne DISPARAIT pas, il cesse seulement de
    /// s'appliquer aux natures qui n'ont pas de notion de completude.
    #[test]
    fn un_album_termine_disparait_toujours_de_la_section() {
        let state = AppState::new(":memory:", 0, Default::default()).unwrap();
        let album = poser_album(&state, "Court", "Deux pistes", 2);
        for (n, titre) in ["Une", "Deux"].iter().enumerate() {
            ecoute_avec_contexte(
                &state,
                titre,
                Some("Court"),
                Some("Deux pistes"),
                Some(album),
                "album",
                &album.to_string(),
                Some(n as i64),
                &format!("2026-08-28T22:4{n}:00Z"),
            );
        }

        let Ok(items) = fetch_continue_listening(&state, 10, None) else {
            panic!("la requete doit repondre")
        };

        assert!(
            items.is_empty(),
            "un album ecoute en entier ne se « continue » pas : {items:?}"
        );
    }

    // --- LE SOCLE EXISTANT NE DOIT PAS TOMBER -----------------------------

    /// Une base en service porte des milliers de lignes anterieures a la
    /// migration 84, sans contexte. Si la section ne rendait QUE des
    /// contextes, elle se viderait le jour de la mise a jour — la pire des
    /// regressions : le correctif d'un manque d'affichage produisant un
    /// ecran blanc.
    #[test]
    fn les_ecoutes_sans_contexte_alimentent_toujours_la_section() {
        let state = AppState::new(":memory:", 0, Default::default()).unwrap();
        let album = poser_album(&state, "Pulp", "Different Class", 12);
        state
            .backend
            .execute(
                "INSERT INTO listen_history \
                 (title, artist_name, album_title, album_id, listened_at) \
                 VALUES ('Common People', 'Pulp', 'Different Class', ?1, \
                         '2026-08-28T22:45:00Z')",
                &[&album as &dyn ToSqlValue],
            )
            .unwrap();

        let Ok(items) = fetch_continue_listening(&state, 10, None) else {
            panic!("la requete doit repondre")
        };

        assert_eq!(items.len(), 1, "la section s'est videe : {items:?}");
        assert_eq!(items[0]["album_id"].as_i64(), Some(album));
        assert_eq!(items[0]["context_type"], "album");
        assert!(
            items[0]["position"].is_null(),
            "ces lignes n'ont jamais porte de rang : le declarer serait \
             l'inventer"
        );
    }

    /// Le meme album ecoute AVANT et APRES la mise a jour ne doit pas
    /// apparaitre deux fois : le contexte prime, lui seul porte le rang.
    #[test]
    fn un_album_vu_par_les_deux_chemins_n_apparait_qu_une_fois() {
        let state = AppState::new(":memory:", 0, Default::default()).unwrap();
        let album = poser_album(&state, "Pulp", "Different Class", 12);
        state
            .backend
            .execute(
                "INSERT INTO listen_history \
                 (title, artist_name, album_title, album_id, listened_at) \
                 VALUES ('Common People', 'Pulp', 'Different Class', ?1, \
                         '2026-08-27T10:00:00Z')",
                &[&album as &dyn ToSqlValue],
            )
            .unwrap();
        ecoute_avec_contexte(
            &state,
            "Disco 2000",
            Some("Pulp"),
            Some("Different Class"),
            Some(album),
            "album",
            &album.to_string(),
            Some(2),
            "2026-08-28T22:45:00Z",
        );

        let Ok(items) = fetch_continue_listening(&state, 10, None) else {
            panic!("la requete doit repondre")
        };

        assert_eq!(items.len(), 1, "l'album est rendu deux fois : {items:?}");
        assert_eq!(
            items[0]["position"].as_i64(),
            Some(2),
            "c'est l'entree PORTEUSE DU RANG qui doit survivre au dedoublonnage"
        );
    }

    /// Les cinq natures cote a cote, du geste le plus recent au plus ancien.
    /// L'auditeur relit son histoire, pas « les albums puis le reste ».
    #[test]
    fn les_cinq_natures_cohabitent_et_sortent_du_plus_recent_au_plus_ancien() {
        let state = AppState::new(":memory:", 0, Default::default()).unwrap();
        let album = poser_album(&state, "Pulp", "Different Class", 12);
        state
            .backend
            .execute("INSERT INTO playlists (name) VALUES ('Route de nuit')", &[])
            .unwrap();
        let playlist = state.backend.last_insert_rowid();
        state
            .backend
            .execute("INSERT INTO artists (name) VALUES ('Miles Davis')", &[])
            .unwrap();
        let artiste = state.backend.last_insert_rowid();

        let gestes: [(&str, String, &str); 5] = [
            ("album", album.to_string(), "2026-08-28T22:41:00Z"),
            ("playlist", playlist.to_string(), "2026-08-28T22:42:00Z"),
            ("artist", artiste.to_string(), "2026-08-28T22:43:00Z"),
            ("label", "Blue Note".into(), "2026-08-28T22:44:00Z"),
            ("track", "7".into(), "2026-08-28T22:45:00Z"),
        ];
        for (nature, id, quand) in &gestes {
            ecoute_avec_contexte(
                &state,
                "Common People",
                Some("Pulp"),
                Some("Different Class"),
                Some(album),
                nature,
                id,
                Some(1),
                quand,
            );
        }

        let Ok(items) = fetch_continue_listening(&state, 10, None) else {
            panic!("la requete doit repondre")
        };

        let natures: Vec<&str> = items
            .iter()
            .filter_map(|i| i["context_type"].as_str())
            .collect();
        assert_eq!(
            natures,
            vec!["track", "label", "artist", "playlist", "album"],
            "les cinq natures doivent cohabiter, du plus recent au plus \
             ancien : {items:?}"
        );
    }

    /// La borne de la section reste celle qu'on demande, toutes natures
    /// confondues — la marge interne ne doit pas fuir dans la reponse.
    #[test]
    fn la_limite_demandee_est_respectee_toutes_natures_confondues() {
        let state = AppState::new(":memory:", 0, Default::default()).unwrap();
        for n in 0..9 {
            ecoute_avec_contexte(
                &state,
                "Take Five",
                None,
                None,
                None,
                "label",
                &format!("Label {n}"),
                Some(0),
                &format!("2026-08-28T22:4{n}:00Z"),
            );
        }

        let Ok(items) = fetch_continue_listening(&state, 3, None) else {
            panic!("la requete doit repondre")
        };

        assert_eq!(items.len(), 3, "limite non respectee : {items:?}");
    }
}

/// Les albums entres dans la bibliotheque au cours des `days` derniers jours.
///
/// `days` va de 1 a [`FENETRE_JOURS_MAX`], defaut [`FENETRE_JOURS_DEFAUT`] ;
/// hors bornes, 400 et le message dit la borne (#3039).
///
/// La forme de la reponse — un TABLEAU d'albums — ne change pas : les clients
/// deployes la lisent telle quelle. Les nombres du sous-titre vivent a cote,
/// dans [`recently_added_summary`].
async fn recently_added(
    State(state): State<AppState>,
    Query(p): Query<RecentlyAddedParams>,
) -> Result<Json<Value>, AppError> {
    let limit = p.limit.unwrap_or(20);
    let depuis = borne_basse_de_fenetre(p.days)?;
    let items = fetch_recently_added(&state, limit, depuis)?;
    Ok(Json(json!(items)))
}

/// Le sous-titre de l'onglet : combien d'albums, combien de pistes, combien de
/// temps, sur la MEME fenetre que [`recently_added`] (#3039).
///
/// Route separee, et non un champ de plus dans la reponse ci-dessus : passer
/// le tableau en objet casserait tout client deja deploye. `limit` n'est pas
/// lu ici — on compte la fenetre, pas la page.
async fn recently_added_summary(
    State(state): State<AppState>,
    Query(p): Query<RecentlyAddedParams>,
) -> Result<Json<Value>, AppError> {
    let depuis = borne_basse_de_fenetre(p.days)?;
    let engine = state.backend.engine();
    let sql = home_queries::recently_added_totaux(engine);
    let params: [&dyn ToSqlValue; 1] = [&depuis];
    let rows = state
        .backend
        .query_many(&sql, &params)
        .ou_defaut_journalise();
    let cols = rows.first();
    let nombre = |i: usize| cols.and_then(|c: &Vec<_>| c.get(i)?.as_i64()).unwrap_or(0);
    let duree_ms = nombre(2);
    Ok(Json(json!({
        "days": p.days.unwrap_or(FENETRE_JOURS_DEFAUT),
        "album_count": nombre(0),
        "track_count": nombre(1),
        "duration_ms": duree_ms,
        // Les secondes en plus des millisecondes : c'est la seule des trois
        // valeurs que l'ecran affiche telle quelle (« 5 h 55 min »), et la
        // division n'a pas a etre refaite par chaque client.
        "duration_seconds": duree_ms / 1000,
    })))
}

/// « Ajoutes recemment ».
///
/// GROUP BY exhaustif, et non `GROUP BY a.id` : jumelle exacte du defaut de
/// « Continuer l'ecoute » (#2860). `ar.name` vient de `artists`, que la
/// dependance fonctionnelle de PostgreSQL sur `albums.id` ne couvre pas —
/// `column "ar.name" must appear in the GROUP BY clause or be used in an
/// aggregate function`, avalee par le `unwrap_or_default()` plus bas. Cette
/// section-la etait donc vide, elle aussi, sur toute installation PostgreSQL.
///
/// `depuis` est la borne basse de la fenetre, en secondes epoch : elle vient
/// de [`borne_basse_de_fenetre`], qui l'a deja bornee. Elle etait auparavant
/// recalculee ici a 7 jours en dur, hors d'atteinte de tout appelant (#3039).
fn fetch_recently_added(state: &AppState, limit: i64, depuis: f64) -> Result<Vec<Value>, AppError> {
    let engine = state.backend.engine();
    let sql = home_queries::recently_added(engine);
    let params: [&dyn ToSqlValue; 2] = [&depuis, &limit];
    let rows = state
        .backend
        .query_many(&sql, &params)
        .ou_defaut_journalise();
    Ok(rows
        .iter()
        .map(|cols| {
            json!({
                "id": cols.get(0).and_then(|v| v.as_i64()).unwrap_or(0),
                "title": cols.get(1).and_then(|v| v.as_string()).unwrap_or_default(),
                "artist_name": cols.get(2).and_then(|v| v.as_string()),
                "year": cols.get(3).and_then(|v| v.as_i64()),
                "cover_path": cols.get(4).and_then(|v| v.as_string()),
                "genre": cols.get(5).and_then(|v| v.as_string()),
                "format": cols.get(6).and_then(|v| v.as_string()),
                "sample_rate": cols.get(7).and_then(|v| v.as_i64()),
                "bit_depth": cols.get(8).and_then(|v| v.as_i64()),
                "track_count": cols.get(9).and_then(|v| v.as_i64()),
                // `added_at`, et non plus `added_mtime` : la valeur n'est plus
                // un `mtime` de fichier mais la premiere vue par le scan quand
                // elle existe (#3039). L'ancien nom est conserve a cote le
                // temps que les clients migrent — il porte la meme valeur.
                "added_at": cols.get(10).and_then(|v| v.as_f64()),
                "added_mtime": cols.get(10).and_then(|v| v.as_f64()),
            })
        })
        .collect())
}

/// Recommendations based on listening history: find most-played genres/artists,
/// suggest albums from the same genres that haven't been listened to yet.
async fn home_recommendations(
    State(state): State<AppState>,
    Query(p): Query<HomeParams>,
) -> Result<Json<Value>, AppError> {
    let limit = p.limit.unwrap_or(20);
    let items = fetch_recommendations(&state, limit)?;
    Ok(Json(json!(items)))
}

fn fetch_recommendations(state: &AppState, limit: i64) -> Result<Vec<Value>, AppError> {
    let engine = state.backend.engine();

    // Find top genres from listen history
    let top_genres: Vec<String> = state
        .backend
        .query_many(&sql_top_genres(), &[])
        .ou_defaut_journalise()
        .into_iter()
        .filter_map(|cols| cols.into_iter().next().and_then(|v| v.as_string()))
        .collect();

    if top_genres.is_empty() {
        // Fallback: return random albums
        let p1 = ph(engine, 1);
        let sql = format!(
            "SELECT a.id, a.title, ar.name, a.year, a.cover_path, a.genre \
                   FROM albums a LEFT JOIN artists ar ON a.artist_id = ar.id \
                   ORDER BY RANDOM() LIMIT {p1}"
        );
        let params: [&dyn ToSqlValue; 1] = [&limit];
        let rows = state
            .backend
            .query_many(&sql, &params)
            .ou_defaut_journalise();
        return Ok(rows
            .iter()
            .map(|cols| {
                json!({
                    "id": cols.get(0).and_then(|v| v.as_i64()).unwrap_or(0),
                    "title": cols.get(1).and_then(|v| v.as_string()).unwrap_or_default(),
                    "artist_name": cols.get(2).and_then(|v| v.as_string()),
                    "year": cols.get(3).and_then(|v| v.as_i64()),
                    "cover_path": cols.get(4).and_then(|v| v.as_string()),
                    "genre": cols.get(5).and_then(|v| v.as_string()),
                    "reason": "random",
                })
            })
            .collect());
    }

    // Find albums matching top genres that the user hasn't listened to.
    // Build engine-specific placeholders for the IN clause.
    let genre_placeholders: String = top_genres
        .iter()
        .enumerate()
        .map(|(i, _)| ph(engine, i + 1))
        .collect::<Vec<_>>()
        .join(",");
    let limit_ph = ph(engine, top_genres.len() + 1);
    let sql = format!(
        "SELECT a.id, a.title, ar.name, a.year, a.cover_path, a.genre \
         FROM albums a \
         LEFT JOIN artists ar ON a.artist_id = ar.id \
         WHERE a.genre IN ({genre_placeholders}) \
           AND NOT EXISTS (SELECT 1 FROM listen_history lh \
                           WHERE {HISTORIQUE_VERS_ALBUM}) \
         ORDER BY RANDOM() \
         LIMIT {limit_ph}"
    );

    // Build a Vec of owned SqlValue-able params: genres + limit.
    let mut param_vals: Vec<Box<dyn ToSqlValue>> = top_genres
        .iter()
        .map(|g| Box::new(g.clone()) as Box<dyn ToSqlValue>)
        .collect();
    param_vals.push(Box::new(limit));
    let param_refs: Vec<&dyn ToSqlValue> = param_vals.iter().map(|p| p.as_ref()).collect();

    let rows = state
        .backend
        .query_many(&sql, &param_refs)
        .ou_defaut_journalise();
    Ok(rows
        .iter()
        .map(|cols| {
            json!({
                "id": cols.get(0).and_then(|v| v.as_i64()).unwrap_or(0),
                "title": cols.get(1).and_then(|v| v.as_string()).unwrap_or_default(),
                "artist_name": cols.get(2).and_then(|v| v.as_string()),
                "year": cols.get(3).and_then(|v| v.as_i64()),
                "cover_path": cols.get(4).and_then(|v| v.as_string()),
                "genre": cols.get(5).and_then(|v| v.as_string()),
                "reason": "genre_match",
            })
        })
        .collect())
}

/// Auto-generated "mixes" by genre from top genres in history.
/// Each mix = playlist of 20 tracks from that genre.
async fn top_mixes(State(state): State<AppState>) -> Result<Json<Value>, AppError> {
    let engine = state.backend.engine();

    // Get top 5 genres from history
    let top_genres: Vec<(String, i64)> = state
        .backend
        .query_many(&sql_top_genres(), &[])
        .ou_defaut_journalise()
        .into_iter()
        .filter_map(|cols| {
            let genre = cols.first()?.as_string()?;
            let cnt = cols.get(1).and_then(|v| v.as_i64()).unwrap_or(0);
            Some((genre, cnt))
        })
        .collect();

    let p1 = ph(engine, 1);
    let p2 = ph(engine, 2);
    let tracks_sql = format!(
        "SELECT t.id, t.title, ar.name, al.title, \
                CAST(t.duration_ms AS BIGINT), al.cover_path \
         FROM tracks t \
         LEFT JOIN albums al ON t.album_id = al.id \
         LEFT JOIN artists ar ON t.artist_id = ar.id \
         WHERE t.genre = {p1} OR al.genre = {p2} \
         ORDER BY RANDOM() LIMIT 20"
    );

    let mixes: Vec<Value> = top_genres
        .into_iter()
        .filter_map(|(genre, play_count)| {
            let params: [&dyn ToSqlValue; 2] = [&genre, &genre];
            let tracks: Vec<Value> = state
                .backend
                .query_many(&tracks_sql, &params)
                .ou_defaut_journalise()
                .iter()
                .map(|cols| {
                    json!({
                        "id": cols.get(0).and_then(|v| v.as_i64()).unwrap_or(0),
                        "title": cols.get(1).and_then(|v| v.as_string()).unwrap_or_default(),
                        "artist_name": cols.get(2).and_then(|v| v.as_string()),
                        "album_title": cols.get(3).and_then(|v| v.as_string()),
                        "duration_ms": cols.get(4).and_then(|v| v.as_i64()).unwrap_or(0),
                        "cover_path": cols.get(5).and_then(|v| v.as_string()),
                    })
                })
                .collect();

            if tracks.is_empty() {
                return None;
            }

            Some(json!({
                "genre": genre,
                "title": format!("Mix {}", genre),
                "play_count": play_count,
                "track_count": tracks.len(),
                "tracks": tracks,
            }))
        })
        .collect();

    Ok(Json(json!(mixes)))
}

/// Albums most recently added to the library, newest first.
///
/// Grouped by ALBUM, not by track. Returning tracks meant a freshly imported
/// 15-track record filled half the row with the same cover — "'New in your
/// library' can sometimes show 10-20 tracks from the same album" (Alex
/// Campbell, 9 Aug 2026).
///
/// The shape is what the home carousel has always assumed: it calls
/// `playAlbum(item.id)` and `navigateToAlbum(item.id)` and reads
/// `item.artist_id`. Sending tracks meant `id` was a TRACK id and `title` a
/// track title, so the covers were labelled with song names and clicking one
/// navigated by an id that means something else entirely. No client change is
/// needed — the server now sends what the client was already reading.
///
/// Tracks with no album are left out rather than shown as one-track entries:
/// this row is about records landing in the library, and a loose file has no
/// album to open.
async fn new_in_library(
    State(state): State<AppState>,
    Query(p): Query<HomeParams>,
) -> Result<Json<Value>, AppError> {
    let limit = p.limit.unwrap_or(30);
    let engine = state.backend.engine();
    let p1 = ph(engine, 1);
    // MAX(file_mtime) dates an album by its most recently imported track, so a
    // record whose files arrived together stays together in the ordering.
    let sql = format!(
        "SELECT al.id, al.title, al.artist_id, ar.name, al.cover_path, al.source, \
                MAX(t.file_mtime) AS newest \
        FROM tracks t \
        JOIN albums al ON t.album_id = al.id \
        LEFT JOIN artists ar ON al.artist_id = ar.id \
        WHERE t.file_mtime IS NOT NULL \
        GROUP BY al.id, al.title, al.artist_id, ar.name, al.cover_path, al.source \
        ORDER BY newest DESC \
        LIMIT {p1}"
    );
    let params: [&dyn ToSqlValue; 1] = [&limit];
    let items: Vec<Value> = state
        .backend
        .query_many(&sql, &params)
        .ou_defaut_journalise()
        .iter()
        .map(|cols| {
            json!({
                "id": cols.get(0).and_then(|v| v.as_i64()).unwrap_or(0),
                "title": cols.get(1).and_then(|v| v.as_string()).unwrap_or_default(),
                "artist_id": cols.get(2).and_then(|v| v.as_i64()),
                "artist_name": cols.get(3).and_then(|v| v.as_string()),
                "cover_path": cols.get(4).and_then(|v| v.as_string()),
                "source": cols.get(5).and_then(|v| v.as_string()),
                "file_mtime": cols.get(6).and_then(|v| v.as_f64()),
            })
        })
        .collect();
    Ok(Json(json!(items)))
}

/// Les ecoutes recentes examinees pour la recherche STREAMING. Chacune coute
/// une recherche par service connecte (moins le cache) : ce nombre est le
/// budget reseau de la section, pas un choix d'affichage.
const ECOUTES_STREAMING: usize = 6;

/// `GET /home/other-versions` — les autres versions, DANS LA BIBLIOTHEQUE, des
/// morceaux ecoutes RECEMMENT.
///
/// ## Pourquoi les N dernieres ecoutes, et non « aujourd'hui »
///
/// La premiere version bornait sur le jour CIVIL, en UTC. Deux defauts, vus
/// des la mise en service :
///
/// 1. **Minuit UTC coupe la soiree.** A 10 h du matin en France, tout ce qui
///    a ete ecoute la veille apres 2 h — donc toute la soiree — etait deja
///    hors fenetre. Le jour civil de l'utilisateur ne commence pas a la meme
///    heure que celui du serveur, et le fuseau du navigateur n'arrive pas
///    jusqu'ici.
/// 2. **Un jour ordinaire ne contient pas assez de matiere.** Mesure sur une
///    bibliotheque reelle : UNE ecoute dans la fenetre, et donc une section
///    vide la plupart du temps.
///
/// J'avais justifie l'UTC en invoquant le correctif des horaires de favoris
/// radio (#2179). C'etait un mauvais raisonnement : ce defaut-la portait sur
/// l'AFFICHAGE d'un horodatage, pas sur la definition d'une journee.
///
/// Les N dernieres ecoutes n'ont ni fuseau ni bord de journee. La fenetre ne
/// glisse pas, ne depend d'aucune horloge, et contient toujours de la matiere.
///
/// Le cas concret : on ecoute « Ordinary World » depuis The Wedding Album, et
/// on possede aussi la version acoustique sur une compilation. Rien ne le dit
/// aujourd'hui — il faut chercher le titre a la main pour s'en apercevoir.
///
/// ## Ce que cette route fait, et ce qu'elle ne fait PAS
///
/// Elle rapproche **titre + artiste**, et ne retient que les pistes d'un
/// **autre album** que celui ecoute. C'est volontairement etroit :
///
/// - pas de reprises par un autre interprete (« Comme d'habitude » / « My Way ») :
///   cela demande les relations d'oeuvre de MusicBrainz, donc un MBID, et la
///   couverture MBID de la bibliotheque est encore trop faible pour que le
///   resultat soit autre chose qu'un hasard ;
/// - les versions des services de streaming sont cherchees sur un vivier plus
///   petit et mises en cache six heures, afin de borner les appels distants.
///
/// Le rapprochement local est insensible a la casse et strict sur le coeur du
/// titre : seul un suffixe d'edition delimite par ` (` ou ` [` est admis. Il
/// ne s'agit pas d'une recherche floue.
async fn other_versions(
    State(state): State<AppState>,
    Query(p): Query<HomeParams>,
) -> Result<Json<Value>, AppError> {
    // Plafond borne cote serveur : ce nombre part dans le SQL, il ne doit pas
    // venir tel quel de l'URL.
    let limit = p.limit.unwrap_or(20).clamp(1, 100);
    // Le vivier d'ecoutes examine. Large devant `limit` : beaucoup de morceaux
    // n'ont aucune autre version, il en faut donc bien plus que de groupes
    // souhaites pour en remplir quelques-uns.
    const ECOUTES_EXAMINEES: usize = 200;

    // `listened_at` est ordonne comme chaine (ISO-8601), donc `ORDER BY` suffit
    // pour prendre les dernieres : aucun cast de date, donc aucun ecart entre
    // SQLite et PostgreSQL.
    // Le rapprochement lui-meme est ecrit UNE fois, dans
    // `routes::versions` : la route par piste (#2372) applique exactement
    // la meme regle a un vivier different.
    let predicat = crate::routes::versions::predicat_rapprochement(
        "lh.title",
        "lh.artist_name",
        "lh.album_title",
    );
    // ── Pourquoi un `GROUP BY` et non le `SELECT DISTINCT` d'avant (#3181) ──
    //
    // La forme precedente triait sur `lh.listened_at` SANS le selectionner :
    // PostgreSQL la refuse — « for SELECT DISTINCT, ORDER BY expressions must
    // appear in select list » — et la section partait VIDE chez tout
    // utilisateur PG, l'echec avale par `ou_defaut_journalise`.
    //
    // Ajouter `listened_at` a la liste de selection aurait rendu la requete
    // legale et le resultat FAUX : le `DISTINCT` ne dedoublonnait plus la meme
    // chose. Il ne portait de fait que sur (titre, artiste, album, `t.id`) —
    // les quatre autres colonnes sont fonctionnellement determinees par
    // `t.id` — et son role etait d'effondrer les REECOUTES du meme morceau,
    // qui ne different que par `listened_at`. Le rendre distinct fait
    // reapparaitre une ligne par ecoute : mesure sur trois lignes d'historique
    // dont deux reecoutes, 3 lignes au lieu de 2, la meme version listee deux
    // fois, et le `LIMIT` mange par les repetitions.
    //
    // Le dedoublonnage descend donc dans le vivier, AVANT la jointure, avec le
    // patron deja utilise seize lignes plus bas par `sql_recentes` :
    // `GROUP BY` sur les trois colonnes du `DISTINCT`, `MAX(listened_at)` pour
    // la cle de tri. Meme ensemble qu'avant sur les deux moteurs, et plus
    // aucun `DISTINCT` a satisfaire — `lh` est unique sur ses trois colonnes,
    // `t.id` est unique, et `al`/`ar`/`ar2` se joignent par cle primaire.
    //
    // `MAX(listened_at)` est ici la DERNIERE ecoute (chaine ISO-8601, cf.
    // ci-dessus) : c'est ce que « les dernieres ecoutes » veut dire, et c'est
    // desormais defini, la ou le `DISTINCT` laissait le moteur choisir
    // n'importe laquelle des valeurs effondrees.
    //
    // `t.id` en second critere de tri n'est pas un ornement : une meme ecoute
    // se deplie en PLUSIEURS versions qui partagent toutes son `listened_at`,
    // donc sans departage l'ordre de ces lignes — et ce que le `LIMIT`
    // retient — depend du plan de chaque moteur. C'est la condition pour que
    // SQLite et PostgreSQL rendent le meme ensemble DANS LE MEME ORDRE.
    let sql = format!(
        "SELECT lh.title, lh.artist_name, lh.album_title, \
                t.id, al.id, al.title, al.cover_path, t.duration_ms \
        FROM (SELECT title, artist_name, album_title, MAX(listened_at) AS listened_at \
              FROM (SELECT title, artist_name, album_title, listened_at \
                    FROM listen_history \
                    WHERE artist_name IS NOT NULL \
                    ORDER BY listened_at DESC \
                    LIMIT {ECOUTES_EXAMINEES}) le \
              GROUP BY title, artist_name, album_title) lh \
        CROSS JOIN tracks t \
        JOIN albums al ON t.album_id = al.id \
        LEFT JOIN artists ar ON al.artist_id = ar.id \
        LEFT JOIN artists ar2 ON t.artist_id = ar2.id \
        WHERE {predicat} \
        ORDER BY lh.listened_at DESC, t.id \
        LIMIT {limit}"
    );

    // Une piste ecoutee, ses autres versions : on regroupe cote serveur pour
    // que l'ecran n'ait pas a le refaire (et a le refaire differemment sur
    // chacun des trois clients).
    let mut groupes: Vec<Value> = Vec::new();
    for cols in state.backend.query_many(&sql, &[]).ou_defaut_journalise() {
        let titre = cols.first().and_then(|v| v.as_string()).unwrap_or_default();
        let artiste = cols.get(1).and_then(|v| v.as_string()).unwrap_or_default();
        let joue = cols.get(2).and_then(|v| v.as_string()).unwrap_or_default();
        let version = json!({
            "track_id": cols.get(3).and_then(|v| v.as_i64()),
            "album_id": cols.get(4).and_then(|v| v.as_i64()),
            "album_title": cols.get(5).and_then(|v| v.as_string()),
            "cover_path": cols.get(6).and_then(|v| v.as_string()),
            "duration_ms": cols.get(7).and_then(|v| v.as_i64()),
        });
        match groupes.iter_mut().find(|g| {
            g["title"].as_str() == Some(titre.as_str())
                && g["artist_name"].as_str() == Some(artiste.as_str())
        }) {
            Some(g) => {
                if let Some(arr) = g["versions"].as_array_mut() {
                    arr.push(version);
                }
            }
            None => groupes.push(json!({
                "title": titre,
                "artist_name": artiste,
                "played_album": joue,
                "versions": [version],
            })),
        }
    }

    // ── Les versions et reprises DISPONIBLES EN STREAMING ──
    //
    // La doc de cette route promettait ce branchement « quand la section
    // aurait fait ses preuves en local » : c'est demande explicitement
    // maintenant. Budget borne : les ECOUTES_STREAMING dernieres ecoutes
    // distinctes, UNE recherche par service et par titre, cache six heures.
    // Les N derniers TITRES distincts — pas les N dernières lignes. Trois
    // réécoutes du même morceau mangeaient tout le budget : sur un accueil
    // réel, un seul groupe sur sept avait sa recherche streaming, et
    // « Billie Jean » — écoutée juste avant — n'en avait aucune (25/08).
    let sql_recentes = format!(
        "SELECT title, artist_name, MAX(COALESCE(album_title, '')) FROM (SELECT title, artist_name, album_title, listened_at FROM listen_history WHERE artist_name IS NOT NULL ORDER BY listened_at DESC LIMIT 200) le GROUP BY title, artist_name ORDER BY MAX(listened_at) DESC LIMIT {ECOUTES_STREAMING}"
    );
    let recentes: Vec<(String, String, String)> = state
        .backend
        .query_many(&sql_recentes, &[])
        .ou_defaut_journalise()
        .into_iter()
        .filter_map(|cols| {
            Some((
                cols.first().and_then(|v| v.as_string())?,
                cols.get(1).and_then(|v| v.as_string())?,
                cols.get(2).and_then(|v| v.as_string()).unwrap_or_default(),
            ))
        })
        .collect();

    for (titre, artiste, album) in recentes {
        let trouvees =
            crate::routes::versions::versions_streaming(&state, &titre, &artiste, &album).await;
        if trouvees.is_empty() {
            continue;
        }
        match groupes.iter_mut().find(|g| {
            g["title"]
                .as_str()
                .is_some_and(|t| t.eq_ignore_ascii_case(&titre))
                && g["artist_name"]
                    .as_str()
                    .is_some_and(|a| a.eq_ignore_ascii_case(&artiste))
        }) {
            Some(g) => g["streaming"] = json!(trouvees),
            // Un morceau sans autre version LOCALE forme quand meme un groupe
            // si le streaming en a : c'est le cas « Billie Jean » — aucune
            // autre version possedee, des dizaines disponibles.
            None => groupes.push(json!({
                "title": titre,
                "artist_name": artiste,
                "played_album": album,
                "versions": [],
                "streaming": trouvees,
            })),
        }
    }

    Ok(Json(json!(groupes)))
}

#[cfg(test)]
mod tests_other_versions {
    use super::*;

    /// Le second appelant du predicat partage (#2638) doit accepter la meme
    /// variante de titre que la route par piste, sans perdre l'artiste reel
    /// d'une piste rangee dans une compilation « Artistes divers ».
    #[tokio::test]
    async fn accueil_retrouve_une_edition_suffixee_du_titre_ecoute() {
        let state = AppState::new(":memory:", 0, Default::default()).unwrap();
        let b = &state.backend;

        b.execute("INSERT INTO artists (name) VALUES ('Kate Bush')", &[])
            .unwrap();
        let kate = b.last_insert_rowid();
        b.execute("INSERT INTO artists (name) VALUES ('Artistes divers')", &[])
            .unwrap();
        let divers = b.last_insert_rowid();
        b.execute(
            "INSERT INTO albums (title, artist_id) VALUES ('Hit Collection', ?1)",
            &[&divers as &dyn ToSqlValue],
        )
        .unwrap();
        b.execute(
            "INSERT INTO albums (title, artist_id) VALUES ('Before The Dawn', ?1)",
            &[&kate as &dyn ToSqlValue],
        )
        .unwrap();
        let before = b.last_insert_rowid();
        b.execute(
            "INSERT INTO tracks (title, album_id, artist_id, duration_ms, file_path) \
             VALUES ('Running Up That Hill (A Deal With God)', ?1, ?2, 296000, '/before.flac')",
            &[&before as &dyn ToSqlValue, &kate as &dyn ToSqlValue],
        )
        .unwrap();
        b.execute(
            "INSERT INTO listen_history \
             (title, artist_name, album_title, listened_at) \
             VALUES ('Running Up that Hill', 'Kate Bush', 'Hit Collection', \
                     '2026-08-28T09:32:00Z')",
            &[],
        )
        .unwrap();

        let resultat = other_versions(
            State(state),
            Query(HomeParams {
                limit: Some(20),
                zone_id: None,
            }),
        )
        .await;
        let Json(groupes) = match resultat {
            Ok(reponse) => reponse,
            Err(_) => panic!("la route doit repondre"),
        };

        let groupes = groupes.as_array().expect("groupes de versions");
        assert_eq!(groupes.len(), 1, "groupes rendus : {groupes:?}");
        assert_eq!(
            groupes[0]["versions"][0]["album_title"].as_str(),
            Some("Before The Dawn")
        );
    }

    /// Trois REECOUTES du meme morceau ne proposent qu'UNE fois sa version
    /// (#3181).
    ///
    /// C'est le travail que faisait le `SELECT DISTINCT`, et c'est ce qu'on
    /// perd en le rendant simplement legal sur PostgreSQL : ajouter
    /// `listened_at` a la liste de selection rend trois lignes la ou il en
    /// faut une, la meme version apparait trois fois dans le groupe, et le
    /// `LIMIT` se remplit de repetitions. Ce garde est ecrit du cote SQLite —
    /// le moteur qui acceptait l'ancienne forme — pour qu'une correction
    /// PostgreSQL ne puisse pas payer sa legalite avec ce dedoublonnage.
    #[tokio::test]
    async fn autres_versions_effondre_les_reecoutes_du_meme_morceau() {
        let state = AppState::new(":memory:", 0, Default::default()).unwrap();
        let b = &state.backend;

        b.execute("INSERT INTO artists (name) VALUES ('Kate Bush')", &[])
            .unwrap();
        let kate = b.last_insert_rowid();
        b.execute(
            "INSERT INTO albums (title, artist_id) VALUES ('Before The Dawn', ?1)",
            &[&kate as &dyn ToSqlValue],
        )
        .unwrap();
        let before = b.last_insert_rowid();
        b.execute(
            "INSERT INTO tracks (title, album_id, artist_id, duration_ms, file_path) \
             VALUES ('Running Up That Hill', ?1, ?2, 296000, '/i3181-before.flac')",
            &[&before as &dyn ToSqlValue, &kate as &dyn ToSqlValue],
        )
        .unwrap();
        // Trois ecoutes du meme morceau depuis une compilation : trois lignes
        // d'historique qui ne different QUE par `listened_at`.
        for horodatage in [
            "2026-08-28T09:32:00Z",
            "2026-08-29T18:04:00Z",
            "2026-08-30T07:11:00Z",
        ] {
            b.execute(
                "INSERT INTO listen_history \
                 (title, artist_name, album_title, listened_at) \
                 VALUES ('Running Up That Hill', 'Kate Bush', 'Hit Collection', ?1)",
                &[&horodatage as &dyn ToSqlValue],
            )
            .unwrap();
        }

        let resultat = other_versions(
            State(state),
            Query(HomeParams {
                limit: Some(20),
                zone_id: None,
            }),
        )
        .await;
        // `AppError` n'implemente pas `Debug` : un `expect` ne compile pas.
        let Json(groupes) = match resultat {
            Ok(reponse) => reponse,
            Err(_) => panic!("la route doit repondre"),
        };

        let groupes = groupes.as_array().expect("groupes de versions");
        assert_eq!(groupes.len(), 1, "groupes rendus : {groupes:?}");
        let versions = groupes[0]["versions"]
            .as_array()
            .expect("versions du groupe");
        assert_eq!(
            versions.len(),
            1,
            "trois reecoutes ont produit {} versions au lieu d'une : {versions:?}",
            versions.len()
        );
        assert_eq!(versions[0]["album_title"].as_str(), Some("Before The Dawn"));
    }
}

/// Favorite radios + recently played radios.
async fn radio_picks(State(state): State<AppState>) -> Result<Json<Value>, AppError> {
    let items = fetch_radio_picks(&state)?;
    Ok(Json(json!(items)))
}

fn fetch_radio_picks(state: &AppState) -> Result<Vec<Value>, AppError> {
    let repo = RadioRepo::with_backend(state.backend.clone());

    let mut items: Vec<Value> = repo
        .favorites()
        .unwrap_or_default()
        .into_iter()
        .map(|r| json!(r))
        .collect();

    // `is_favorite = '0'` et non `= 0` (#3181). La colonne n'a pas le meme type
    // selon l'origine de la base : `SMALLINT` sur une installation PostgreSQL
    // neuve (`migrations/postgres/005_additional_tables.sql`), mais `TEXT` sur
    // une base VENUE DE SQLITE (`pg_migrate.rs` la declare `is_favorite TEXT
    // DEFAULT 0`, et aucune des migrations numeriques 010/011/013 ne la
    // reconvertit — elles ne reprennent que `bitrate` et `play_count`). Sur ce
    // second chemin, `is_favorite = 0` leve « operator does not exist: text =
    // integer » et la section « radios recemment ecoutees » partait vide, sans
    // rien dire. Le litteral QUOTE, lui, reste non type a l'analyse et se
    // resout dans les deux cas : `text = text` ici, `smallint = smallint` la.
    // C'est deja l'idiome du depot — `RadioRepo::sql::favorites()` ecrit
    // `WHERE is_favorite = '1'` — et sur SQLite l'affinite INTEGER de la
    // colonne convertit `'0'` en `0` : meme lignes qu'avant, NULL exclus des
    // deux cotes comme avant.
    let recent: Vec<Value> = state
        .backend
        .query_many(
            "SELECT id, name, url, logo_url, genre, last_played, play_count \
             FROM radio_stations \
             WHERE is_favorite = '0' AND last_played IS NOT NULL \
             ORDER BY last_played DESC LIMIT 10",
            &[],
        )
        .ou_defaut_journalise()
        .iter()
        .map(|cols| {
            json!({
                "id": cols.get(0).and_then(|v| v.as_i64()).unwrap_or(0),
                "name": cols.get(1).and_then(|v| v.as_string()).unwrap_or_default(),
                "url": cols.get(2).and_then(|v| v.as_string()).unwrap_or_default(),
                "logo_url": cols.get(3).and_then(|v| v.as_string()),
                "genre": cols.get(4).and_then(|v| v.as_string()),
                "last_played": cols.get(5).and_then(|v| v.as_string()),
                "play_count": cols.get(6).and_then(|v| v.as_i64()).unwrap_or(0),
                "is_favorite": false,
            })
        })
        .collect();

    items.extend(recent);
    Ok(items)
}

#[cfg(test)]
mod tests_3181_radios_recentes {
    use super::*;

    /// « Radios recemment ecoutees » : les non-favorites datees, et elles
    /// seules (#3181).
    ///
    /// Le litteral quote `= '0'` doit selectionner EXACTEMENT ce que `= 0`
    /// selectionnait sur SQLite : la favorite est ecartee du bloc « recentes »
    /// (elle arrive par `favorites()`, pas ici), et une station jamais jouee
    /// n'y figure pas. Les stations semees par les migrations ont toutes
    /// `last_played` NUL, donc aucune ne s'invite.
    #[test]
    fn radios_recentes_gardent_les_non_favorites_datees() {
        let state = AppState::new(":memory:", 0, Default::default()).unwrap();
        let b = &state.backend;

        b.execute(
            "INSERT INTO radio_stations (name, url, is_favorite, last_played) \
             VALUES ('i3181 Favorite', 'http://i3181/fav', 1, '2026-09-01T10:00:00Z')",
            &[],
        )
        .unwrap();
        b.execute(
            "INSERT INTO radio_stations (name, url, is_favorite, last_played) \
             VALUES ('i3181 Recente', 'http://i3181/rec', 0, '2026-09-02T10:00:00Z')",
            &[],
        )
        .unwrap();
        b.execute(
            "INSERT INTO radio_stations (name, url, is_favorite, last_played) \
             VALUES ('i3181 Jamais jouee', 'http://i3181/jamais', 0, NULL)",
            &[],
        )
        .unwrap();

        // `AppError` n'implemente pas `Debug` : un `expect` ne compile pas.
        let items = match fetch_radio_picks(&state) {
            Ok(items) => items,
            Err(_) => panic!("la section doit repondre"),
        };

        // Le bloc « recentes » est celui dont `is_favorite` est rendu `false`
        // par le gestionnaire ; `favorites()` rend la station complete.
        let recentes: Vec<&str> = items
            .iter()
            .filter(|i| i["is_favorite"].as_bool() == Some(false))
            .filter_map(|i| i["name"].as_str())
            .collect();

        assert_eq!(
            recentes,
            vec!["i3181 Recente"],
            "radios recemment ecoutees rendues : {recentes:?}"
        );
        // Temoin : la favorite est bien servie, par l'autre moitie.
        assert!(
            items
                .iter()
                .any(|i| i["name"].as_str() == Some("i3181 Favorite")),
            "la radio favorite a disparu de la section : {items:?}"
        );
    }
}

fn fetch_top_tracks(state: &AppState, limit: i64) -> Vec<Value> {
    let repo = HistoryRepo::with_backend(state.backend.clone());
    repo.top_tracks(limit).unwrap_or_default()
}

/// If Tidal/Qobuz authenticated, fetch their featured/new-releases.
async fn streaming_highlights(State(state): State<AppState>) -> Json<Value> {
    let registry = state.services.lock().await;
    let statuses = registry.status_all().await;
    drop(registry);

    let mut highlights: Vec<Value> = Vec::new();

    for svc_status in &statuses {
        let name = svc_status
            .get("name")
            .and_then(|n| n.as_str())
            .unwrap_or("");
        let authenticated = svc_status
            .get("authenticated")
            .and_then(|a| a.as_bool())
            .unwrap_or(false);

        if !authenticated {
            continue;
        }

        match name {
            "tidal" | "qobuz" => {
                highlights.push(json!({
                    "service": name,
                    "authenticated": true,
                    "featured_url": format!("/api/v1/streaming/{}/featured", name),
                    "new_releases_url": format!("/api/v1/streaming/{}/new-releases", name),
                }));
            }
            "spotify" | "deezer" => {
                highlights.push(json!({
                    "service": name,
                    "authenticated": true,
                    "featured_url": format!("/api/v1/streaming/{}/featured", name),
                }));
            }
            _ => {}
        }
    }

    // If we have authenticated services, also add settings hint
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let preferred_service = settings
        .get("preferred_streaming_service")
        .ok()
        .flatten()
        .unwrap_or_default();

    Json(json!({
        "services": highlights,
        "preferred_service": preferred_service,
    }))
}

/// #2441 — l'avancement que le client dessine et que le serveur n'emettait
/// nulle part.
///
/// `HomeView.svelte` (tune-web-client, `origin/main`) porte, sous la vignette
/// de « Continuer l'ecoute » :
///
/// ```svelte
/// {#if item.progress_percent != null}
///   <div class="continue-progress">
///     <div class="continue-progress-bar" style="width: {item.progress_percent}%"></div>
/// ```
///
/// `progress_percent` ne figurait dans AUCUN fichier de ce depot — compte fait
/// le 01/09/2026 : zero occurrence, toutes extensions confondues. La condition
/// n'a donc jamais pu etre vraie et la barre n'est apparue chez personne. C'est
/// le dernier point de l'arbitrage rendu par Bertrand le 01/09 : « corriger au
/// passage le champ de progression que le client lit et que le serveur n'emet
/// nulle part ».
///
/// Ces tests portent aussi le FAIT DE BASE de ce ticket, sur lequel toute la
/// section repose : un historique couvrant PLUSIEURS albums doit en rendre
/// plusieurs, dans le bon ordre, sans doublon.
#[cfg(test)]
mod tests_2441_progression {
    use super::*;

    fn poser_album(state: &AppState, artiste: &str, titre: &str, pistes: i64) -> i64 {
        let b = &state.backend;
        b.execute(
            "INSERT INTO artists (name) VALUES (?1)",
            &[&artiste as &dyn ToSqlValue],
        )
        .unwrap();
        let artiste_id = b.last_insert_rowid();
        b.execute(
            "INSERT INTO albums (title, artist_id, track_count) VALUES (?1, ?2, ?3)",
            &[
                &titre as &dyn ToSqlValue,
                &artiste_id as &dyn ToSqlValue,
                &pistes as &dyn ToSqlValue,
            ],
        )
        .unwrap();
        b.last_insert_rowid()
    }

    #[allow(clippy::too_many_arguments)]
    fn ecoute(
        state: &AppState,
        titre: &str,
        album: Option<&str>,
        album_id: Option<i64>,
        nature: &str,
        contexte_id: &str,
        quand: &str,
    ) {
        state
            .backend
            .execute(
                "INSERT INTO listen_history \
                 (title, artist_name, album_title, album_id, source, \
                  context_type, context_id, listened_at) \
                 VALUES (?1, 'Interprete', ?2, ?3, 'local', ?4, ?5, ?6)",
                &[
                    &titre as &dyn ToSqlValue,
                    &album as &dyn ToSqlValue,
                    &album_id as &dyn ToSqlValue,
                    &nature as &dyn ToSqlValue,
                    &contexte_id as &dyn ToSqlValue,
                    &quand as &dyn ToSqlValue,
                ],
            )
            .unwrap();
    }

    /// LE FAIT DE BASE de #2441, et l'avancement de chacun.
    ///
    /// Trois disques de cinq pistes, entames respectivement d'une, deux et
    /// trois pistes. La section doit rendre LES TROIS — c'est ce que le titre
    /// du ticket reclame — du plus recent au plus ancien, sans qu'aucun
    /// n'apparaisse deux fois, et chacun avec sa propre barre.
    #[test]
    fn plusieurs_albums_entames_rendent_chacun_leur_avancement() {
        let state = AppState::new(":memory:", 0, Default::default()).unwrap();
        let un = poser_album(&state, "Artiste Un", "Disque Un", 5);
        let deux = poser_album(&state, "Artiste Deux", "Disque Deux", 5);
        let trois = poser_album(&state, "Artiste Trois", "Disque Trois", 5);

        // Le plus ANCIEN est celui qu'on a le plus ecoute : l'ordre attendu
        // est celui de l'ecoute, pas celui de l'avancement.
        for (i, t) in ["A1"].iter().enumerate() {
            ecoute(
                &state,
                t,
                Some("Disque Un"),
                Some(un),
                "album",
                &un.to_string(),
                &format!("2026-08-28T22:4{i}:00Z"),
            );
        }
        for (i, t) in ["B1", "B2"].iter().enumerate() {
            ecoute(
                &state,
                t,
                Some("Disque Deux"),
                Some(deux),
                "album",
                &deux.to_string(),
                &format!("2026-08-28T23:1{i}:00Z"),
            );
        }
        for (i, t) in ["C1", "C2", "C3"].iter().enumerate() {
            ecoute(
                &state,
                t,
                Some("Disque Trois"),
                Some(trois),
                "album",
                &trois.to_string(),
                &format!("2026-08-29T08:0{i}:00Z"),
            );
        }

        let Ok(items) = fetch_continue_listening(&state, 10, None) else {
            panic!("la requete doit repondre")
        };

        // 1. PLUSIEURS albums, pas un seul.
        assert_eq!(
            items.len(),
            3,
            "« Continuer l'ecoute » doit rendre les TROIS disques entames, \
             obtenu : {items:?}"
        );

        // 2. Aucun doublon.
        let ids: Vec<i64> = items
            .iter()
            .filter_map(|i| i["album_id"].as_i64())
            .collect();
        let uniques: std::collections::HashSet<i64> = ids.iter().copied().collect();
        assert_eq!(
            ids.len(),
            uniques.len(),
            "un album remonte deux fois : {ids:?}"
        );

        // 3. Le bon ordre : du plus recemment ecoute au plus ancien.
        assert_eq!(
            ids,
            vec![trois, deux, un],
            "l'ordre doit etre celui de la derniere ecoute"
        );

        // 4. CHACUN avec son avancement — le champ que le client lit.
        let pourcents: Vec<Option<i64>> = items
            .iter()
            .map(|i| i["progress_percent"].as_i64())
            .collect();
        assert_eq!(
            pourcents,
            vec![Some(60), Some(40), Some(20)],
            "3/5, 2/5 et 1/5 de disque ecoute : {items:?}"
        );
    }

    /// TEMOIN — le cas a un seul album continue de rendre exactement cet
    /// album, avec sa barre. La correction ne doit rien changer ici.
    #[test]
    fn l_album_unique_rend_toujours_exactement_cet_album() {
        let state = AppState::new(":memory:", 0, Default::default()).unwrap();
        let seul = poser_album(&state, "Pulp", "Live", 5);
        for (i, t) in ["Common People", "Disco 2000"].iter().enumerate() {
            ecoute(
                &state,
                t,
                Some("Live"),
                Some(seul),
                "album",
                &seul.to_string(),
                &format!("2026-08-28T22:4{i}:00Z"),
            );
        }

        let Ok(items) = fetch_continue_listening(&state, 10, None) else {
            panic!("la requete doit repondre")
        };
        assert_eq!(items.len(), 1, "un seul album attendu : {items:?}");
        assert_eq!(items[0]["album_id"].as_i64(), Some(seul));
        assert_eq!(items[0]["context_type"], "album");
        assert_eq!(items[0]["listened_tracks"].as_i64(), Some(2));
        assert_eq!(
            items[0]["progress_percent"].as_i64(),
            Some(40),
            "2 pistes sur 5 : {items:?}"
        );
    }

    /// Une playlist n'a AUCUNE notion de completude : elle doit remonter, mais
    /// sans barre. Une barre a 0 % ferait croire a une ecoute jamais
    /// commencee — le client, lui, teste `!= null` et n'affiche alors rien.
    #[test]
    fn une_playlist_remonte_sans_barre_plutot_qu_a_zero_pour_cent() {
        let state = AppState::new(":memory:", 0, Default::default()).unwrap();
        state
            .backend
            .execute("INSERT INTO playlists (name) VALUES ('Route de nuit')", &[])
            .unwrap();
        let playlist = state.backend.last_insert_rowid();
        ecoute(
            &state,
            "So What",
            Some("Kind of Blue"),
            None,
            "playlist",
            &playlist.to_string(),
            "2026-08-28T22:45:00Z",
        );

        let Ok(items) = fetch_continue_listening(&state, 10, None) else {
            panic!("la requete doit repondre")
        };
        assert_eq!(items.len(), 1, "la playlist doit remonter : {items:?}");
        assert_eq!(items[0]["context_type"], "playlist");
        assert!(
            items[0]["progress_percent"].is_null(),
            "une playlist n'a pas d'avancement d'album : {items:?}"
        );
        assert!(
            items[0]
                .as_object()
                .unwrap()
                .contains_key("progress_percent"),
            "le champ doit EXISTER meme nul — les clients deployes le lisent"
        );
    }

    /// Un album de STREAMING n'a pas de ligne dans `albums` : on ignore son
    /// nombre de pistes. Pas de barre inventee.
    #[test]
    fn un_album_de_streaming_n_annonce_pas_d_avancement() {
        let state = AppState::new(":memory:", 0, Default::default()).unwrap();
        ecoute(
            &state,
            "Piste Qobuz",
            Some("Album Qobuz"),
            None,
            "album",
            "qobuz:12345",
            "2026-08-28T22:45:00Z",
        );

        let Ok(items) = fetch_continue_listening(&state, 10, None) else {
            panic!("la requete doit repondre")
        };
        assert_eq!(items.len(), 1, "l'album de flux doit remonter : {items:?}");
        assert!(
            items[0]["progress_percent"].is_null(),
            "nombre de pistes inconnu : aucune barre. {items:?}"
        );
    }

    /// GARDE — deux gestes de la MEME seconde ne doivent pas se classer au
    /// hasard. `listened_at` est au format seconde : deux clics rapproches y
    /// tombent, et le `truncate` decidait alors en silence lequel disparait.
    ///
    /// La divergence n'a PAS ete observee entre les deux moteurs sur ce jeu
    /// (mesure du 01/09) : ce test fige un ordre total, il ne repare pas un
    /// defaut constate.
    #[test]
    fn deux_contextes_de_la_meme_seconde_gardent_un_ordre_total() {
        let state = AppState::new(":memory:", 0, Default::default()).unwrap();
        let album = poser_album(&state, "Pulp", "Live", 5);
        state
            .backend
            .execute("INSERT INTO playlists (name) VALUES ('Route de nuit')", &[])
            .unwrap();
        let playlist = state.backend.last_insert_rowid();

        let meme_seconde = "2026-08-28T22:45:00Z";
        ecoute(
            &state,
            "Common People",
            Some("Live"),
            Some(album),
            "album",
            &album.to_string(),
            meme_seconde,
        );
        ecoute(
            &state,
            "So What",
            Some("Kind of Blue"),
            None,
            "playlist",
            &playlist.to_string(),
            meme_seconde,
        );

        let lire = || {
            let Ok(items) = fetch_continue_listening(&state, 10, None) else {
                panic!("la requete doit repondre")
            };
            items
                .iter()
                .map(|i| {
                    format!(
                        "{}:{}",
                        i["context_type"].as_str().unwrap_or_default(),
                        i["context_id"].as_str().unwrap_or_default()
                    )
                })
                .collect::<Vec<_>>()
        };

        let attendu = vec![format!("album:{album}"), format!("playlist:{playlist}")];
        assert_eq!(lire(), attendu, "l'ordre doit etre celui du departage");
        assert_eq!(lire(), attendu, "et il doit etre le meme a chaque appel");
    }
}
