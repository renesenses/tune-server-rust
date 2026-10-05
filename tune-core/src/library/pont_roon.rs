//! Le pont Roon, côté IMPORT — phase 2 du chantier (#3914).
//!
//! Le moissonneur (`tune-moissonneur-roon`, hors du binaire) a parcouru le
//! Core de Fabien le 16/09/2026 : 597 artistes, 1 266 albums, 15 512 pistes
//! avec leurs crédits, 589 images d'artistes. Ce module lit CE format — le
//! nôtre, écrit par le moissonneur — et décide à quoi chaque élément
//! correspond dans la bibliothèque. Il ne rend rien, n'écrit rien : la route
//! (`routes/system/import.rs`) applique.
//!
//! ## Ce que Roon apporte, et ce qu'il n'apporte pas
//!
//! La sonde brute du 16/09 a tranché : l'API Browse de Roon n'expose ni
//! biographie ni identifiant (MBID, UPC, ISRC) — les réponses brutes portent
//! exactement les cinq clés que la caisse rend. Ce qui reste, et qui est
//! réel : les **crédits par piste** (`subtitle` d'une piste dans l'album :
//! « 16 Horsepower, Hank Williams ») et les **images** — que l'export ne
//! porte encore que par leur clé (`image_key`), pas leurs octets.
//!
//! ## La règle des crédits
//!
//! Roon ne dit pas le RÔLE : la ligne mêle interprète et auteurs. On retire
//! les noms qui sont l'artiste de l'album ou de la piste — ceux-là sont des
//! interprètes, et Tune les connaît déjà — et le reste entre comme
//! `composer`, le rôle le plus proche de « a écrit ce morceau ». C'est une
//! approximation, et elle est DITE : la provenance est marquée sur la piste
//! (`track_metadata.credits_source = roon`), et rien n'est écrit sur une
//! piste qui a déjà des crédits — le vide seulement, comme partout.
//!
//! ## L'appariement, en deux niveaux (#5749)
//!
//! Le niveau 1 est l'égalité stricte du titre replié ([`plier`]), sous les
//! fiches de l'artiste. Il garde la priorité. Le niveau 2 ne voit que ce qu'il
//! a laissé : ses candidats partagent la clé de [`cle_d_album`] (ponctuation
//! et suffixe de disque ignorés), et seules les PISTES décident
//! ([`decider`]). Le fil 2140 l'a montré : « Dresden (Live-2007) » contre
//! « … (CD 1/2) », mêmes sept pistes, et deux « FACTORY Communications » de
//! titres voisins mais de pistes autres.
//!
//! ## La garde, inchangée
//!
//! Ce qui vient de Roon reste LOCAL. `cloud::library_sync` ne pousse ni les
//! crédits ni les images — un témoin le tient (voir `routes/system/import.rs`).
use serde::Deserialize;

