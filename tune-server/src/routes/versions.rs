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

use crate::routes::filtre_sources::FiltreSources;

use crate::state::AppState;

/// Classement d'un resultat de recherche par rapport au morceau de reference.
///
/// Le rapprochement reste strict sur le coeur du titre (insensible a la
/// casse) : identite exacte, ou suffixe d'edition ouvert par l'un des
/// [`DELIMITEURS_D_EDITION`]. Il ne fait aucun rapprochement flou. La REPRISE
/// est assumee : meme coeur de titre, autre artiste. Pour un titre banal
/// (« Angel ») cela produira des homonymes — c'est le prix explicite de la
/// demande (« des reprises de Billie Jean, il y en a plein », Bertrand,
/// 25/08), et l'ecran les range sous un libelle « Reprises » qui assume
/// l'incertitude.
///
/// ⚠️ Cette classe dit qui ENTRE, jamais dans quel ordre. L'ordre est le
/// travail de [`score_version`] : un arbre binaire sur trois chaines ne peut
/// pas departager deux candidats egalement « meme artiste, autre album », et
/// c'est ce qui laissait la meilleure version en queue de liste (#2372).
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

/// Les trois delimiteurs qui ouvrent un suffixe d'edition.
///
/// ` (` et ` [` sont d'origine. ` - ` a ete ajoute pour #2372 : c'est la forme
/// que Qobuz, Tidal et Deezer emploient pour les remasters — « Smooth Operator
/// - 2011 Remastered », « Heroes - 2017 Remaster ». Sans lui, la fonction
/// « Autres versions » rate la convention de nommage LA PLUS COURANTE de ce
/// qu'elle est faite pour trouver, exactement le cas decrit par Gros Bidon
/// (« Reissue, Remastered, Special Edition », fil 1627, 31/08).
///
/// ⚠️ Le tiret est exige ENTOURE d'espaces (` - `), jamais nu : sans cela
/// `Cross-Eyed Mary` deviendrait une variante de `Cross`.
///
/// ⚠️ Trou connu, et ANCIEN : rien ne distingue un suffixe d'edition d'une
/// partie d'oeuvre. `Rock And Roll - Part 2` passe pour une version de
/// `Rock And Roll` — mais `Rock And Roll (Part 2)` le faisait deja avec ` (`.
/// Le tiret n'ouvre donc pas une classe d'erreur neuve, il elargit la
/// precedente ; la fermer demanderait le dictionnaire de qualificatifs propose
/// par FabienM (fil 1627), qui est un chantier a lui seul et devrait alors
/// valoir pour les TROIS delimiteurs.
const DELIMITEURS_D_EDITION: [&str; 3] = [" (", " [", " - "];

/// Deux titres designent le meme morceau quand ils sont identiques, ou quand
/// l'un ajoute au titre nu un suffixe d'edition ouvert par l'un des
/// [`DELIMITEURS_D_EDITION`].
///
/// Ce n'est volontairement PAS un matcher flou : `Heroes` ne rejoint pas
/// `Hero`, et `Somebody` ne rejoint pas `Somebody To Love` — le piege des
/// titres inclus, nomme par FabienM. La frontiere explicite evite qu'un simple
/// prefixe lexical suffise. Les variantes de FabienM, en revanche, convergent :
/// `Running Up That Hill`, `Running Up That Hill (A Deal With God)` et
/// `Running Up That Hill (12' Mix) [Bonus Track]` ; celle de Gros Bidon aussi
/// desormais : `Smooth Operator - 2011 Remastered`.
pub(crate) fn titres_equivalents(a: &str, b: &str) -> bool {
    let a = a.trim().to_lowercase();
    let b = b.trim().to_lowercase();
    a == b || titre_est_base_de(&a, &b) || titre_est_base_de(&b, &a)
}

fn titre_est_base_de(base: &str, variante: &str) -> bool {
    variante
        .strip_prefix(base)
        .is_some_and(|suffixe| DELIMITEURS_D_EDITION.iter().any(|d| suffixe.starts_with(d)))
}

/// Traduction SQL portable de [`titres_equivalents`]. `SUBSTR`, `LENGTH`,
/// `TRIM` et `LOWER` ont le meme contrat dans SQLite et PostgreSQL. Aucun
/// `LIKE` : les `%` et `_` legitimes d'un titre ne deviennent jamais des
/// jokers SQL.
///
/// Les delimiteurs ne sont pas recopies a la main : ils sont derives de
/// [`DELIMITEURS_D_EDITION`], pour qu'un delimiteur ajoute d'un cote ne puisse
/// pas manquer de l'autre. C'est exactement ce qui separerait la route locale
/// (SQL) de la route streaming (Rust) sur le meme titre.
pub(crate) fn predicat_titres_equivalents(a: &str, b: &str) -> String {
    let a = format!("TRIM({a})");
    let b = format!("TRIM({b})");
    format!(
        "(LOWER({a}) = LOWER({b}) OR {} OR {})",
        predicat_suffixe(&a, &b),
        predicat_suffixe(&b, &a)
    )
}

