//! Le rapprochement « autres versions d'un titre », PARTAGE.
//!
//! Deux routes s'en servent, et il ne doit exister qu'une seule doctrine de
//! rapprochement :
//!
//! - `GET /home/other-versions` (`routes/home.rs`) — le vivier est
//!   l'historique d'ecoute, borne aux dernieres ecoutes ;
//! - `GET /library/tracks/{id}/versions` (`routes/library/tracks.rs`) — le
//!   vivier est UNE piste, celle que l'auditeur designe dans le menu « … ».
//!
//! Ce qui est commun est ici : le classement d'un resultat
//! (`classer_version`), le predicat SQL du rapprochement local
//! (`predicat_rapprochement`), et la recherche streaming avec son cache
//! (`versions_streaming`). Les deux routes ne gardent que leur vivier.

use serde_json::{Value, json};
use tune_http_types::panne_sql::OuDefautJournalise;

use tune_core::db::backend::ToSqlValue;
use tune_core::db::engine::{Engine, PostgresDialect, SqlDialect, SqliteDialect};

use crate::state::AppState;

/// Classement d'un resultat de recherche par rapport au morceau de reference.
///
/// Le rapprochement reste strict sur le coeur du titre (insensible a la
/// casse) : identite exacte, ou suffixe d'edition delimite par ` (` / ` [`.
/// Il ne fait aucun rapprochement flou. La REPRISE est assumee : meme coeur
/// de titre, autre artiste. Pour un titre banal (« Angel ») cela produira des
/// homonymes — c'est le prix explicite de la demande (« des reprises de
/// Billie Jean, il y en a plein », Bertrand, 25/08), et l'ecran les range
/// sous un libelle « Reprises » qui assume l'incertitude.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ClasseVersion {
    /// Le meme enregistrement (meme artiste, meme album) : rien a proposer.
    MemeEnregistrement,
    /// Meme artiste, autre album : une autre version au sens strict.
    AutreVersion,
    /// Meme titre, autre artiste : une reprise possible.
    Reprise,
    /// Titre different : hors sujet.
    SansRapport,
}

pub(crate) fn classer_version(
    titre_ecoute: &str,
    artiste_ecoute: &str,
    album_ecoute: &str,
    titre_trouve: &str,
    artiste_trouve: &str,
    album_trouve: &str,
) -> ClasseVersion {
    let meme = |a: &str, b: &str| a.trim().to_lowercase() == b.trim().to_lowercase();
    if !titres_equivalents(titre_ecoute, titre_trouve) {
        return ClasseVersion::SansRapport;
    }
    if meme(artiste_ecoute, artiste_trouve) {
        if meme(album_ecoute, album_trouve) {
            ClasseVersion::MemeEnregistrement
        } else {
            ClasseVersion::AutreVersion
        }
    } else {
        ClasseVersion::Reprise
    }
}

/// Deux titres designent le meme morceau quand ils sont identiques, ou quand
/// l'un ajoute au titre nu un suffixe d'edition entre parentheses/crochets.
///
/// Ce n'est volontairement PAS un matcher flou : `Heroes` ne rejoint ni
/// `Hero` ni `Heroes - Live`. La frontiere explicite evite aussi qu'un simple
/// prefixe lexical suffise. Les variantes de FabienM, en revanche, convergent :
/// `Running Up That Hill`, `Running Up That Hill (A Deal With God)` et
/// `Running Up That Hill (12' Mix) [Bonus Track]`.
pub(crate) fn titres_equivalents(a: &str, b: &str) -> bool {
    let a = a.trim().to_lowercase();
    let b = b.trim().to_lowercase();
    a == b || titre_est_base_de(&a, &b) || titre_est_base_de(&b, &a)
}

fn titre_est_base_de(base: &str, variante: &str) -> bool {
    variante
        .strip_prefix(base)
        .is_some_and(|suffixe| suffixe.starts_with(" (") || suffixe.starts_with(" ["))
}

/// Traduction SQL portable de [`titres_equivalents`]. `SUBSTR`, `LENGTH`,
/// `TRIM` et `LOWER` ont le meme contrat dans SQLite et PostgreSQL. Aucun
/// `LIKE` : les `%` et `_` legitimes d'un titre ne deviennent jamais des
/// jokers SQL.
pub(crate) fn predicat_titres_equivalents(a: &str, b: &str) -> String {
    let a = format!("TRIM({a})");
    let b = format!("TRIM({b})");
    let a_est_base = format!(
        "(LENGTH({a}) < LENGTH({b}) \
         AND LOWER(SUBSTR({b}, 1, LENGTH({a}))) = LOWER({a}) \
         AND SUBSTR({b}, LENGTH({a}) + 1, 2) IN (' (', ' ['))"
    );
    let b_est_base = format!(
        "(LENGTH({b}) < LENGTH({a}) \
         AND LOWER(SUBSTR({a}, 1, LENGTH({b}))) = LOWER({b}) \
         AND SUBSTR({a}, LENGTH({b}) + 1, 2) IN (' (', ' ['))"
    );
    format!("(LOWER({a}) = LOWER({b}) OR {a_est_base} OR {b_est_base})")
}