/// L'export du moissonneur, tel qu'il l'écrit.
#[derive(Debug, Clone, Deserialize)]
pub struct ExportRoon {
    pub source: String,
    #[serde(default)]
    pub releve: String,
    #[serde(default)]
    pub core: String,
    #[serde(default)]
    pub artistes: Vec<ArtisteRoon>,
    #[serde(default)]
    pub absent_de_l_api: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ArtisteRoon {
    pub nom: String,
    #[serde(default)]
    pub image: Option<String>,
    #[serde(default)]
    pub albums: Vec<AlbumRoon>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AlbumRoon {
    pub titre: String,
    #[serde(default)]
    pub sous_titre: Option<String>,
    #[serde(default)]
    pub image: Option<String>,
    #[serde(default)]
    pub pistes: Vec<PisteRoon>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PisteRoon {
    /// `"1. Hutterite Mile"` — le numéro est DANS le titre, c'est Roon.
    pub titre: String,
    /// `"16 Horsepower, David Eugene Edwards"` — à virgules, sans rôle.
    #[serde(default)]
    pub credits: Option<String>,
}

impl ExportRoon {
    /// Lit un export et refuse ce qui n'en est pas un : un autre JSON, ou un
    /// export d'une autre source, ne doit pas entrer par cette porte.
    pub fn lire(texte: &str) -> Result<Self, String> {
        let e: ExportRoon = serde_json::from_str(texte).map_err(|e| format!("json: {e}"))?;
        if e.source != "roon" {
            return Err(format!(
                "source « {} » : ce n'est pas un export du pont Roon",
                e.source
            ));
        }
        Ok(e)
    }
}

/// Replie un nom pour l'appariement : accents, casse, espaces, article.
/// « The Wailers » et « Wailers, The » et « the wailers » sont le même.
pub fn plier(nom: &str) -> String {
    use unicode_normalization::UnicodeNormalization;
    let base: String = nom
        .nfd()
        .filter(|c| !unicode_normalization::char::is_combining_mark(*c))
        .flat_map(char::to_lowercase)
        .collect();
    let base = base.split_whitespace().collect::<Vec<_>>().join(" ");
    let base = base.trim_end_matches(", the").to_string();
    base.strip_prefix("the ")
        .map(str::to_string)
        .unwrap_or(base)
}

/// Le numéro qu'un titre Roon porte en tête, quand il en porte un.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Numero {
    /// `1-7 Titre` : le disque, sur un album à plusieurs disques.
    pub disque: Option<u32>,
    pub piste: u32,
}

/// `"12. Titre"` → piste 12 ; `"1-7 Titre"` → disque 1, piste 7 (la forme des
/// albums à plusieurs disques, mesurée sur 3 208 des 15 512 pistes de
/// l'export de Fabien) ; `"Titre"` → aucun numéro.
pub fn numero_et_titre(titre: &str) -> (Option<Numero>, String) {
    let t = titre.trim();
    let chiffres = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit());
    if let Some((num, reste)) = t.split_once(". ")
        && chiffres(num)
        && let Ok(n) = num.parse::<u32>()
    {
        return (
            Some(Numero {
                disque: None,
                piste: n,
            }),
            reste.trim().to_string(),
        );
    }
    if let Some((tete, reste)) = t.split_once(' ')
        && let Some((d, n)) = tete.split_once('-')
        && chiffres(d)
        && chiffres(n)
        && let (Ok(d), Ok(n)) = (d.parse::<u32>(), n.parse::<u32>())
    {
        return (
            Some(Numero {
                disque: Some(d),
                piste: n,
            }),
            reste.trim().to_string(),
        );
    }
    (None, t.to_string())
}

/// Suffixes qu'une virgule ne sépare pas : « Grover Washington, Jr. » est un
/// seul nom — le cas réel du corpus de Fabien (#4015).
const SUFFIXES: &[&str] = &["jr", "jr.", "sr", "sr.", "ii", "iii", "iv"];

/// Les noms d'une ligne de crédits Roon, dans l'ordre, dédoublonnés.
pub fn noms_des_credits(credits: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for morceau in credits.split(',') {
        let m = morceau.trim();
        if m.is_empty() {
            continue;
        }
        if SUFFIXES.contains(&m.to_lowercase().as_str())
            && let Some(dernier) = out.last_mut()
        {
            *dernier = format!("{dernier}, {m}");
            continue;
        }
        if !out.iter().any(|x| plier(x) == plier(m)) {
            out.push(m.to_string());
        }
    }
    out
}

/// Les crédits à écrire pour une piste : la ligne Roon MOINS les interprètes
/// que Tune connaît déjà (artiste de l'album, artiste de la piste). Vide quand
/// la ligne ne dit rien de plus que ce qu'on sait.
pub fn credits_a_ecrire(credits: &str, interpretes: &[&str]) -> Vec<String> {
    let connus: Vec<String> = interpretes.iter().map(|i| plier(i)).collect();
    noms_des_credits(credits)
        .into_iter()
        .filter(|n| !connus.contains(&plier(n)))
        .collect()
}

/// Une ligne de [`Rapport::classement`].
#[derive(Debug, Default, Clone, serde::Serialize, PartialEq, Eq)]
pub struct Classement {
    /// Indice de l'artiste dans `artistes`, puis de l'album dans ses `albums`.
    pub i: usize,
    pub j: usize,
    /// `strict`, `contenu`, `ambigu` ou `inconnu`.
    pub classe: &'static str,
    pub ids_tune: Vec<i64>,
}

/// Ce que l'appariement d'un export contre une bibliothèque a trouvé — les
/// comptes que l'aperçu et le rapport affichent.
#[derive(Debug, Default, Clone, serde::Serialize, PartialEq, Eq)]
pub struct Rapport {
    pub artistes_total: usize,
    pub artistes_apparies: usize,
    pub artistes_inconnus: Vec<String>,
    pub albums_total: usize,
    /// Albums Roon appariés, aux deux niveaux : `strict + contenu`.
    pub albums_apparies: usize,
    /// … par l'égalité stricte du titre replié, sous l'artiste (niveau 1).
    pub albums_apparies_strict: usize,
    /// Le détail du niveau 1 : « Artiste — Titre Roon → [id] Titre Tune ».
    pub albums_par_strict: Vec<String>,
    /// … par leurs pistes (niveau 2, voir [`decider`]).
    pub albums_apparies_contenu: usize,
    /// Albums de TUNE enrichis : un coffret Roon en compte un par disque.
    pub albums_tune_apparies: usize,
    /// Le détail du niveau 2 : « Artiste — Titre Roon → [id] Titre Tune
    /// [+ [id] …] ».
    pub albums_par_contenu: Vec<String>,
    /// Plusieurs candidats tiennent : rien n'est écrit. Chaque candidat porte
    /// son id Tune ; des copies aux pistes identiques sont dites
    /// (« N exemplaires identiques »).
    pub albums_ambigus: Vec<String>,
    /// Introuvables, artistes inconnus compris. Avec les trois autres
    /// catégories : `albums_total = strict + contenu + ambigus + inconnus`.
    pub albums_inconnus: Vec<String>,
    /// Deux fiches Tune pour un même nom replié (artiste) ou un même titre
    /// replié (album) : signalées, jamais tranchées par l'ordre des lignes.
    pub doublons: Vec<String>,
    /// Chaque album Roon À SA PLACE dans l'export (`artistes[i].albums[j]`),
    /// avec sa catégorie et les id Tune retenus ou en cause : la clé de
    /// jointure exacte pour comparer deux appariements du même export, même
    /// quand Roon porte quatre fois le même titre.
    pub classement: Vec<Classement>,
    pub pistes_total: usize,
    pub pistes_appariees: usize,
    /// Pistes dont la ligne Roon apporte au moins un nom de plus.
    pub credits_a_ecrire: usize,
    /// … dont Tune a déjà des crédits : on n'y touche pas.
    pub credits_deja_presents: usize,
    pub credits_ecrits: usize,
    /// Images : combien l'export en NOMME (clés), et combien il en PORTE.
    pub images_nommees: usize,
    pub images_portees: usize,
    /// Artistes appariés, sans image chez Tune, dont l'archive porte l'image.
    pub images_artistes_a_poser: usize,
    pub images_artistes_posees: usize,
    /// Albums appariés, sans pochette chez Tune, dont l'archive porte l'image.
    pub images_albums_a_poser: usize,
    pub images_albums_posees: usize,
}

/// Une piste de Tune, réduite à ce que l'appariement lit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PisteLocale {
    pub id: i64,
    pub titre: String,
    pub numero: Option<i32>,
    pub disque: Option<i32>,
    pub artiste: Option<String>,
    pub a_des_credits: bool,
}