/// « `base` est le titre nu de `variante` » en SQL.
///
/// Un `OR` par delimiteur, chacun compare sur SA longueur : ` (` et ` [` font
/// deux caracteres, ` - ` en fait trois. Un `IN` unique ne pourrait pas les
/// melanger.
fn predicat_suffixe(base: &str, variante: &str) -> String {
    let alternatives: Vec<String> = DELIMITEURS_D_EDITION
        .iter()
        .map(|d| {
            format!(
                "SUBSTR({variante}, LENGTH({base}) + 1, {}) = '{d}'",
                d.chars().count()
            )
        })
        .collect();
    format!(
        "(LENGTH({base}) < LENGTH({variante}) \
         AND LOWER(SUBSTR({variante}, 1, LENGTH({base}))) = LOWER({base}) \
         AND ({}))",
        alternatives.join(" OR ")
    )
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

/// Le morceau de REFERENCE d'un rapprochement, avec les signaux qui
/// permettent de classer les candidats et pas seulement de les retenir.
///
/// La route par piste les connait tous (elle part d'une ligne de `tracks`) ;
/// la section d'accueil n'en connait aucun (son vivier est `listen_history`,
/// qui ne porte ni ISRC ni duree). Les champs sont donc optionnels, et un
/// signal absent ne rapporte simplement aucun point — jamais une penalite.
#[derive(Debug, Default, Clone)]
pub(crate) struct Reference {
    pub titre: String,
    pub artiste: String,
    pub album: String,
    pub isrc: Option<String>,
    pub duree_ms: Option<i64>,
    pub annee: Option<i64>,
}

/// Les signaux d'un CANDIDAT, locaux ou venus d'un service.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct Signaux<'a> {
    pub titre: &'a str,
    pub isrc: Option<&'a str>,
    pub duree_ms: Option<i64>,
    pub annee: Option<i64>,
}

/// ── Le bareme, ecrit une fois ────────────────────────────────────────────
///
/// Ce qui existait avant #2372 n'etait pas un classement : `classer_version`
/// rendait une CLASSE a partir de trois chaines, et les candidats sortaient
/// dans l'ordre ou le service les avait rendus. Deux versions egalement
/// « autre album » etaient donc indiscernables, et la plus interessante
/// pouvait finir en queue de liste.
///
/// Le score ne remplace pas `classer_version` : celui-la decide qui ENTRE
/// (version, reprise, ou rien), celui-ci decide dans quel ORDRE. Aucun seuil,
/// aucune exclusion — un candidat a 0 point reste affiche, simplement plus
/// bas. C'est deliberé : le seul signal qui pourrait justifier d'exclure est
/// l'identite d'oeuvre, et elle n'existe pas (cf. la couverture MBID).
///
/// L'ISRC domine tout le reste parce que c'est le seul signal NON TEXTUEL du
/// lot : deux enregistrements qui le partagent sont le meme enregistrement,
/// sans interpretation. Les durees viennent ensuite, par ecarts croissants —
/// un live ou un remix s'ecarte, un remaster ne bouge quasiment pas. L'annee
/// ne vaut que pour departager : une edition d'une AUTRE annee apprend
/// quelque chose, la meme annee n'apprend rien.
pub(crate) const POINTS_ISRC_IDENTIQUE: i32 = 100;
/// Ecart de duree ≤ 2 s : le meme master, ou son remaster.
pub(crate) const POINTS_DUREE_QUASI_EGALE: i32 = 40;
/// Ecart ≤ 10 s : la meme prise, montee autrement.
pub(crate) const POINTS_DUREE_PROCHE: i32 = 25;
/// Ecart ≤ 60 s : plausible (live, edit radio), sans plus.
pub(crate) const POINTS_DUREE_VOISINE: i32 = 10;
/// Titre identique au caractere pres (casse et espaces ignores).
pub(crate) const POINTS_TITRE_EXACT: i32 = 20;
/// Titre nu + suffixe d'edition : lie, mais pas identique.
pub(crate) const POINTS_TITRE_SUFFIXE: i32 = 10;
/// Deux annees connues et DIFFERENTES : une autre edition, donc une vraie
/// trouvaille.
pub(crate) const POINTS_ANNEE_DIFFERENTE: i32 = 5;

