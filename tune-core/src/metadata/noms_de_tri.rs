//! #6773 — le nom de tri des artistes qui n'en ont pas.
//!
//! Signalé par Xavier Joly (fil 2383, rc7) : « André Ceccarelli », « Maurice
//! André » et « Angélique Kidjo » restaient rangés à leur prénom dans le tri
//! « par nom de famille ». Depuis #6474, ce tri suit le NOM DE TRI de la
//! fiche, et lui seul. Ce nom vient d'une balise (ARTISTSORT,
//! ALBUMARTISTSORT), d'une saisie, ou du `sort-name` MusicBrainz posé à
//! l'identification d'un pressage. Deux trous :
//!
//! 1. **Les artistes déjà identifiés** (MBID connu) avant la rc7 : le nom de
//!    tri MusicBrainz n'est posé qu'à une NOUVELLE identification. Le
//!    rattrapage ([`rattraper_par_musicbrainz`]) lit la fiche MusicBrainz de
//!    chacun (`GET /artist/{mbid}`, une requête au créneau du limiteur
//!    partagé) et pose son `sort-name`, avec les règles de
//!    [`super::artistes_du_pressage::nom_de_tri_des_credits`] : rien
//!    d'inventé, pseudo-artistes exclus, et la fiche MusicBrainz doit NOMMER
//!    l'artiste local.
//! 2. **Les artistes sans MBID ni balise** (go de Bertrand du 22/10) : un nom
//!    de tri « Nom, Prénom » DÉDUIT ([`nom_de_tri_deduit`]), pour une
//!    personne seulement. Sa provenance est `deduit`
//!    ([`SOURCE_TRI_DEDUIT`]) : une balise ou MusicBrainz le remplacent, une
//!    saisie le remplace et n'est plus jamais touchée.
//!
//! Ni l'un ni l'autre n'écrase un nom de tri saisi à la main ou lu dans une
//! balise : les écritures passent par les requêtes conditionnelles de
//! [`crate::db::artist_repo`].

use std::sync::Arc;

use serde_json::Value;
use tracing::{debug, info};

use crate::db::artist_repo::{SOURCE_TRI_DEDUIT, cle_artiste};
use crate::db::backend::{DbBackend, ToSqlValue};
use crate::db::engine::fold_diacritics;
use crate::metadata::artistes_du_pressage::MBID_PSEUDO_ARTISTES;
use crate::metadata::musicbrainz_release::{RefusMusicBrainz, est_un_artiste_fictif};

/// Une fiche à rattraper : `(id, nom, mbid)`.
pub type FicheARattraper = (i64, String, String);

/// Le bilan d'un tour du rattrapage MusicBrainz.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct BilanRattrapage {
    /// Fiches lues.
    pub traitees: usize,
    /// Noms de tri posés.
    pub poses: usize,
    /// Fiches MusicBrainz sans nom de tri, ou qui ne nomment pas l'artiste
    /// local : rien n'est écrit.
    pub ecartees: usize,
    /// Refus de MusicBrainz (réseau, statut, corps illisible).
    pub pannes: usize,
    /// La dernière fiche lue : le curseur du tour suivant.
    pub dernier_id: i64,
    /// Le tour a atteint la fin de la liste.
    pub fin_de_liste: bool,
}

fn est_pseudo(mbid: &str) -> bool {
    MBID_PSEUDO_ARTISTES.contains(&mbid.trim().to_lowercase().as_str())
}