/// Retrouve la piste locale d'une piste Roon : par NUMÉRO quand Roon en donne
/// un et qu'une seule piste le porte, sinon par titre replié.
pub fn apparier_piste<'a>(roon: &PisteRoon, locales: &'a [PisteLocale]) -> Option<&'a PisteLocale> {
    let (num, titre) = numero_et_titre(&roon.titre);
    if let Some(n) = num {
        // Le disque compte quand Roon le donne : la piste 1 du disque 2 n'est
        // pas la piste 1 du disque 1.
        let memes: Vec<&PisteLocale> = locales
            .iter()
            .filter(|p| p.numero == Some(n.piste as i32))
            .filter(|p| {
                n.disque
                    .is_none_or(|d| p.disque.is_none_or(|pd| pd == d as i32))
            })
            .collect();
        if let [seule] = memes.as_slice() {
            return Some(seule);
        }
    }
    // Un titre générique (« Track 01 ») ne désigne rien : sans numéro, il
    // n'apparie pas.
    if titre_generique_numerote(&titre, num.map(|n| n.piste as i32)) {
        return None;
    }
    let voulu = plier(&titre);
    locales.iter().find(|p| plier(&p.titre) == voulu)
}

/// Niveau 2 — part MINIMALE des pistes qui doivent se retrouver, en
/// pourcentage, des deux côtés : celles de l'album Roon ET celles de l'album
/// Tune (pour un coffret, celles de CHAQUE disque Tune). 80 % tolère une piste
/// cachée ou un bonus sur dix, pas un autre album : les deux « FACTORY
/// Communications » du fil 2140 ont des titres voisins et des pistes autres.
pub const SEUIL_CONTENU_PCT: usize = 80;
/// Niveau 2 — en dessous de deux pistes retrouvées, le contenu ne prouve rien
/// (un single, une « Intro ») : l'album reste introuvable.
pub const PISTES_MIN_CONTENU: usize = 2;
/// Niveau 2 — écart MINIMAL, en points, entre la couverture du candidat retenu
/// et celle du suivant. La couverture d'un candidat est la plus petite de ses
/// deux parts, `min(m / pistes Roon, m / pistes Tune)` : 85 % contre 78 % est
/// trop serré (ambigu), l'édition standard contre la deluxe (100 % contre
/// 67 %) reste tranchée.
pub const MARGE_CONTENU_PCT: usize = 10;

/// Les titres de piste génériques, une fois repliés par [`plier_large`] : ils
/// ne désignent aucun morceau, et deux albums mal étiquetés « Track 01…
/// Track 12 » se ressembleraient parfaitement. Exclus du compte des pistes
/// communes, à tous les niveaux, et jamais appariés par leur seul titre.
///
/// - vide ;
/// - des chiffres seuls ÉGAUX au numéro de la piste (« 01 » ou « 1 » pour la
///   piste 1) : voir [`titre_generique_numerote`]. « 1999 » ou « 22 » sur une
///   autre piste sont de vrais titres ;
/// - un mot de piste suivi ou non d'un numéro : `track`, `piste`, `titre`,
///   `pista`, `traccia`, `titel` (« Track 01 »,
///   « Track01 », « Piste 1 ») ;
/// - `unknown`, `untitled`, `sans titre`, `no title`, `inconnu`, et tout titre
///   qui contient `unknown title` ou `unknown track`.
pub fn titre_generique(titre: &str) -> bool {
    static GENERIQUE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(
            r"^(?:|(?:track|piste|titre|pista|traccia|titel) ?\d*|unknown|untitled|sans titre|no title|inconnu|inconnue)$",
        )
        .expect("regex des titres génériques")
    });
    let t = plier_large(titre);
    GENERIQUE.is_match(&t) || t.contains("unknown title") || t.contains("unknown track")
}

/// [`titre_generique`], plus le titre fait de chiffres seuls qui ne fait que
/// répéter le numéro de la piste (« 03 » en piste 3).
pub fn titre_generique_numerote(titre: &str, numero: Option<i32>) -> bool {
    if titre_generique(titre) {
        return true;
    }
    let t = plier_large(titre);
    numero.is_some_and(|n| {
        !t.is_empty()
            && t.chars().all(|c| c.is_ascii_digit())
            && t.parse::<i64>().ok() == Some(i64::from(n))
    })
}

/// Les noms d'artiste ou d'album génériques, une fois repliés par
/// [`plier_large`] : ils ne désignent personne ni aucun disque, et deux
/// « Unknown Artist — Unknown Album » ne sont pas le même album. Ils ne
/// s'apparient JAMAIS au niveau strict : seules les pistes peuvent les relier.
///
/// `unknown`, `unknown artist`, `unknown album`, `inconnu`, `inconnue`,
/// `artiste inconnu`, `album inconnu`, `various`, `various artists`, `va`,
/// `divers`, `artistes divers`, `artistes varies`, `compilation`, `untitled`,
/// `sans titre`, `no title`, et le nom vide.
pub fn nom_generique(nom: &str) -> bool {
    const NOMS: &[&str] = &[
        "",
        "unknown",
        "unknown artist",
        "unknown album",
        "inconnu",
        "inconnue",
        "artiste inconnu",
        "album inconnu",
        "various",
        "various artists",
        "va",
        "divers",
        "artistes divers",
        "artistes varies",
        "compilation",
        "untitled",
        "sans titre",
        "no title",
    ];
    NOMS.contains(&plier_large(nom).as_str())
}