/// Le score d'un candidat face a la reference. Voir le bareme ci-dessus.
pub(crate) fn score_version(reference: &Reference, candidat: Signaux<'_>) -> i32 {
    let mut points = 0;

    // ISRC : egalite stricte apres normalisation (les services le rendent
    // tantot en majuscules, tantot avec des tirets — `FRZ128800001` et
    // `FR-Z12-88-00001` designent le meme enregistrement).
    if let (Some(a), Some(b)) = (reference.isrc.as_deref(), candidat.isrc)
        && !a.trim().is_empty()
        && normaliser_isrc(a) == normaliser_isrc(b)
    {
        points += POINTS_ISRC_IDENTIQUE;
    }

    if let (Some(a), Some(b)) = (reference.duree_ms, candidat.duree_ms)
        && a > 0
        && b > 0
    {
        points += match a.abs_diff(b) {
            0..=2_000 => POINTS_DUREE_QUASI_EGALE,
            2_001..=10_000 => POINTS_DUREE_PROCHE,
            10_001..=60_000 => POINTS_DUREE_VOISINE,
            _ => 0,
        };
    }

    points += if reference
        .titre
        .trim()
        .eq_ignore_ascii_case(candidat.titre.trim())
    {
        POINTS_TITRE_EXACT
    } else if titres_equivalents(&reference.titre, candidat.titre) {
        POINTS_TITRE_SUFFIXE
    } else {
        0
    };

    if let (Some(a), Some(b)) = (reference.annee, candidat.annee)
        && a > 0
        && b > 0
        && a != b
    {
        points += POINTS_ANNEE_DIFFERENTE;
    }

    points
}

/// Un ISRC comparable : majuscules, sans separateur ni espace.
fn normaliser_isrc(brut: &str) -> String {
    brut.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_uppercase())
        .collect()
}