/// Les fiches à rattraper après `apres_id` : un MBID, et un nom de tri vide
/// ou seulement déduit. Une balise, une saisie ou un nom déjà tiré de
/// MusicBrainz ne sont pas candidats.
pub fn candidats_musicbrainz(
    backend: &Arc<dyn DbBackend>,
    apres_id: i64,
    limite: usize,
) -> Result<Vec<FicheARattraper>, String> {
    let limite = limite.max(1) as i64;
    let lignes = backend.query_many(
        &format!(
            "SELECT id, name, musicbrainz_id FROM artists \
             WHERE id > ? AND musicbrainz_id IS NOT NULL AND TRIM(musicbrainz_id) <> '' \
               AND (sort_name IS NULL OR TRIM(sort_name) = '' \
                    OR sort_name_source = '{SOURCE_TRI_DEDUIT}') \
             ORDER BY id LIMIT ?"
        ),
        &[&apres_id as &dyn ToSqlValue, &limite as &dyn ToSqlValue],
    )?;
    Ok(lignes
        .into_iter()
        .filter_map(|l| {
            let id = l.first()?.as_i64()?;
            let nom = l.get(1)?.as_string().unwrap_or_default();
            let mbid = l.get(2)?.as_string()?.trim().to_lowercase();
            Some((id, nom, mbid))
        })
        .collect())
}