/// Le predicat SQL du rapprochement LOCAL, ecrit UNE fois.
///
/// Les trois arguments sont des EXPRESSIONS SQL, pas des valeurs : la route
/// d'accueil y passe les colonnes de sa sous-requete d'historique
/// (`lh.title`, …), la route par piste y passe des marqueurs de parametre.
/// Les alias `t` (tracks), `al` (albums), `ar` (artiste d'album) et `ar2`
/// (artiste de piste) sont donc imposes aux deux appelants — c'est le prix a
/// payer pour que la regle ne soit pas recopiee.
///
/// La regle, elle, est celle de `classer_version` traduite en SQL : meme
/// titre, meme artiste, album DIFFERENT.
pub(crate) fn predicat_rapprochement(titre: &str, artiste: &str, album: &str) -> String {
    let titres = predicat_titres_equivalents("t.title", titre);
    format!(
        "{titres} \
         AND LOWER(COALESCE(ar2.name, ar.name, '')) = LOWER({artiste}) \
         AND LOWER(COALESCE(al.title, '')) <> LOWER(COALESCE({album}, ''))"
    )
}

/// Marqueur de parametre selon le moteur.
pub(crate) fn marqueur(engine: Engine, idx: usize) -> String {
    match engine {
        Engine::Sqlite => SqliteDialect.placeholder(idx),
        Engine::Postgres => PostgresDialect.placeholder(idx),
    }
}

/// Cache des recherches de versions : une entree par (service, titre), six
/// heures. Sans lui, chaque ouverture de l'accueil relancerait jusqu'a une
/// trentaine de recherches — le plafond de requetes des services n'y
/// survivrait pas. La route par piste partage le meme cache : un clic sur
/// « Autres versions » d'un morceau deja vu a l'accueil ne coute rien.
static CACHE_VERSIONS: std::sync::LazyLock<
    tokio::sync::Mutex<std::collections::HashMap<String, (std::time::Instant, Value)>>,
> = std::sync::LazyLock::new(|| tokio::sync::Mutex::new(std::collections::HashMap::new()));
const CACHE_VERSIONS_TTL: std::time::Duration = std::time::Duration::from_secs(6 * 3600);

/// Taille du vivier demandé au service avant le classement local.
///
/// Gardé dans une fonction pure pour que le contrat propre à chaque API soit
/// testable sans authentifiants de streaming.
fn limite_recherche_versions(service: &str) -> usize {
    match service {
        // Qobuz accepte 50 éléments dans sa première page. Demander cette
        // page entière ne coûte donc aucun aller-retour de plus, mais évite
        // qu'un titre courant (`Somebody`, #2774) chasse les versions du bon
        // artiste hors du vivier avant `classer_version`.
        "qobuz" => 50,
        _ => 10,
    }
}

/// Les autres versions d'un morceau PRESENTES DANS LA BIBLIOTHEQUE.
///
/// `exclure` est la piste de depart : elle satisferait le predicat si son
/// album etait NUL des deux cotes, et se proposerait elle-meme.
pub(crate) fn versions_locales(
    state: &AppState,
    titre: &str,
    artiste: &str,
    album: &str,
    exclure: Option<i64>,
    limite: i64,
) -> Vec<Value> {
    let e = state.backend.engine();
    // Les valeurs sont LIEES, jamais interpolees : elles viennent des tags
    // d'un fichier, donc d'une source qu'on ne controle pas. Seule `limite`,
    // bornee par l'appelant, part dans le texte de la requete.
    let sql = format!(
        "SELECT t.id, al.id, al.title, al.cover_path, t.duration_ms, t.format, al.year \
         FROM tracks t \
         JOIN albums al ON t.album_id = al.id \
         LEFT JOIN artists ar ON al.artist_id = ar.id \
         LEFT JOIN artists ar2 ON t.artist_id = ar2.id \
         CROSS JOIN (SELECT CAST({} AS TEXT) AS title, \
                            CAST({} AS TEXT) AS artist, \
                            CAST({} AS TEXT) AS album) ref_version \
         WHERE {} AND t.id <> {} \
         ORDER BY al.title \
         LIMIT {limite}",
        marqueur(e, 1),
        marqueur(e, 2),
        marqueur(e, 3),
        predicat_rapprochement(
            "ref_version.title",
            "ref_version.artist",
            "ref_version.album"
        ),
        marqueur(e, 4),
    );
    // `-1` quand il n'y a rien a exclure : aucune piste ne porte cet id, et
    // la requete garde une forme unique — un `AND` conditionnel serait un
    // deuxieme chemin SQL a tester.
    let sans = exclure.unwrap_or(-1);
    let params: [&dyn ToSqlValue; 4] = [&titre, &artiste, &album, &sans];
    state
        .backend
        .query_many(&sql, &params)
        .ou_defaut_journalise()
        .into_iter()
        .map(|cols| {
            json!({
                "track_id": cols.first().and_then(|v| v.as_i64()),
                "album_id": cols.get(1).and_then(|v| v.as_i64()),
                "album_title": cols.get(2).and_then(|v| v.as_string()),
                "cover_path": cols.get(3).and_then(|v| v.as_string()),
                "duration_ms": cols.get(4).and_then(|v| v.as_i64()),
                "format": cols.get(5).and_then(|v| v.as_string()),
                "year": cols.get(6).and_then(|v| v.as_i64()),
            })
        })
        .collect()
}