/// Les requetes envoyees a un service pour trouver les versions d'un morceau.
///
/// ⚠️ C'est ICI que se joue le defaut que FabienM decrit depuis le 27/08. Le
/// serveur n'envoyait QUE le titre, puis filtrait sur l'artiste APRES
/// reception — donc dans un vivier ou l'artiste cherche pouvait ne pas figurer
/// du tout. Il l'a mesure lui-meme (fil 1611, 29/08) : « Lorsque je cherche
/// directement "Somebody" [...] j'obtiens 50 resultats de titres mais aucun de
/// l'artiste "Depeche Mode" [...] Si je cherche le meme titre dans
/// l'application Qobuz, j'obtiens bien des titres "Somebody" de Depeche Mode. »
/// Elargir le vivier de 10 a 50 (#2790) ne pouvait pas y suffire : le
/// probleme n'est pas la taille de la page, c'est que la requete ne nomme pas
/// l'artiste. Le rapprochement LOCAL, lui, nomme l'artiste depuis #2497 —
/// `predicat_rapprochement` le porte ; c'est le seul chemin streaming qui ne
/// le faisait pas.
///
/// Deux requetes, et pas une :
///
/// 1. `artiste titre` — celle qui trouve les AUTRES VERSIONS DU MEME ARTISTE,
///    c'est-a-dire la definition stricte d'« autre version » ;
/// 2. `titre` seul — celle qui trouve les REPRISES, portees par construction
///    par un autre artiste. La supprimer retirerait la moitie de la fonction,
///    ce que #2790 refusait deja explicitement.
///
/// Le cout : DEUX recherches par service et par titre au lieu d'une, soit au
/// pire 6 titres × 4 services × 2 = 48 appels pour un accueil froid. Le cache
/// six heures les absorbe, et la cle de cache porte desormais la REQUETE et
/// non le titre — sans quoi la seconde requete lirait la reponse de la
/// premiere.
pub(crate) fn requetes_versions(titre: &str, artiste: &str) -> Vec<String> {
    let titre = titre.trim();
    let artiste = artiste.trim();
    if artiste.is_empty() || titre.is_empty() {
        return vec![titre.to_string()];
    }
    vec![format!("{artiste} {titre}"), titre.to_string()]
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

/// Plafond du vivier LOCAL lu avant classement.
///
/// Le `LIMIT` de la requete ne peut plus etre `limite` : c'est desormais le
/// SCORE qui decide de l'ordre, donc de ce que `limite` retient, et le score
/// se calcule en Rust. Un `LIMIT limite` cote SQL choisirait les lignes AVANT
/// de savoir lesquelles sont les meilleures, et il les choisirait selon la
/// collation du moteur — `ORDER BY al.title` ne range pas pareil sous la
/// collation binaire de SQLite et sous celle, locale, de PostgreSQL. Deux
/// moteurs rendraient alors deux ENSEMBLES differents.
///
/// On lit donc jusqu'a ce plafond, on classe, puis on coupe a `limite`. Le
/// plafond est deux fois et demie le maximum accepte par la route (200) :
/// au-dela, une bibliotheque porterait 500 exemplaires du meme morceau par le
/// meme artiste sur 500 albums differents, ce qui n'est pas un etat reel.
const VIVIER_LOCAL_MAX: i64 = 500;

/// Les autres versions d'un morceau PRESENTES DANS LA BIBLIOTHEQUE.
///
/// `exclure` est la piste de depart : elle satisferait le predicat si son
/// album etait NUL des deux cotes, et se proposerait elle-meme.
///
/// L'ordre du resultat est decide en RUST, pas par le moteur : voir
/// [`VIVIER_LOCAL_MAX`] et [`classer_par_score`].
pub(crate) fn versions_locales(
    state: &AppState,
    reference: &Reference,
    exclure: Option<i64>,
    limite: i64,
) -> Vec<Value> {
    let e = state.backend.engine();
    // Les valeurs sont LIEES, jamais interpolees : elles viennent des tags
    // d'un fichier, donc d'une source qu'on ne controle pas. Seul le plafond,
    // une constante, part dans le texte de la requete.
    let sql = format!(
        "SELECT t.id, al.id, al.title, al.cover_path, t.duration_ms, t.format, al.year, \
                t.title, t.isrc \
         FROM tracks t \
         JOIN albums al ON t.album_id = al.id \
         LEFT JOIN artists ar ON al.artist_id = ar.id \
         LEFT JOIN artists ar2 ON t.artist_id = ar2.id \
         CROSS JOIN (SELECT CAST({} AS TEXT) AS title, \
                            CAST({} AS TEXT) AS artist, \
                            CAST({} AS TEXT) AS album) ref_version \
         WHERE {} AND t.id <> {} \
         ORDER BY al.title, t.id \
         LIMIT {VIVIER_LOCAL_MAX}",
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
    let titre = reference.titre.clone();
    let artiste = reference.artiste.clone();
    let album = reference.album.clone();
    let params: [&dyn ToSqlValue; 4] = [&titre, &artiste, &album, &sans];
    let mut lignes: Vec<Value> = state
        .backend
        .query_many(&sql, &params)
        .ou_defaut_journalise()
        .into_iter()
        .map(|cols| {
            let titre_trouve = cols.get(7).and_then(|v| v.as_string()).unwrap_or_default();
            let isrc = cols.get(8).and_then(|v| v.as_string());
            let duree = cols.get(4).and_then(|v| v.as_i64());
            let annee = cols.get(6).and_then(|v| v.as_i64());
            let score = score_version(
                reference,
                Signaux {
                    titre: &titre_trouve,
                    isrc: isrc.as_deref(),
                    duree_ms: duree,
                    annee,
                },
            );
            json!({
                "track_id": cols.first().and_then(|v| v.as_i64()),
                "album_id": cols.get(1).and_then(|v| v.as_i64()),
                "album_title": cols.get(2).and_then(|v| v.as_string()),
                "cover_path": cols.get(3).and_then(|v| v.as_string()),
                "duration_ms": duree,
                "format": cols.get(5).and_then(|v| v.as_string()),
                "year": annee,
                // Le titre RETROUVE, et non celui de reference : avec le
                // delimiteur ` - `, « Smooth Operator - 2011 Remastered » est
                // desormais une version de « Smooth Operator », et l'ecran ne
                // peut plus supposer que les deux libelles coincident.
                "title": titre_trouve,
                "score": score,
            })
        })
        .collect();
    classer_par_score(&mut lignes, |v| {
        (
            v["album_title"].as_str().unwrap_or_default().to_lowercase(),
            v["track_id"].as_i64().unwrap_or_default(),
        )
    });
    lignes.truncate(limite.max(0) as usize);
    lignes
}

/// Range des candidats deja porteurs d'un champ `score` : score DECROISSANT,
/// puis la cle de departage rendue par `cle`.
///
/// ⚠️ Le tri est fait ICI, en Rust, et pas par un `ORDER BY` — c'est la seule
/// facon d'obtenir le MEME ORDRE sur les deux moteurs. `ORDER BY al.title`
/// compare sous la collation du moteur : binaire pour SQLite, locale pour
/// PostgreSQL. « the wall » et « Thriller » n'y tombent pas dans le meme
/// ordre. Un tri Rust sur des chaines deja minusculees ne depend, lui, de
/// rien d'autre que des octets — et un tri non deterministe ferait reapparaitre
/// les memes lignes d'une page a l'autre.
///
/// `sort_by` est STABLE, mais la cle est totale (elle finit par un
/// identifiant unique) : deux elements ne peuvent pas etre ex aequo.
fn classer_par_score<K, F>(lignes: &mut [Value], cle: F)
where
    K: Ord,
    F: Fn(&Value) -> K,
{
    lignes.sort_by(|a, b| {
        let sa = a["score"].as_i64().unwrap_or_default();
        let sb = b["score"].as_i64().unwrap_or_default();
        sb.cmp(&sa).then_with(|| cle(a).cmp(&cle(b)))
    });
}

/// Les services interroges pour les versions d'un morceau.
///
/// Ecrite UNE fois : `/home/other-versions` s'en sert pour savoir s'il vaut la
/// peine de lire son vivier d'ecoutes avant d'appeler
/// [`versions_streaming`]. Deux listes qui derivent l'une de l'autre feraient
/// lire l'historique pour rien, ou pire, le sauteraient alors qu'un service
/// repond.
pub(crate) const SERVICES_VERSIONS: [&str; 4] = ["qobuz", "tidal", "deezer", "spotify"];

/// Les versions et reprises d'un morceau DISPONIBLES EN STREAMING.
///
/// Un service absent, non authentifie, en erreur ou lent ne fait jamais
/// echouer l'appel : il est simplement saute, et le resultat est partiel.
///
/// `filtre` dit QUELS services repondent — le meme `sources` que partout
/// ailleurs (`routes::filtre_sources`). Il est pris en ARGUMENT et non relu
/// d'une URL : cette fonction sert DEUX routes, `/home/other-versions` et
/// `/library/tracks/{id}/versions`, et c'est ce qui garantit qu'elles
/// appliquent la meme regle sans la recopier. [`FiltreSources::tout`] rend le
/// comportement d'avant, a l'octet.
///
/// Le service ecarte l'est AVANT le cache et avant tout appel reseau : le
/// filtre economise l'appel, il ne jette pas un resultat deja paye.
pub(crate) async fn versions_streaming(
    state: &AppState,
    reference: &Reference,
    filtre: &FiltreSources,
) -> Vec<Value> {
    let titre = reference.titre.as_str();
    let artiste = reference.artiste.as_str();
    let album = reference.album.as_str();
    let requetes = requetes_versions(titre, artiste);
    let mut trouvees: Vec<Value> = Vec::new();
    // (service, source_id) deja retenus. Les deux requetes se recouvrent par
    // construction — « Depeche Mode Somebody » et « Somebody » rendent les
    // memes pistes du bon artiste —, et une version affichee deux fois serait
    // pire que la version manquante qu'on repare.
    let mut deja: std::collections::HashSet<(String, String)> = std::collections::HashSet::new();
    for nom_service in SERVICES_VERSIONS {
        if !filtre.service_demande(nom_service) {
            continue;
        }
        for requete in &requetes {
            let cle_cache = format!("{nom_service}:{}", requete.to_lowercase());
            let en_cache = {
                let cache = CACHE_VERSIONS.lock().await;
                cache.get(&cle_cache).and_then(|(quand, v)| {
                    (quand.elapsed() < CACHE_VERSIONS_TTL).then(|| v.clone())
                })
            };
            let pistes: Value = if let Some(v) = en_cache {
                v
            } else {
                let arc = {
                    let registre = state.services.lock().await;
                    registre.get(nom_service)
                };
                let Some(arc) = arc else { break };
                let svc = arc.read().await;
                if !svc.enabled() || !svc.auth_status().await.authenticated {
                    break;
                }
                let Ok(resultats) = svc
                    .search(requete, limite_recherche_versions(nom_service))
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
                let source_id = piste["source_id"].as_str().unwrap_or_default().to_string();
                if !deja.insert((nom_service.to_string(), source_id)) {
                    continue;
                }
                let duree = piste["duration_ms"].as_i64();
                let isrc = piste["isrc"].as_str();
                trouvees.push(json!({
                    "service": nom_service,
                    "source_id": piste["source_id"],
                    "title": t,
                    "artist_name": a,
                    "album_title": al,
                    "album_id": piste["album_id"],
                    "cover_path": piste["cover_path"],
                    "kind": genre,
                    // Rendus au client parce qu'ils sont ce qui a servi a
                    // classer : sans eux l'ecran affiche un ordre qu'il ne
                    // peut ni expliquer ni reproduire.
                    "duration_ms": duree,
                    "isrc": isrc,
                    "score": score_version(
                        reference,
                        Signaux { titre: t, isrc, duree_ms: duree, annee: None },
                    ),
                }));
            }
        }
    }

    // ⚠️ Bandcamp est un CINQUIEME service, hors du tableau ci-dessus et hors
    // du registre : il ne s'authentifie pas et sa recherche est un appel
    // reseau direct. Filtrer la seule boucle du registre l'aurait laisse
    // repondre a `sources=qobuz` — un service non demande, paye au prix d'un
    // appel sortant. Il n'est visible ni depuis la liste des routes ni depuis
    // `state.services` ; c'est le mecanisme qu'il faut sonder, pas le mot.
    #[cfg(feature = "bandcamp")]
    for requete in filtre
        .service_demande("bandcamp")
        .then_some(&requetes)
        .into_iter()
        .flatten()
    {
        let cle_cache = format!("bandcamp:{}", requete.to_lowercase());
        let en_cache = {
            let cache = CACHE_VERSIONS.lock().await;
            cache
                .get(&cle_cache)
                .and_then(|(quand, v)| (quand.elapsed() < CACHE_VERSIONS_TTL).then(|| v.clone()))
        };
        let pistes: Value = if let Some(v) = en_cache {
            v
        } else {
            let v = json!(tune_bandcamp::rechercher_pistes(requete).await);
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
                let url = piste["url"].as_str().unwrap_or_default().to_string();
                if !deja.insert(("bandcamp".to_string(), url)) {
                    continue;
                }
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
                    "duration_ms": Value::Null,
                    "isrc": Value::Null,
                    "score": score_version(
                        reference,
                        Signaux { titre: t, isrc: None, duree_ms: None, annee: None },
                    ),
                }));
            }
        }
    }

    // Meme regle que le vivier local : le score decide, et la cle de
    // departage rend l'ordre total — donc reproductible d'un appel a l'autre
    // et d'une page a l'autre, quel que soit l'ordre ou les services ont
    // repondu.
    classer_par_score(&mut trouvees, |v| {
        (
            v["service"].as_str().unwrap_or_default().to_string(),
            v["title"].as_str().unwrap_or_default().to_lowercase(),
            v["source_id"].as_str().unwrap_or_default().to_string(),
        )
    });
    trouvees
}