/// Le nom de tri qu'une fiche MusicBrainz (`GET /artist/{mbid}?inc=aliases`)
/// donne à l'artiste local `nom`, ou rien.
///
/// Mêmes règles que [`super::artistes_du_pressage::nom_de_tri_des_credits`] :
/// un pseudo-artiste, un artiste fictif ou un `sort-name` vide ne donnent
/// rien, et la fiche doit NOMMER l'artiste local — par son nom, son nom de
/// tri (aussi lu « Prénom Nom ») ou un alias, à la clé [`cle_artiste`] près.
/// Un MBID mal rattaché ne prête donc pas son nom de tri à un autre artiste.
pub fn nom_de_tri_de_la_fiche(nom: &str, mbid: &str, fiche: &Value) -> Option<String> {
    if est_pseudo(mbid) || est_un_artiste_fictif(nom) {
        return None;
    }
    let texte = |v: &Value, k: &str| -> Option<String> {
        v.get(k)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let tri = texte(fiche, "sort-name")?;
    let mut noms: Vec<String> = [texte(fiche, "name"), Some(tri.clone())]
        .into_iter()
        .flatten()
        .collect();
    if let Some(alias) = fiche.get("aliases").and_then(Value::as_array) {
        for a in alias {
            noms.extend(
                [texte(a, "name"), texte(a, "sort-name")]
                    .into_iter()
                    .flatten(),
            );
        }
    }
    let cle = cle_artiste(nom);
    let nomme = noms.iter().any(|n| {
        cle_artiste(n) == cle
            || n.split_once(',')
                .map(|(nom_de_famille, prenoms)| {
                    cle_artiste(&format!("{} {}", prenoms.trim(), nom_de_famille.trim())) == cle
                })
                .unwrap_or(false)
    });
    nomme.then_some(tri)
}

/// Pose le nom de tri MusicBrainz `tri` sur la fiche `id`, si elle n'en a
/// pas, ou si le sien est déduit ou vient déjà de MusicBrainz. Une balise ou
/// une saisie n'est jamais remplacée.
fn poser_le_tri_musicbrainz(
    backend: &Arc<dyn DbBackend>,
    id: i64,
    tri: &str,
) -> Result<bool, String> {
    crate::db::artist_repo::ArtistRepo::with_backend(backend.clone())
        .poser_nom_de_tri_musicbrainz(id, tri)
        .map_err(|e| e.to_string())
}

/// Un tour du rattrapage MusicBrainz : au plus `limite` fiches après
/// `apres_id`. `interroger` lit une fiche MusicBrainz par son MBID (le
/// limiteur partagé et le User-Agent de Tune en production, une réponse
/// factice au banc).
///
/// Une panne n'écrit rien et ne fait pas avancer le curseur au-delà de la
/// fiche en panne : le tour s'arrête, et le suivant la reprend.
pub async fn rattraper_par_musicbrainz<F, Fut>(
    backend: &Arc<dyn DbBackend>,
    apres_id: i64,
    limite: usize,
    interroger: F,
) -> Result<BilanRattrapage, String>
where
    F: Fn(String) -> Fut,
    Fut: std::future::Future<Output = Result<Value, RefusMusicBrainz>>,
{
    let fiches = candidats_musicbrainz(backend, apres_id, limite)?;
    let mut bilan = BilanRattrapage {
        dernier_id: apres_id,
        fin_de_liste: fiches.len() < limite.max(1),
        ..Default::default()
    };
    for (id, nom, mbid) in fiches {
        if est_pseudo(&mbid) || est_un_artiste_fictif(&nom) {
            bilan.traitees += 1;
            bilan.ecartees += 1;
            bilan.dernier_id = id;
            continue;
        }
        match interroger(mbid.clone()).await {
            Ok(fiche) => {
                bilan.traitees += 1;
                bilan.dernier_id = id;
                match nom_de_tri_de_la_fiche(&nom, &mbid, &fiche) {
                    Some(tri) if poser_le_tri_musicbrainz(backend, id, &tri)? => {
                        debug!(artiste_id = id, %tri, "nom_de_tri_musicbrainz_rattrape");
                        bilan.poses += 1;
                    }
                    _ => bilan.ecartees += 1,
                }
            }
            Err(e) => {
                debug!(artiste_id = id, erreur = ?e, "nom_de_tri_rattrapage_panne");
                bilan.pannes += 1;
                bilan.fin_de_liste = false;
                break;
            }
        }
    }
    Ok(bilan)
}

/// La fiche MusicBrainz d'un artiste, alias compris : une requête, au créneau
/// du limiteur partagé, avec le User-Agent de Tune.
pub async fn interroger_la_fiche_musicbrainz(mbid: String) -> Result<Value, RefusMusicBrainz> {
    use crate::metadata::musicbrainz_release::{
        MB_UA, constater_reponse_musicbrainz, rate_limit_delay,
    };
    rate_limit_delay().await;
    let resp = crate::http::client::shared()
        .get(format!("https://musicbrainz.org/ws/2/artist/{mbid}"))
        .query(&[("inc", "aliases"), ("fmt", "json")])
        .header("User-Agent", MB_UA)
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await
        .map_err(|_| RefusMusicBrainz::Transport)?;
    constater_reponse_musicbrainz(&resp);
    if !resp.status().is_success() {
        return Err(RefusMusicBrainz::Statut(resp.status().as_u16()));
    }
    resp.json()
        .await
        .map_err(|_| RefusMusicBrainz::CorpsIllisible)
}

// ── Déduction « Nom, Prénom » (go de Bertrand du 22/10) ─────────────────

/// Les particules d'un nom de famille : elles suivent les prénoms dans le
/// nom de tri, à la manière de MusicBrainz (« Beethoven, Ludwig van »).
const PARTICULES: &[&str] = &[
    "de", "du", "des", "d'", "da", "das", "do", "dos", "di", "del", "della", "van", "von", "der",
    "den", "ter", "ten", "le", "la", "zu",
];

/// Les mots qui disent un GROUPE (ou une formation) : un seul suffit pour
/// qu'aucun nom de tri ne soit déduit.
const MOTS_DE_GROUPE: &[&str] = &[
    "and",
    "et",
    "y",
    "und",
    "e",
    "the",
    "les",
    "los",
    "las",
    "die",
    "il",
    "i",
    "orchestra",
    "orchestre",
    "orquesta",
    "orchester",
    "orkest",
    "orchestral",
    "philharmonic",
    "philharmonique",
    "philharmoniker",
    "symphony",
    "symphonique",
    "symphonic",
    "quartet",
    "quartett",
    "quatuor",
    "quartetto",
    "quintet",
    "quintette",
    "sextet",
    "septet",
    "octet",
    "trio",
    "duo",
    "band",
    "ensemble",
    "choir",
    "choeur",
    "chœur",
    "coro",
    "chor",
    "chorale",
    "singers",
    "brothers",
    "sisters",
    "freres",
    "frères",
    "soeurs",
    "sœurs",
    "family",
    "famille",
    "project",
    "projet",
    "group",
    "groupe",
    "crew",
    "collective",
    "collectif",
    "gang",
    "club",
    "boys",
    "girls",
    "sound",
    "sounds",
    "experience",
    "players",
    "consort",
    "feat",
    "featuring",
    "ft",
    "vs",
    "with",
    "avec",
    "presents",
    "dj",
    "mc",
    "big",
    "orch",
    "jazz",
    "quartette",
    "allstars",
    "all-stars",
    "combo",
    "unit",
    "machine",
    "society",
    "company",
    "compagnie",
    "fanfare",
    "big-band",
    "bigband",
    "sinfonietta",
    "camerata",
    "academy",
    "academie",
    "académie",
    "kammerorchester",
    "soloists",
    "solistes",
    "virtuosi",
    "musici",
    "baroque",
    "kings",
    "queens",
    "children",
    "people",
    "orchestras",
];

/// Des prénoms courants (sans accents, en minuscules). La déduction n'a
/// lieu que si TOUS les mots avant le nom de famille en sont : c'est la
/// garde contre les groupes (« Pink Floyd », « Daft Punk », « Iron Maiden »
/// n'ont pas de prénom en tête). Une liste fermée, volontairement : un
/// prénom absent ne déduit rien, il ne range pas mal.
const PRENOMS: &[&str] = &[
    // Français
    "adele",
    "adrien",
    "agnes",
    "alain",
    "alban",
    "albert",
    "alexandre",
    "alexandra",
    "alexis",
    "alice",
    "aline",
    "amandine",
    "amelie",
    "anais",
    "andre",
    "andree",
    "angele",
    "angelique",
    "anne",
    "annie",
    "antoine",
    "arielle",
    "arnaud",
    "arthur",
    "aurelie",
    "aurelien",
    "barbara",
    "benjamin",
    "benoit",
    "bernard",
    "bertrand",
    "brigitte",
    "bruno",
    "camille",
    "carla",
    "caroline",
    "catherine",
    "cecile",
    "celine",
    "charles",
    "charlotte",
    "christian",
    "christine",
    "christophe",
    "claire",
    "claude",
    "claudine",
    "clement",
    "colette",
    "corinne",
    "cyril",
    "damien",
    "daniel",
    "danielle",
    "david",
    "denis",
    "didier",
    "dominique",
    "edith",
    "edouard",
    "elisabeth",
    "elise",
    "elodie",
    "emile",
    "emilie",
    "emma",
    "emmanuel",
    "emmanuelle",
    "eric",
    "etienne",
    "eugene",
    "fabien",
    "fabrice",
    "fanny",
    "felix",
    "florence",
    "florent",
    "francis",
    "francois",
    "francoise",
    "frederic",
    "gabriel",
    "gaston",
    "genevieve",
    "georges",
    "gerard",
    "gilbert",
    "gilles",
    "guillaume",
    "guy",
    "helene",
    "henri",
    "herve",
    "hugues",
    "isabelle",
    "jacques",
    "jacqueline",
    "jean",
    "jeanne",
    "jerome",
    "joel",
    "josephine",
    "josiane",
    "julie",
    "julien",
    "juliette",
    "laetitia",
    "laura",
    "laurence",
    "laurent",
    "leo",
    "leon",
    "lionel",
    "louis",
    "louise",
    "luc",
    "lucie",
    "lucien",
    "manu",
    "marc",
    "marcel",
    "margaux",
    "marguerite",
    "marie",
    "marine",
    "mathieu",
    "matthieu",
    "maurice",
    "maxime",
    "melanie",
    "michel",
    "micheline",
    "michele",
    "mireille",
    "monique",
    "nadine",
    "nathalie",
    "nicolas",
    "nicole",
    "noel",
    "odile",
    "olivier",
    "pascal",
    "pascale",
    "patrice",
    "patricia",
    "patrick",
    "paul",
    "pauline",
    "philippe",
    "pierre",
    "rene",
    "renaud",
    "richard",
    "robert",
    "roger",
    "romain",
    "sabine",
    "sacha",
    "sandrine",
    "sebastien",
    "serge",
    "simone",
    "solange",
    "sophie",
    "stephane",
    "stephanie",
    "suzanne",
    "sylvain",
    "sylvie",
    "theo",
    "therese",
    "thierry",
    "thomas",
    "valerie",
    "veronique",
    "vincent",
    "virginie",
    "xavier",
    "yann",
    "yannick",
    "yves",
    "yvette",
    "zaz",
    // Anglais
    "aaron",
    "adam",
    "alan",
    "alex",
    "alicia",
    "alison",
    "amanda",
    "amy",
    "andrew",
    "andy",
    "angela",
    "ann",
    "anna",
    "anthony",
    "art",
    "barry",
    "ben",
    "benny",
    "betty",
    "bill",
    "billie",
    "billy",
    "bob",
    "bobby",
    "bonnie",
    "brad",
    "brandon",
    "brian",
    "bruce",
    "bud",
    "burt",
    "carl",
    "carly",
    "carol",
    "carole",
    "cecil",
    "charlie",
    "chet",
    "chick",
    "chris",
    "christina",
    "christopher",
    "chuck",
    "cindy",
    "cliff",
    "clifford",
    "cole",
    "count",
    "curtis",
    "cyndi",
    "dan",
    "dave",
    "dean",
    "debbie",
    "dennis",
    "diana",
    "diane",
    "dinah",
    "dizzy",
    "don",
    "donald",
    "donna",
    "donny",
    "dorothy",
    "doug",
    "duke",
    "dusty",
    "dwight",
    "ed",
    "eddie",
    "edward",
    "ella",
    "ellie",
    "elton",
    "elvis",
    "emily",
    "emmylou",
    "eric",
    "erykah",
    "etta",
    "eva",
    "frank",
    "frankie",
    "fred",
    "freddie",
    "gary",
    "gene",
    "george",
    "gil",
    "glen",
    "glenn",
    "gloria",
    "gordon",
    "grace",
    "graham",
    "greg",
    "gregory",
    "hank",
    "harry",
    "helen",
    "herbie",
    "horace",
    "howard",
    "ian",
    "isaac",
    "jack",
    "jackie",
    "jackson",
    "jake",
    "james",
    "jamie",
    "jane",
    "janet",
    "janis",
    "jason",
    "jeff",
    "jeffrey",
    "jennifer",
    "jerry",
    "jessica",
    "jill",
    "jim",
    "jimi",
    "jimmy",
    "joan",
    "joanna",
    "joe",
    "joey",
    "john",
    "johnny",
    "jon",
    "joni",
    "jonathan",
    "joseph",
    "josh",
    "joshua",
    "judy",
    "julia",
    "julian",
    "justin",
    "karen",
    "kate",
    "katherine",
    "kathy",
    "katie",
    "keith",
    "kelly",
    "ken",
    "kenny",
    "kevin",
    "kim",
    "kurt",
    "lana",
    "larry",
    "lee",
    "lena",
    "lenny",
    "leonard",
    "lester",
    "lewis",
    "linda",
    "lionel",
    "lisa",
    "liz",
    "lou",
    "lucy",
    "lyle",
    "madeleine",
    "margaret",
    "maria",
    "mariah",
    "mark",
    "marvin",
    "mary",
    "matt",
    "matthew",
    "max",
    "michael",
    "mick",
    "mike",
    "miles",
    "nancy",
    "nat",
    "neil",
    "nick",
    "nina",
    "norah",
    "oliver",
    "oscar",
    "otis",
    "patsy",
    "patti",
    "peggy",
    "pete",
    "peter",
    "phil",
    "philip",
    "quincy",
    "rachel",
    "randy",
    "ray",
    "rebecca",
    "rick",
    "ricky",
    "rita",
    "rob",
    "robbie",
    "robin",
    "rod",
    "ron",
    "ronald",
    "ronnie",
    "rosemary",
    "roy",
    "ruby",
    "russell",
    "ruth",
    "ryan",
    "sam",
    "samuel",
    "sandy",
    "sarah",
    "scott",
    "sean",
    "sheryl",
    "simon",
    "sonny",
    "stan",
    "stanley",
    "stephen",
    "steve",
    "steven",
    "stevie",
    "stuart",
    "susan",
    "suzanne",
    "taylor",
    "ted",
    "terry",
    "thelonious",
    "tim",
    "timothy",
    "tina",
    "toby",
    "todd",
    "tom",
    "tommy",
    "tony",
    "tracy",
    "van",
    "victoria",
    "walter",
    "wayne",
    "wes",
    "whitney",
    "william",
    "willie",
    "woody",
    "wynton",
    // Italien, espagnol, portugais
    "adriano",
    "alberto",
    "alejandro",
    "alessandro",
    "andrea",
    "angelo",
    "antonio",
    "carlos",
    "carmen",
    "caterina",
    "cesaria",
    "chico",
    "claudio",
    "cristina",
    "dario",
    "diego",
    "eduardo",
    "elena",
    "elis",
    "enrico",
    "enrique",
    "ennio",
    "ernesto",
    "fabrizio",
    "federico",
    "fernando",
    "francesco",
    "franco",
    "gilberto",
    "gino",
    "giorgio",
    "giovanni",
    "giuseppe",
    "gloria",
    "ivano",
    "javier",
    "joao",
    "jorge",
    "jose",
    "juan",
    "laura",
    "lorenzo",
    "luca",
    "lucia",
    "lucio",
    "luigi",
    "luis",
    "luisa",
    "manuel",
    "marco",
    "mario",
    "marisa",
    "milton",
    "mina",
    "miguel",
    "nicola",
    "ornella",
    "pablo",
    "paolo",
    "pedro",
    "pino",
    "raffaella",
    "ricardo",
    "roberto",
    "rosa",
    "salvatore",
    "sergio",
    "silvio",
    "stefano",
    "tiziano",
    "toquinho",
    "umberto",
    "vinicius",
    "vittorio",
    // Allemand, néerlandais, nordique, slave
    "anja",
    "anton",
    "bjorn",
    "dieter",
    "dietrich",
    "erik",
    "franz",
    "friedrich",
    "gustav",
    "hans",
    "heinrich",
    "helmut",
    "herbert",
    "ingrid",
    "jan",
    "jens",
    "johann",
    "johannes",
    "jurgen",
    "karl",
    "klaus",
    "lars",
    "ludwig",
    "magnus",
    "nils",
    "olaf",
    "sven",
    "ulrich",
    "wolfgang",
    "agnetha",
    "anni",
    "arvo",
    "bela",
    "dmitri",
    "frederick",
    "igor",
    "leos",
    "sergei",
    "sviatoslav",
    "vladimir",
    // Afrique, monde arabe
    "ali",
    "amadou",
    "aziz",
    "baaba",
    "fela",
    "habib",
    "ibrahim",
    "ismael",
    "khaled",
    "manu",
    "mariam",
    "miriam",
    "mory",
    "oumou",
    "rachid",
    "salif",
    "souad",
    "youssou",
];

/// Un mot écrit en nom propre : une majuscule en tête, des lettres ensuite
/// (trait d'union et apostrophe admis).
fn en_nom_propre(mot: &str) -> bool {
    let mut lettres = mot.chars();
    let Some(premiere) = lettres.next() else {
        return false;
    };
    premiere.is_uppercase()
        && mot
            .chars()
            .all(|c| c.is_alphabetic() || c == '-' || c == '\'' || c == '\u{2019}')
}

fn est_prenom(mot: &str) -> bool {
    let plat = fold_diacritics(mot).to_lowercase();
    // « Jean-Jacques », « Marie-Claire » : chaque partie est un prénom.
    plat.split('-')
        .all(|p| !p.is_empty() && PRENOMS.contains(&p))
}

/// Le nom de tri « Nom, Prénom » déduit pour une PERSONNE, ou rien.
///
/// Go de Bertrand du 22/10 (#6773). La garde est volontairement étroite :
/// deux à quatre mots ; aucun mot de groupe ([`MOTS_DE_GROUPE`]) ni chiffre
/// ni signe (« & », « / », virgule, parenthèse, point d'initiale) ; tous les
/// mots avant le nom de famille sont des prénoms connus ([`PRENOMS`]), les
/// particules (« van », « de ») se rangent après eux, comme chez MusicBrainz.
/// Hors de ce cas, rien n'est déduit : un nom laissé tel quel se range comme
/// avant, alors qu'un groupe inversé se rangerait mal.
///
/// André Ceccarelli → « Ceccarelli, André » ; Maurice André → « André,
/// Maurice » ; Ludwig van Beethoven → « Beethoven, Ludwig van ».
pub fn nom_de_tri_deduit(nom: &str) -> Option<String> {
    let nom = nom.trim();
    if nom.is_empty() || est_un_artiste_fictif(nom) {
        return None;
    }
    if nom.chars().any(|c| {
        c.is_numeric() || !(c.is_alphabetic() || c.is_whitespace() || "-'\u{2019}".contains(c))
    }) {
        return None;
    }
    let mots: Vec<&str> = nom.split_whitespace().collect();
    if !(2..=4).contains(&mots.len()) {
        return None;
    }
    let bas = |m: &str| fold_diacritics(m).to_lowercase();
    if mots.iter().any(|m| {
        let b = bas(m);
        MOTS_DE_GROUPE.contains(&b.as_str())
    }) {
        return None;
    }
    let nom_de_famille = *mots.last()?;
    if !en_nom_propre(nom_de_famille) {
        return None;
    }
    let mut fin_des_prenoms = mots.len() - 1;
    while fin_des_prenoms > 1 && PARTICULES.contains(&bas(mots[fin_des_prenoms - 1]).as_str()) {
        fin_des_prenoms -= 1;
    }
    let prenoms = &mots[..fin_des_prenoms];
    let particules = &mots[fin_des_prenoms..mots.len() - 1];
    if prenoms.is_empty() || !prenoms.iter().all(|p| en_nom_propre(p) && est_prenom(p)) {
        return None;
    }
    let mut tri = format!("{nom_de_famille}, {}", prenoms.join(" "));
    for p in particules {
        tri.push(' ');
        tri.push_str(p);
    }
    Some(tri)
}

/// Pose un nom de tri DÉDUIT sur chaque artiste sans MBID dont le nom de tri
/// est vide. Rend le nombre de fiches écrites. Idempotente : une fiche déjà
/// pourvue (balise, saisie, MusicBrainz ou déduction) n'est plus candidate.
pub fn deduire_les_noms_de_tri(backend: &Arc<dyn DbBackend>) -> Result<usize, String> {
    let lignes = backend.query_many(
        "SELECT id, name FROM artists \
         WHERE (musicbrainz_id IS NULL OR TRIM(musicbrainz_id) = '') \
           AND (sort_name IS NULL OR TRIM(sort_name) = '') \
         ORDER BY id",
        &[],
    )?;
    let mut poses = 0;
    for l in lignes {
        let (Some(id), Some(nom)) = (
            l.first().and_then(|v| v.as_i64()),
            l.get(1).and_then(|v| v.as_string()),
        ) else {
            continue;
        };
        let Some(tri) = nom_de_tri_deduit(&nom) else {
            continue;
        };
        // La condition est REDITE dans l'écriture : une balise ou une saisie
        // posée entre la lecture et l'écriture n'est pas écrasée.
        let ecrites = backend.execute(
            &format!(
                "UPDATE artists SET sort_name = ?, sort_name_source = '{SOURCE_TRI_DEDUIT}' \
                 WHERE id = ? AND (sort_name IS NULL OR TRIM(sort_name) = '') \
                   AND (musicbrainz_id IS NULL OR TRIM(musicbrainz_id) = '')"
            ),
            &[&tri as &dyn ToSqlValue, &id as &dyn ToSqlValue],
        )?;
        poses += ecrites as usize;
    }
    if poses > 0 {
        info!(poses, "noms_de_tri_deduits");
    }
    Ok(poses)
}

#[cfg(test)]
#[path = "noms_de_tri_tests_6773.rs"]
mod tests;