static SUFFIXE_DISQUE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
    regex::Regex::new(
        r"[\s\-–:,]*[\(\[\{]?\s*\b(?:cd|disc|disk|disque|disco)\s*\.?\s*(\d{1,2})(?:\s*(?:/|of|sur|de)\s*\d{1,2})?\s*[\)\]\}]?\s*$",
    )
    .expect("regex du suffixe de disque")
});

/// [`plier`], puis toute ponctuation devient espace : « Cristal Automatique
/// #1 » et « Cristal automatique 1 », « (Live-2007) » et « Live 2007 » se
/// rejoignent. Sert aux titres de piste du niveau 2 et à la clé de candidat.
pub fn plier_large(nom: &str) -> String {
    plier(nom)
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// La clé de CANDIDAT d'un titre d'album, et le disque que son suffixe
/// annonce : « Dresden (Live-2007) (CD 1/2) » → (`dresden live 2007`, 1).
///
/// Elle ne sert qu'à TROUVER des candidats, jamais à décider : deux titres de
/// même clé restent deux albums tant que leurs pistes ne concordent pas.
/// Les crochets ne sont pas retirés avec leur contenu : « [2006] » ou
/// « (Karajan 1977) » distinguent souvent deux éditions.
pub fn cle_d_album(titre: &str) -> (String, Option<u32>) {
    let mut t = plier(titre);
    let mut disque = None;
    while let Some(c) = SUFFIXE_DISQUE.captures(&t) {
        let debut = c.get(0).map_or(t.len(), |m| m.start());
        if debut == 0 {
            break; // le titre n'est QUE « CD 2 » : on le garde
        }
        if disque.is_none() {
            disque = c.get(1).and_then(|n| n.as_str().parse().ok());
        }
        t.truncate(debut);
    }
    (plier_large(&t), disque)
}

static ENTRE_CROCHETS: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
    // Un groupe fermé, ou un groupe resté ouvert en fin de titre
    // (« Britten - War Requiem [Decca Originals, »).
    regex::Regex::new(r"\([^()]*(?:\)|$)|\[[^\[\]]*(?:\]|$)|\{[^{}]*(?:\}|$)")
        .expect("regex des crochets")
});

/// La SECONDE clé de candidat : [`cle_d_album`] sans les crochets ni les
/// parenthèses, contenu compris. « Black Orpheus [Original Soundtrack] » →
/// `black orpheus`. Consultée seulement quand la clé normale ne trouve AUCUN
/// candidat : elle rapproche des éditions que seules les pistes séparent.
pub fn cle_courte_d_album(titre: &str) -> String {
    let mut t = plier(titre);
    while let Some(c) = SUFFIXE_DISQUE.find(&t) {
        if c.start() == 0 {
            break;
        }
        t.truncate(c.start());
    }
    let mut avant = String::new();
    while avant != t {
        avant = t.clone();
        t = ENTRE_CROCHETS.replace_all(&t, " ").into_owned();
    }
    let court = plier_large(&t);
    if court.is_empty() {
        cle_d_album(titre).0
    } else {
        court
    }
}

/// Un album de Tune candidat au niveau 2, avec ses pistes.
#[derive(Debug, Clone)]
pub struct Candidat {
    pub id: i64,
    pub titre: String,
    pub pistes: Vec<PisteLocale>,
}

/// Les pistes Roon retrouvées dans un album Tune, une à une : (indice Roon,
/// indice local). Les titres génériques ([`titre_generique`]) ne comptent
/// jamais. Une piste se retrouve par son titre replié large ET, quand
/// les deux côtés le donnent, par le même numéro et le même disque. Le disque
/// local est celui du SUFFIXE du titre quand il en porte un : Tune range
/// souvent « … (CD 2/2) » avec des pistes étiquetées disque 1.
pub fn pistes_communes(roon: &[PisteRoon], c: &Candidat) -> Vec<(usize, usize)> {
    let disque_album = cle_d_album(&c.titre).1;
    let titres: Vec<String> = c.pistes.iter().map(|p| plier_large(&p.titre)).collect();
    let generiques: Vec<bool> = c
        .pistes
        .iter()
        .map(|p| titre_generique_numerote(&p.titre, p.numero))
        .collect();
    let mut prises = vec![false; c.pistes.len()];
    let mut out = Vec::new();
    for (i, p) in roon.iter().enumerate() {
        let (num, titre) = numero_et_titre(&p.titre);
        if titre_generique_numerote(&titre, num.map(|n| n.piste as i32)) {
            continue;
        }
        let voulu = plier_large(&titre);
        let trouve = c.pistes.iter().enumerate().position(|(j, l)| {
            if prises[j] || titres[j] != voulu || generiques[j] {
                return false;
            }
            let Some(n) = num else { return true };
            let numero_ok = l.numero.is_none_or(|ln| ln == n.piste as i32);
            let disque_local = disque_album.map(|d| d as i32).or(l.disque);
            let disque_ok = match (n.disque, disque_local) {
                (Some(d), Some(ld)) => ld == d as i32,
                _ => true,
            };
            numero_ok && disque_ok
        });
        if let Some(j) = trouve {
            prises[j] = true;
            out.push((i, j));
        }
    }
    out
}

/// Le plus grand nombre de candidats aux listes de pistes IDENTIQUES (même
/// numéro, même titre replié large, dans l'ordre) : deux copies d'un même
/// album. Le rapport le dit, pour que la bibliothèque soit dédoublonnée.
pub fn exemplaires_identiques(candidats: &[&Candidat]) -> usize {
    let empreinte = |c: &Candidat| -> Vec<(Option<i32>, String)> {
        c.pistes
            .iter()
            .map(|p| (p.numero, plier_large(&p.titre)))
            .collect()
    };
    let empreintes: Vec<_> = candidats.iter().map(|c| empreinte(c)).collect();
    empreintes
        .iter()
        .filter(|e| !e.is_empty())
        .map(|e| empreintes.iter().filter(|f| *f == e).count())
        .max()
        .unwrap_or(0)
}