#[cfg(test)]
mod tests {
    use super::{
        ClasseVersion, POINTS_ANNEE_DIFFERENTE, POINTS_DUREE_PROCHE, POINTS_DUREE_QUASI_EGALE,
        POINTS_DUREE_VOISINE, POINTS_ISRC_IDENTIQUE, POINTS_TITRE_EXACT, POINTS_TITRE_SUFFIXE,
        Reference, Signaux, classer_version, limite_recherche_versions, predicat_rapprochement,
        predicat_titres_equivalents, requetes_versions, score_version, titres_equivalents,
    };

    fn reference(titre: &str) -> Reference {
        Reference {
            titre: titre.to_string(),
            artiste: "Sade".to_string(),
            album: "Diamond Life".to_string(),
            ..Default::default()
        }
    }

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
    }

    /// ⭐ Le cas de Gros Bidon (#2372, fil 1627) : un REMASTER, nomme avec un
    /// tiret comme le font Qobuz, Tidal et Deezer, est une version du titre de
    /// base.
    ///
    /// Ce test RENVERSE une assertion qui existait ici — `assert!(
    /// !titres_equivalents("Heroes", "Heroes - Live"))`. Ce renversement est
    /// l'objet du lot : sans le tiret, « Autres versions » ratait la
    /// convention de nommage la plus repandue de ce qu'elle cherche.
    #[test]
    fn un_remaster_avec_tiret_est_une_version_du_titre_de_base() {
        assert!(titres_equivalents(
            "Smooth Operator",
            "Smooth Operator - 2011 Remastered"
        ));
        assert!(titres_equivalents(
            "Smooth Operator - 2011 Remastered",
            "smooth operator"
        ));
        assert!(titres_equivalents("Heroes", "Heroes - Live"));
    }

    /// Le tiret ne devient PAS un joint lexical : il n'ouvre un suffixe que
    /// borne d'espaces des deux cotes.
    ///
    /// `Cross-Eyed Mary` reste hors de portee de `Cross`, et le piege des
    /// titres inclus nomme par FabienM (`Somebody` vs `Somebody To Love`) le
    /// reste aussi — la frontiere n'est pas un `contains`.
    #[test]
    fn le_tiret_nu_n_ouvre_aucun_suffixe() {
        assert!(!titres_equivalents("Cross", "Cross-Eyed Mary"));
        assert!(!titres_equivalents("Cross", "Cross -Eyed Mary"));
        assert!(!titres_equivalents("Cross", "Cross- Eyed Mary"));
        assert!(!titres_equivalents("Somebody", "Somebody To Love"));
    }

    #[test]
    fn le_predicat_titre_n_utilise_aucun_joker_like() {
        let p = predicat_titres_equivalents("t.title", "$1");
        assert!(p.contains("SUBSTR"), "frontiere de suffixe absente : {p}");
        assert!(
            !p.contains("LIKE"),
            "un titre ne doit pas devenir un motif : {p}"
        );
    }

    /// ⭐ La contre-epreuve du lot : les TROIS delimiteurs sont dans le SQL,
    /// avec la BONNE longueur pour chacun.
    ///
    /// Sans ce garde, la route locale (SQL) et la route streaming (Rust)
    /// pourraient reconnaitre deux jeux de titres differents pour le meme
    /// morceau. Un ` - ` compare sur 2 caracteres au lieu de 3 ne trouverait
    /// jamais rien, et le test Rust ci-dessus resterait vert.
    #[test]
    fn le_predicat_sql_porte_les_trois_delimiteurs_avec_leur_longueur() {
        let p = predicat_titres_equivalents("t.title", "$1");
        for (delimiteur, longueur) in [(" (", 2), (" [", 2), (" - ", 3)] {
            let attendu = format!(", {longueur}) = '{delimiteur}'");
            assert!(
                p.contains(&attendu),
                "delimiteur {delimiteur:?} sur {longueur} caracteres absent : {p}"
            );
        }
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

    /// ⭐ Le defaut que FabienM decrit depuis le 27/08 : la requete envoyee au
    /// service ne nommait pas l'artiste.
    ///
    /// « j'obtiens 50 resultats de titres mais aucun de l'artiste "Depeche
    /// Mode" » (fil 1611, 29/08). Le vivier elargi de #2790 ne pouvait rien y
    /// faire : c'est la requete qu'il fallait qualifier.
    #[test]
    fn la_requete_streaming_nomme_l_artiste() {
        let r = requetes_versions("Somebody", "Depeche Mode");
        assert_eq!(
            r.first().map(String::as_str),
            Some("Depeche Mode Somebody"),
            "la requete qualifiee doit venir en TETE : {r:?}"
        );
        assert!(
            r.iter().any(|q| q == "Somebody"),
            "le titre seul reste demande — c'est lui qui trouve les reprises : {r:?}"
        );
        assert_eq!(r.len(), 2, "deux requetes, pas plus : {r:?}");
    }

    /// Sans artiste connu il ne reste qu'une requete : sans ce repli, un
    /// morceau sans tag d'artiste enverrait « ` Somebody` » avec une espace en
    /// tete, et la reponse du service ne vaudrait rien.
    #[test]
    fn sans_artiste_une_seule_requete() {
        assert_eq!(requetes_versions("Somebody", "  "), vec!["Somebody"]);
        assert_eq!(requetes_versions("Somebody", ""), vec!["Somebody"]);
    }

    /// L'ISRC domine tout le reste : c'est le seul signal non textuel.
    #[test]
    fn l_isrc_identique_domine_le_score() {
        let mut r = reference("Smooth Operator");
        r.isrc = Some("GBAAA8400001".into());
        r.duree_ms = Some(291_000);
        let meme_enregistrement = score_version(
            &r,
            Signaux {
                titre: "Smooth Operator - 2011 Remastered",
                // Meme ISRC, ecrit avec les tirets de la norme.
                isrc: Some("gb-aaa-84-00001"),
                duree_ms: Some(291_000),
                annee: None,
            },
        );
        let autre_enregistrement = score_version(
            &r,
            Signaux {
                titre: "Smooth Operator - 2011 Remastered",
                isrc: Some("USAAA0000001"),
                duree_ms: Some(291_000),
                annee: None,
            },
        );
        assert_eq!(
            meme_enregistrement,
            POINTS_ISRC_IDENTIQUE + POINTS_DUREE_QUASI_EGALE + POINTS_TITRE_SUFFIXE
        );
        assert_eq!(
            autre_enregistrement,
            POINTS_DUREE_QUASI_EGALE + POINTS_TITRE_SUFFIXE
        );
        assert!(meme_enregistrement > autre_enregistrement);
    }

    /// Les paliers de duree se suivent, et un ecart enorme ne rapporte rien —
    /// il ne RETIRE rien non plus : le score classe, il n'exclut pas.
    #[test]
    fn les_paliers_de_duree_sont_ordonnes() {
        let mut r = reference("Smooth Operator");
        r.duree_ms = Some(300_000);
        let points = |duree: i64| {
            score_version(
                &r,
                Signaux {
                    titre: "Smooth Operator",
                    isrc: None,
                    duree_ms: Some(duree),
                    annee: None,
                },
            ) - POINTS_TITRE_EXACT
        };
        assert_eq!(points(301_000), POINTS_DUREE_QUASI_EGALE);
        assert_eq!(points(295_000), POINTS_DUREE_PROCHE);
        assert_eq!(points(340_000), POINTS_DUREE_VOISINE);
        assert_eq!(points(600_000), 0);
        assert!(POINTS_DUREE_QUASI_EGALE > POINTS_DUREE_PROCHE);
        assert!(POINTS_DUREE_PROCHE > POINTS_DUREE_VOISINE);
    }

    /// Un signal ABSENT ne rapporte rien et ne coute rien : c'est la
    /// difference entre la route par piste (qui a les signaux) et la section
    /// d'accueil (qui n'en a aucun), et les deux doivent rester utilisables.
    #[test]
    fn un_signal_absent_ne_penalise_pas() {
        let sans_signaux = score_version(
            &reference("Smooth Operator"),
            Signaux {
                titre: "Smooth Operator",
                isrc: None,
                duree_ms: None,
                annee: None,
            },
        );
        assert_eq!(sans_signaux, POINTS_TITRE_EXACT);
    }

    /// Une AUTRE annee apprend quelque chose ; la meme annee n'apprend rien.
    #[test]
    fn une_autre_annee_rapporte_des_points() {
        let mut r = reference("Smooth Operator");
        r.annee = Some(1984);
        let points = |annee: i64| {
            score_version(
                &r,
                Signaux {
                    titre: "Smooth Operator",
                    isrc: None,
                    duree_ms: None,
                    annee: Some(annee),
                },
            ) - POINTS_TITRE_EXACT
        };
        assert_eq!(points(2011), POINTS_ANNEE_DIFFERENTE);
        assert_eq!(points(1984), 0);
    }

    /// Le titre exact passe devant le titre suffixe : les deux entrent, mais
    /// l'ordre n'est plus celui du hasard.
    #[test]
    fn le_titre_exact_passe_devant_le_titre_suffixe() {
        let r = reference("Smooth Operator");
        let exact = score_version(
            &r,
            Signaux {
                titre: "smooth operator",
                ..Default::default()
            },
        );
        let suffixe = score_version(
            &r,
            Signaux {
                titre: "Smooth Operator - 2011 Remastered",
                ..Default::default()
            },
        );
        assert_eq!(exact, POINTS_TITRE_EXACT);
        assert_eq!(suffixe, POINTS_TITRE_SUFFIXE);
        assert!(exact > suffixe);
    }
}