/// Les versions et reprises d'un morceau DISPONIBLES EN STREAMING.
///
/// Un service absent, non authentifie, en erreur ou lent ne fait jamais
/// echouer l'appel : il est simplement saute, et le resultat est partiel.
pub(crate) async fn versions_streaming(
    state: &AppState,
    titre: &str,
    artiste: &str,
    album: &str,
) -> Vec<Value> {
    let mut trouvees: Vec<Value> = Vec::new();
    for nom_service in ["qobuz", "tidal", "deezer", "spotify"] {
        let cle_cache = format!("{nom_service}:{}", titre.to_lowercase());
        let en_cache = {
            let cache = CACHE_VERSIONS.lock().await;
            cache
                .get(&cle_cache)
                .and_then(|(quand, v)| (quand.elapsed() < CACHE_VERSIONS_TTL).then(|| v.clone()))
        };
        let pistes: Value = if let Some(v) = en_cache {
            v
        } else {
            let arc = {
                let registre = state.services.lock().await;
                registre.get(nom_service)
            };
            let Some(arc) = arc else { continue };
            let svc = arc.read().await;
            if !svc.enabled() || !svc.auth_status().await.authenticated {
                continue;
            }
            let Ok(resultats) = svc
                .search(titre, limite_recherche_versions(nom_service))
                .await
            else {
                continue;
            };
            drop(svc);
            let v = json!(resultats.tracks);
            CACHE_VERSIONS
                .lock()
                .await
                .insert(cle_cache, (std::time::Instant::now(), v.clone()));
            v
        };
        let Some(pistes) = pistes.as_array() else {
            continue;
        };
        for piste in pistes {
            let t = piste["title"].as_str().unwrap_or_default();
            let a = piste["artist_name"].as_str().unwrap_or_default();
            let al = piste["album_title"].as_str().unwrap_or_default();
            let classe = classer_version(titre, artiste, album, t, a, al);
            let genre = match classe {
                ClasseVersion::AutreVersion => "version",
                ClasseVersion::Reprise => "reprise",
                _ => continue,
            };
            trouvees.push(json!({
                "service": nom_service,
                "source_id": piste["source_id"],
                "title": t,
                "artist_name": a,
                "album_title": al,
                "album_id": piste["album_id"],
                "cover_path": piste["cover_path"],
                "kind": genre,
            }));
        }
    }

    #[cfg(feature = "bandcamp")]
    {
        let cle_cache = format!("bandcamp:{}", titre.to_lowercase());
        let en_cache = {
            let cache = CACHE_VERSIONS.lock().await;
            cache
                .get(&cle_cache)
                .and_then(|(quand, v)| (quand.elapsed() < CACHE_VERSIONS_TTL).then(|| v.clone()))
        };
        let pistes: Value = if let Some(v) = en_cache {
            v
        } else {
            let v = json!(tune_bandcamp::rechercher_pistes(titre).await);
            CACHE_VERSIONS
                .lock()
                .await
                .insert(cle_cache, (std::time::Instant::now(), v.clone()));
            v
        };
        if let Some(pistes) = pistes.as_array() {
            for piste in pistes {
                let t = piste["title"].as_str().unwrap_or_default();
                let a = piste["artist_name"].as_str().unwrap_or_default();
                let al = piste["album_title"].as_str().unwrap_or_default();
                let classe = classer_version(titre, artiste, album, t, a, al);
                let genre = match classe {
                    ClasseVersion::AutreVersion => "version",
                    ClasseVersion::Reprise => "reprise",
                    _ => continue,
                };
                trouvees.push(json!({
                    "service": "bandcamp",
                    "source_id": piste["url"],
                    "title": t,
                    "artist_name": a,
                    "album_title": piste["album_title"],
                    "album_id": Value::Null,
                    "cover_path": piste["cover_url"],
                    "kind": genre,
                    "url": piste["url"],
                }));
            }
        }
    }

    trouvees
}