/// Ce que le niveau 2 conclut pour un album Roon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Un album Tune (indice de candidat), ou plusieurs pour un coffret rangé
    /// un album par disque, chacun avec ses paires de pistes.
    Apparie(Vec<(usize, Vec<(usize, usize)>)>),
    /// Plusieurs lectures tiennent : les candidats en cause. Rien n'est écrit.
    Ambigu(Vec<usize>),
    Introuvable,
}

/// `a` couvre-t-il au moins `b − MARGE_CONTENU_PCT` ? Les couvertures sont
/// des fractions `m / n` (n = le plus grand des deux albums), comparées en
/// entiers.
fn trop_proche((ma, na): (usize, usize), (mb, nb): (usize, usize)) -> bool {
    na > 0 && nb > 0 && ma * 100 * nb + MARGE_CONTENU_PCT * na * nb >= mb * 100 * na
}

fn atteint(trouvees: usize, total: usize) -> bool {
    total > 0 && trouvees * 100 >= SEUIL_CONTENU_PCT * total
}

/// Le niveau 2 : quel(s) candidat(s) portent les pistes de l'album Roon ?
///
/// 1. **Un seul album** : ≥ [`SEUIL_CONTENU_PCT`] des pistes Roon retrouvées
///    ET ≥ ce seuil des pistes du candidat, au moins [`PISTES_MIN_CONTENU`].
///    Deux candidats qui tiennent ainsi → [`Decision::Ambigu`] ; de même
///    quand un autre candidat, même sous le seuil, couvre à moins de
///    [`MARGE_CONTENU_PCT`] points du retenu.
/// 2. **Coffret** (aucun album seul ne tient) : plusieurs candidats dont
///    chacun est couvert au seuil, sans piste Roon commune entre eux, et qui
///    réunis couvrent le seuil des pistes Roon. Deux disques qui réclament la
///    même piste (deux copies du CD 1) → [`Decision::Ambigu`].
pub fn decider(roon: &[PisteRoon], candidats: &[Candidat]) -> Decision {
    if roon.is_empty() || candidats.is_empty() {
        return Decision::Introuvable;
    }
    let paires: Vec<Vec<(usize, usize)>> =
        candidats.iter().map(|c| pistes_communes(roon, c)).collect();
    let pleins: Vec<usize> = (0..candidats.len())
        .filter(|&k| {
            let m = paires[k].len();
            m >= PISTES_MIN_CONTENU
                && atteint(m, roon.len())
                && atteint(m, candidats[k].pistes.len())
        })
        .collect();
    // Couverture d'un candidat : m / max(pistes Roon, pistes Tune).
    let couverture = |k: usize| (paires[k].len(), roon.len().max(candidats[k].pistes.len()));
    match pleins.as_slice() {
        [k] => {
            let proches: Vec<usize> = (0..candidats.len())
                .filter(|&o| o != *k && !paires[o].is_empty())
                .filter(|&o| trop_proche(couverture(o), couverture(*k)))
                .collect();
            if !proches.is_empty() {
                let mut tous = vec![*k];
                tous.extend(proches);
                tous.sort_unstable();
                return Decision::Ambigu(tous);
            }
            return Decision::Apparie(vec![(*k, paires[*k].clone())]);
        }
        [_, _, ..] => return Decision::Ambigu(pleins),
        [] => {}
    }
    let parties: Vec<usize> = (0..candidats.len())
        .filter(|&k| !paires[k].is_empty() && atteint(paires[k].len(), candidats[k].pistes.len()))
        .collect();
    if parties.len() < 2 {
        return Decision::Introuvable;
    }
    let mut vues = std::collections::HashSet::new();
    for &k in &parties {
        for &(i, _) in &paires[k] {
            if !vues.insert(i) {
                return Decision::Ambigu(parties);
            }
        }
    }
    if vues.len() >= PISTES_MIN_CONTENU && atteint(vues.len(), roon.len()) {
        Decision::Apparie(
            parties
                .into_iter()
                .map(|k| (k, paires[k].clone()))
                .collect(),
        )
    } else {
        Decision::Introuvable
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn le_format_du_moissonneur_se_lit_et_les_autres_sont_refuses() {
        let e = ExportRoon::lire(r#"{"source":"roon","artistes":[{"nom":"16 Horsepower","image":"ce1d","albums":[{"titre":"Folklore","pistes":[{"titre":"1. Hutterite Mile","credits":"16 Horsepower, David Eugene Edwards"}]}]}],"absent_de_l_api":["biographies"]}"#).unwrap();
        assert_eq!(e.artistes.len(), 1);
        assert_eq!(e.artistes[0].albums[0].pistes[0].titre, "1. Hutterite Mile");
        assert!(ExportRoon::lire(r#"{"source":"plex","artistes":[]}"#).is_err());
        assert!(ExportRoon::lire("Title,Artist\nA,B").is_err());
    }

    #[test]
    fn le_numero_sort_du_titre() {
        let n = |piste| {
            Some(Numero {
                disque: None,
                piste,
            })
        };
        assert_eq!(
            numero_et_titre("1. Hutterite Mile"),
            (n(1), "Hutterite Mile".into())
        );
        assert_eq!(
            numero_et_titre("12. Mr. Blue Sky"),
            (n(12), "Mr. Blue Sky".into())
        );
        assert_eq!(
            numero_et_titre("Mr. Blue Sky"),
            (None, "Mr. Blue Sky".into())
        );
        assert_eq!(
            numero_et_titre("2001. A Space Odyssey"),
            (n(2001), "A Space Odyssey".into())
        );
        // Plusieurs disques : `1-7 Titre`, sans point — la forme réelle.
        let dn = |disque, piste| {
            Some(Numero {
                disque: Some(disque),
                piste,
            })
        };
        assert_eq!(
            numero_et_titre("1-7 Hutterite Mile (2002, \" Folklore \" )"),
            (dn(1, 7), "Hutterite Mile (2002, \" Folklore \" )".into())
        );
        assert_eq!(
            numero_et_titre("2-10 Low Estate"),
            (dn(2, 10), "Low Estate".into())
        );
        assert_eq!(numero_et_titre("1-A Titre"), (None, "1-A Titre".into()));
    }

    #[test]
    fn les_credits_se_decoupent_sans_couper_un_jr() {
        assert_eq!(
            noms_des_credits("16 Horsepower, Hank Williams"),
            vec!["16 Horsepower", "Hank Williams"]
        );
        assert_eq!(
            noms_des_credits("Grover Washington, Jr., Bill Withers"),
            vec!["Grover Washington, Jr.", "Bill Withers"]
        );
        assert_eq!(noms_des_credits("A, a, A "), vec!["A"]);
        assert_eq!(noms_des_credits(""), Vec::<String>::new());
    }

    #[test]
    fn les_interpretes_connus_sont_retires_et_le_reste_est_a_ecrire() {
        assert_eq!(
            credits_a_ecrire("16 Horsepower, Hank Williams", &["16 Horsepower"]),
            vec!["Hank Williams"]
        );
        assert_eq!(
            credits_a_ecrire("The Wailers, Bob Marley", &["Wailers, The", "bob marley"]),
            Vec::<String>::new()
        );
        assert_eq!(
            credits_a_ecrire("Édith Piaf", &["edith piaf"]),
            Vec::<String>::new()
        );
    }

    #[test]
    fn une_piste_s_apparie_par_numero_puis_par_titre() {
        let l = |id, titre: &str, numero, disque| PisteLocale {
            id,
            titre: titre.into(),
            numero: Some(numero),
            disque: Some(disque),
            artiste: None,
            a_des_credits: false,
        };
        let locales = vec![
            l(10, "Hutterite Mile", 1, 1),
            l(11, "Outlaw Song", 2, 1),
            l(12, "Outlaw Song (live)", 2, 1),
            l(20, "Black Soul Choir", 1, 2),
        ];
        let r = |t: &str| PisteRoon {
            titre: t.into(),
            credits: None,
        };
        // Deux pistes 1 (disque 1 et disque 2) : sans disque, le titre
        // tranche ; avec le disque, le numéro suffit.
        assert_eq!(
            apparier_piste(&r("1. Hutterite Mile"), &locales).map(|p| p.id),
            Some(10)
        );
        assert_eq!(
            apparier_piste(&r("2-1 Black Soul Choir"), &locales).map(|p| p.id),
            Some(20)
        );
        assert_eq!(
            apparier_piste(&r("1-1 Hutterite Mile"), &locales).map(|p| p.id),
            Some(10)
        );
        // Deux pistes portent le 2 : le numéro ne tranche pas, le titre oui.
        assert_eq!(
            apparier_piste(&r("2. Outlaw Song"), &locales).map(|p| p.id),
            Some(11)
        );
        assert_eq!(
            apparier_piste(&r("Outlaw Song"), &locales).map(|p| p.id),
            Some(11)
        );
        assert_eq!(apparier_piste(&r("9. Inconnue"), &locales), None);
    }

    #[test]
    fn la_cle_d_album_ignore_le_suffixe_de_disque_et_la_ponctuation() {
        let k = |t: &str| cle_d_album(t);
        // Le cas phare du fil 2140.
        assert_eq!(
            k("Dresden (Live-2007) (CD 1/2)"),
            ("dresden live 2007".into(), Some(1))
        );
        assert_eq!(k("Dresden (Live-2007)"), ("dresden live 2007".into(), None));
        assert_eq!(k("Sun Bear [Disc 2]").1, Some(2));
        assert_eq!(k("Sun Bear - CD2").1, Some(2));
        assert_eq!(k("Sun Bear (Disque 3 sur 4)"), ("sun bear".into(), Some(3)));
        assert_eq!(k("Sun Bear, disc 1 of 2"), ("sun bear".into(), Some(1)));
        assert_eq!(k("Cristal Automatique #1"), k("Cristal automatique 1"));
        // Ni un mot qui finit par « cd », ni « disco », ni un titre QUI EST
        // le suffixe ; les crochets restent, contenu compris.
        assert_eq!(k("Abcd 2"), ("abcd 2".into(), None));
        assert_eq!(k("Disco 2000").1, None);
        assert_eq!(k("CD 2"), ("cd 2".into(), None));
        assert_eq!(
            k("Les Tortures Volontaires [2006]").0,
            "les tortures volontaires 2006"
        );
        // Deux albums différents restent deux clés.
        assert_ne!(k("Communications 1978-81").0, k("Communications 1983-87").0);
    }

    fn roon(titres: &[&str]) -> Vec<PisteRoon> {
        titres
            .iter()
            .map(|t| PisteRoon {
                titre: (*t).into(),
                credits: None,
            })
            .collect()
    }

    fn candidat(id: i64, titre: &str, pistes: &[(i32, i32, &str)]) -> Candidat {
        Candidat {
            id,
            titre: titre.into(),
            pistes: pistes
                .iter()
                .enumerate()
                .map(|(i, (d, n, t))| PisteLocale {
                    id: id * 100 + i as i64,
                    titre: (*t).into(),
                    numero: Some(*n),
                    disque: (*d > 0).then_some(*d),
                    artiste: None,
                    a_des_credits: false,
                })
                .collect(),
        }
    }

    #[test]
    fn le_contenu_tranche_entre_deux_titres_voisins() {
        // FACTORY Communications : même artiste, titres proches, pistes
        // autres. Seul celui qui porte les pistes est retenu.
        let r = roon(&["1. Atmosphere", "2. Transmission", "3. Shack Up"]);
        let a = candidat(
            1,
            "Communications 1978-81",
            &[
                (1, 1, "Atmosphere"),
                (1, 2, "Transmission"),
                (1, 3, "Shack Up"),
            ],
        );
        let b = candidat(
            2,
            "Communications 1983-87",
            &[(1, 1, "Blue Monday"), (1, 2, "Fac 51"), (1, 3, "Shack Up")],
        );
        assert_eq!(
            decider(&r, &[b.clone(), a]),
            Decision::Apparie(vec![(1, vec![(0, 0), (1, 1), (2, 2)])])
        );
        // Le même numéro ne suffit pas : le titre doit suivre, et un titre
        // au mauvais numéro ne compte pas.
        assert_eq!(decider(&r, &[b]), Decision::Introuvable);
        let melange = candidat(
            3,
            "x",
            &[
                (1, 2, "Atmosphere"),
                (1, 1, "Transmission"),
                (1, 3, "Shack Up"),
            ],
        );
        assert_eq!(decider(&r, &[melange]), Decision::Introuvable);
        // Sous le seuil côté Tune : l'album local a deux fois plus de pistes.
        let gros = candidat(
            4,
            "x",
            &[
                (1, 1, "Atmosphere"),
                (1, 2, "Transmission"),
                (1, 3, "Shack Up"),
                (1, 4, "A"),
                (1, 5, "B"),
                (1, 6, "C"),
            ],
        );
        assert_eq!(decider(&r, &[gros]), Decision::Introuvable);
        // Une piste ne prouve rien.
        assert_eq!(
            decider(
                &roon(&["1. Intro"]),
                &[candidat(5, "x", &[(1, 1, "Intro")])]
            ),
            Decision::Introuvable
        );
    }

    #[test]
    fn deux_candidats_de_meme_contenu_sont_ambigus() {
        let r = roon(&["1. Mystic Rumba", "2. Lily Dale"]);
        let p = [(0, 1, "Mystic Rumba"), (0, 2, "Lily Dale")];
        assert_eq!(
            decider(
                &r,
                &[
                    candidat(1, "Mystic Rumba", &p),
                    candidat(2, "Mystic Rumba!", &p)
                ]
            ),
            Decision::Ambigu(vec![0, 1])
        );
    }

    #[test]
    fn un_coffret_s_apparie_disque_par_disque() {
        let r = roon(&[
            "1-1 Kyoto Part 1",
            "1-2 Kyoto Part 2",
            "2-1 Osaka Part 1",
            "2-2 Osaka Part 2",
        ]);
        // Le CD 2 de Tune étiquette ses pistes disque 1 : le suffixe fait foi.
        let cd1 = candidat(
            1,
            "Sun Bear Concerts (CD 1/2)",
            &[(1, 1, "Kyoto Part 1"), (1, 2, "Kyoto Part 2")],
        );
        let cd2 = candidat(
            2,
            "Sun Bear Concerts (CD 2/2)",
            &[(1, 1, "Osaka Part 1"), (1, 2, "Osaka Part 2")],
        );
        assert_eq!(
            decider(&r, &[cd1.clone(), cd2.clone()]),
            Decision::Apparie(vec![(0, vec![(0, 0), (1, 1)]), (1, vec![(2, 0), (3, 1)])])
        );
        // Un seul disque chez Tune : la moitié des pistes Roon, sous le seuil.
        assert_eq!(
            decider(&r, std::slice::from_ref(&cd1)),
            Decision::Introuvable
        );
        // Deux copies du CD 1 réclament les mêmes pistes : ambigu.
        let copie = candidat(
            3,
            "Sun Bear Concerts (CD 1)",
            &[(1, 1, "Kyoto Part 1"), (1, 2, "Kyoto Part 2")],
        );
        assert_eq!(
            decider(&r, &[cd1, cd2, copie]),
            Decision::Ambigu(vec![0, 1, 2])
        );
    }

    #[test]
    fn les_titres_generiques_sont_reconnus() {
        for t in [
            "Track 01",
            "Track01",
            "TRACK 1",
            "track",
            "Piste 1",
            "Piste 12",
            "Titre 3",
            "Unknown Title",
            "Unknown Title 7",
            "unknown",
            "Untitled",
            "Sans titre",
            "",
            " - ",
        ] {
            assert!(titre_generique(t), "« {t} » est générique");
        }
        for t in [
            "Paper Nut",
            "Track of the Cat",
            "Untitled Love",
            "Pistes noires",
        ] {
            assert!(!titre_generique(t), "« {t} » n'est pas générique");
        }
        // Des chiffres seuls : génériques seulement s'ils répètent le numéro.
        assert!(titre_generique_numerote("01", Some(1)));
        assert!(titre_generique_numerote("3", Some(3)));
        assert!(!titre_generique_numerote("1999", Some(5)));
        assert!(!titre_generique_numerote("22", Some(3)));
        assert!(!titre_generique_numerote("01", None));
        let r = roon(&["1. 01", "5. 1999", "3. 22"]);
        let c = candidat(1, "x", &[(1, 1, "01"), (1, 5, "1999"), (1, 3, "22")]);
        assert_eq!(pistes_communes(&r, &c), vec![(1, 1), (2, 2)]);
        // Les noms génériques d'artiste et d'album.
        for n in [
            "Unknown Artist",
            "unknown album",
            "<Unknown>",
            "Various Artists",
            "Artiste inconnu",
            "Divers",
        ] {
            assert!(nom_generique(n), "« {n} » est générique");
        }
        for n in ["Unknown Pleasures", "Divers & Mixtes", "Jan Garbarek Group"] {
            assert!(!nom_generique(n), "« {n} » n'est pas générique");
        }
        // Jamais apparié par son seul titre, ni compté au niveau 2.
        let r = roon(&["1. Track 01", "2. Track 02", "Track 03"]);
        let c = candidat(
            1,
            "Unknown Album",
            &[(1, 1, "Track 01"), (1, 2, "Track 02"), (1, 3, "Track 03")],
        );
        assert!(pistes_communes(&r, &c).is_empty());
        assert_eq!(decider(&r, std::slice::from_ref(&c)), Decision::Introuvable);
        assert_eq!(apparier_piste(&r[2], &c.pistes), None);
        // Le numéro, lui, reste une preuve au niveau 1.
        assert_eq!(apparier_piste(&r[0], &c.pistes).map(|p| p.id), Some(100));
    }

    #[test]
    fn la_seconde_cle_retire_crochets_et_parentheses() {
        assert_eq!(
            cle_courte_d_album("Black Orpheus [Original Soundtrack]"),
            "black orpheus"
        );
        assert_eq!(
            cle_courte_d_album("Britten - War Requiem [Decca Originals,"),
            "britten war requiem"
        );
        assert_eq!(
            cle_courte_d_album("Dresden (Live-2007) (CD 1/2)"),
            "dresden"
        );
        assert_eq!(
            cle_courte_d_album("Olympia 1985 (Live à l'Olympia / 1985)"),
            "olympia 1985"
        );
        // Un titre tout entre crochets garde sa clé normale.
        assert_eq!(cle_courte_d_album("[Untitled]"), "untitled");
    }

    /// Un album Roon de `n` pistes T1…Tn, et un candidat qui en porte `m`
    /// (T1…Tm) plus `extra` pistes à lui.
    fn serie(id: i64, m: usize, extra: usize) -> Candidat {
        let titres: Vec<String> = (1..=m)
            .map(|i| format!("T{i}"))
            .chain((1..=extra).map(|i| format!("Bonus {i}")))
            .collect();
        let pistes: Vec<(i32, i32, &str)> = titres
            .iter()
            .enumerate()
            .map(|(i, t)| (1, i as i32 + 1, t.as_str()))
            .collect();
        candidat(id, "x", &pistes)
    }

    fn roon_serie(n: usize) -> Vec<PisteRoon> {
        let t: Vec<String> = (1..=n).map(|i| format!("{i}. T{i}")).collect();
        roon(&t.iter().map(String::as_str).collect::<Vec<_>>())
    }

    #[test]
    fn la_marge_rend_ambigu_un_second_trop_proche() {
        let r = roon_serie(20);
        // 85 % contre 75 % : dix points, pas plus — ambigu.
        assert_eq!(
            decider(&r, &[serie(1, 17, 0), serie(2, 15, 0)]),
            Decision::Ambigu(vec![0, 1])
        );
        // 85 % contre 70 % : la marge est tenue.
        assert!(matches!(
            decider(&r, &[serie(1, 17, 0), serie(2, 14, 0)]),
            Decision::Apparie(v) if v.len() == 1 && v[0].0 == 0
        ));
    }

    #[test]
    fn l_edition_standard_reste_tranchee_face_a_la_deluxe() {
        // Roon a l'édition standard (10 pistes) ; Tune a la standard ET la
        // deluxe (les 10 + 5 bonus) : 100 % contre 67 %, la standard gagne.
        let r = roon_serie(10);
        assert!(matches!(
            decider(&r, &[serie(1, 10, 5), serie(2, 10, 0)]),
            Decision::Apparie(v) if v.len() == 1 && v[0].0 == 1
        ));
    }

    #[test]
    fn les_exemplaires_identiques_se_comptent() {
        let a = serie(1, 3, 0);
        let b = serie(2, 3, 0);
        let c = serie(3, 3, 1);
        assert_eq!(exemplaires_identiques(&[&a, &b, &c]), 2);
        assert_eq!(exemplaires_identiques(&[&a, &c]), 1);
    }

    /// L'export RÉEL de Fabien (16/09/2026), quand on l'a sous la main :
    /// `TUNE_EXPORT_ROON=/chemin/export.json cargo test … -- --ignored`.
    /// Ce qu'il doit donner : 597 artistes, 1 266 albums, 15 512 pistes.
    #[test]
    #[ignore]
    fn l_export_reel_de_fabien_se_lit() {
        let chemin = std::env::var("TUNE_EXPORT_ROON").expect("TUNE_EXPORT_ROON");
        let texte = std::fs::read_to_string(chemin).unwrap();
        let e = ExportRoon::lire(&texte).unwrap();
        let albums: usize = e.artistes.iter().map(|a| a.albums.len()).sum();
        let pistes: usize = e
            .artistes
            .iter()
            .flat_map(|a| &a.albums)
            .map(|a| a.pistes.len())
            .sum();
        let numerotees = e
            .artistes
            .iter()
            .flat_map(|a| &a.albums)
            .flat_map(|a| &a.pistes)
            .filter(|p| numero_et_titre(&p.titre).0.is_some())
            .count();
        eprintln!(
            "artistes={} albums={albums} pistes={pistes} numerotees={numerotees}",
            e.artistes.len()
        );
        assert_eq!(e.artistes.len(), 597);
        assert_eq!(albums, 1266);
        assert_eq!(pistes, 15512);
        assert!(
            numerotees * 100 / pistes >= 95,
            "{numerotees}/{pistes} pistes numérotées"
        );
    }
}