/// Garde de SITE pour la branche Bandcamp de [`versions_streaming`].
///
/// # Pourquoi une garde statique ici, et nulle part ailleurs
///
/// Les trois routes sont eprouvees sur le CORPS de leur reponse par
/// `tests/sources_trois_routes_i3226.rs`, qui est la bonne facon de faire.
/// Cette branche-ci echappe a ce banc, et pour une raison de fond :
/// `tune_bandcamp::rechercher_pistes` est un APPEL RESEAU SORTANT, hors du
/// registre et sans authentification. On ne peut donc pas lui substituer une
/// doublure comme on le fait pour `qobuz` et `tidal`.
///
/// Le banc a d'abord tourne avec un titre reel : Bandcamp y a repondu neuf
/// entrees imprevues, et l'essai devenait un pari sur ce que le site servait
/// ce jour-la. Il tourne desormais sur un titre invente — donc Bandcamp n'y
/// rend jamais rien, et une assertion « Bandcamp est absent » y serait verte
/// contre rien.
///
/// Cette garde-la lit le fichier de production et exige que la boucle Bandcamp
/// soit precedee de sa consultation de `sources`. Sans elle, `sources=qobuz`
/// paierait un appel sortant vers un service que personne n'a demande, et le
/// rendrait dans `streaming`.
#[cfg(test)]
mod garde_de_site_bandcamp {
    /// La partie PRODUCTION du fichier : tout ce qui precede le premier
    /// `#[cfg(test)]`.
    ///
    /// `include_str!` rend le fichier ENTIER, ces modules de test compris — ou
    /// le motif cherche apparait en toutes lettres, dans le code de la garde
    /// comme dans sa contre-epreuve. Sans cette coupe, la garde se prouverait
    /// elle-meme et resterait verte quel que soit le code de production.
    fn production(source: &str) -> &str {
        source
            .split_once("\n#[cfg(test)]")
            .map(|(avant, _)| avant)
            .unwrap_or(source)
    }