#[cfg(test)]
mod tests {
    use super::{
        ClasseVersion, classer_version, limite_recherche_versions, predicat_rapprochement,
        predicat_titres_equivalents, titres_equivalents,
    };

    /// Contre-épreuve de #2774 : `Somebody` / Depeche Mode peut être absent
    /// des dix premiers résultats Qobuz. Cinquante tient encore dans l'unique
    /// page de `/catalog/search`, donc élargit le vivier sans appel réseau
    /// supplémentaire. Les autres services conservent leur contrat existant.
    #[test]
    fn qobuz_classe_une_page_entiere_avant_de_filtrer_les_versions() {
        assert_eq!(limite_recherche_versions("qobuz"), 50);
        assert_eq!(limite_recherche_versions("tidal"), 10);
        assert_eq!(limite_recherche_versions("deezer"), 10);
        assert_eq!(limite_recherche_versions("spotify"), 10);
    }

    /// « Billie Jean » par Michael Jackson sur un AUTRE album : une version.
    #[test]
    fn meme_artiste_autre_album_est_une_version() {
        assert_eq!(
            classer_version(
                "Billie Jean",
                "Michael Jackson",
                "Thriller",
                "billie jean",
                "MICHAEL JACKSON",
                "Number Ones"
            ),
            ClasseVersion::AutreVersion
        );
    }

    /// « Billie Jean » par quelqu'un d'autre : une reprise.
    #[test]
    fn autre_artiste_est_une_reprise() {
        assert_eq!(
            classer_version(
                "Billie Jean",
                "Michael Jackson",
                "Thriller",
                "Billie Jean",
                "Chris Cornell",
                "Unplugged in Sweden"
            ),
            ClasseVersion::Reprise
        );
    }

    /// Le même enregistrement ne doit RIEN proposer.
    #[test]
    fn meme_enregistrement_est_ecarte() {
        assert_eq!(
            classer_version(
                "Billie Jean",
                "Michael Jackson",
                "Thriller",
                "Billie Jean",
                "Michael Jackson",
                "Thriller"
            ),
            ClasseVersion::MemeEnregistrement
        );
    }

    /// Un suffixe d'edition explicite reste le meme morceau et devient donc
    /// une autre version quand l'artiste est identique.
    #[test]
    fn suffixe_d_edition_est_une_version() {
        assert_eq!(
            classer_version(
                "Billie Jean",
                "Michael Jackson",
                "Thriller",
                "Billie Jean (Live)",
                "Michael Jackson",
                "This Is It"
            ),
            ClasseVersion::AutreVersion
        );
    }

    /// Contre-epreuve exacte de #2638 : les trois titres officiels convergent,
    /// mais une simple ressemblance sans delimiteur reste hors sujet.
    #[test]
    fn variantes_running_up_that_hill_partagent_le_meme_coeur() {
        let nu = "Running Up That Hill";
        assert!(titres_equivalents(
            nu,
            "Running Up That Hill (A Deal With God)"
        ));
        assert!(titres_equivalents(
            nu,
            "Running Up That Hill (12' Mix) [Bonus Track]"
        ));
        assert!(titres_equivalents(
            "Running Up That Hill (A Deal With God)",
            nu
        ));
        assert!(!titres_equivalents(nu, "Running Up That Mountain"));
        assert!(!titres_equivalents("Hero", "Heroes"));
        assert!(!titres_equivalents("Heroes", "Heroes - Live"));
    }

    #[test]
    fn le_predicat_titre_n_utilise_aucun_joker_like() {
        let p = predicat_titres_equivalents("t.title", "$1");
        assert!(p.contains("SUBSTR"), "frontiere de suffixe absente : {p}");
        assert!(p.contains("IN (' (', ' [')"), "delimiteurs absents : {p}");
        assert!(
            !p.contains("LIKE"),
            "un titre ne doit pas devenir un motif : {p}"
        );
    }

    /// Marqueur de contrat : le prédicat porte les TROIS conditions. S'il en
    /// perd une, la route par piste et la section d'accueil divergent en
    /// silence — c'est précisément ce que cette factorisation empêche.
    #[test]
    fn le_predicat_porte_les_trois_conditions() {
        let p = predicat_rapprochement("lh.title", "lh.artist_name", "lh.album_title");
        assert!(
            p.contains("LOWER(TRIM(t.title)) = LOWER(TRIM(lh.title))"),
            "titre : {p}"
        );
        assert!(
            p.contains("LOWER(COALESCE(ar2.name, ar.name, '')) = LOWER(lh.artist_name)"),
            "artiste : {p}"
        );
        assert!(
            p.contains("LOWER(COALESCE(al.title, '')) <> LOWER(COALESCE(lh.album_title, ''))"),
            "album different : {p}"
        );
    }
}