    /// Le motif exige : la boucle Bandcamp consulte `sources`.
    const MOTIF: &str = "service_demande(\"bandcamp\")";

    /// `versions.rs`, ligne ~629 : la boucle `#[cfg(feature = "bandcamp")] for
    /// requete in …` doit etre gouvernee par `filtre.service_demande`.
    #[test]
    fn le_filtre_de_sources_couvre_la_branche_bandcamp() {
        let prod = production(include_str!("versions.rs"));
        assert!(
            prod.contains(MOTIF),
            "tune-server/src/routes/versions.rs — la boucle Bandcamp de \
             `versions_streaming` (cherchez `#[cfg(feature = \"bandcamp\")]`, \
             ligne ~629) ne consulte plus `sources` : `{MOTIF}` a disparu du \
             code de production. `sources=qobuz` paierait alors un appel \
             reseau sortant vers Bandcamp — un service que personne n'a \
             demande — et rendrait ses pistes dans `streaming`. Le banc \
             `tests/sources_trois_routes_i3226.rs` ne peut PAS attraper cela : \
             il tourne sur un titre invente, contre lequel Bandcamp ne rend \
             jamais rien."
        );
        // La garde doit designer la BOUCLE, pas n'importe quelle mention :
        // le motif doit se trouver dans `versions_streaming`, apres la boucle
        // du registre.
        let (avant_bandcamp, _) = prod
            .split_once("#[cfg(feature = \"bandcamp\")]")
            .expect("la branche Bandcamp a disparu de versions.rs");
        assert!(
            !avant_bandcamp.is_empty(),
            "la branche Bandcamp doit rester dans `versions_streaming`"
        );
    }

    /// Contre-epreuve de la garde elle-meme, dans les DEUX sens.
    #[test]
    fn la_garde_ne_se_prouve_pas_sur_son_propre_module_de_test() {
        // 1. Elle doit VOIR le motif dans un code de production qui l'a.
        let avec = "fn f() {\n    if filtre.service_demande(\"bandcamp\") {}\n}\n";
        assert!(
            production(avec).contains(MOTIF),
            "la garde doit reconnaitre le motif quand il est present"
        );

        // 2. Elle ne doit PAS le voir quand il n'existe que dans le module de
        //    test — c'est exactement la situation de CE fichier, ou le motif
        //    apparait trois fois ci-dessus.
        let seulement_en_test = format!(
            "fn f() {{}}\n\n#[cfg(test)]\nmod t {{\n    const M: &str = \"{MOTIF}\";\n}}\n"
        );
        assert!(
            !production(&seulement_en_test).contains(MOTIF),
            "un motif present dans le SEUL module de test ne doit pas compter \
             pour du code de production — sans cette coupe, cette garde serait \
             verte contre un `versions.rs` dont la boucle Bandcamp ne filtre \
             plus rien"
        );

        // 3. Et un code de production nu ne doit pas passer.
        assert!(
            !production("fn f() {\n    for r in &requetes {}\n}\n").contains(MOTIF),
            "une boucle Bandcamp nue ne doit pas passer pour filtree"
        );
    }
}
