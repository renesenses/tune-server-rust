//! Regrouper les exemplaires d'un même ENREGISTREMENT entre sources, et
//! choisir celui qu'on joue (#2264).
//!
//! # Ce que ce module décide, et rien d'autre
//!
//! Il reçoit des exemplaires déjà trouvés — la piste de départ, ses autres
//! versions locales, ce que les services ont rendu — et répond à deux
//! questions :
//!
//! 1. lesquels sont le MÊME enregistrement ([`grouper`]) ;
//! 2. dans un groupe, lequel jouer par défaut ([`choisir`]).
//!
//! Il ne cherche rien, ne lit pas la base et n'écrit rien. La recherche des
//! candidats reste celle de `routes/versions.rs` ; ce module est pur pour que
//! chacune de ses règles se prouve sans service ni base.
//!
//! # L'identité, dans l'ordre (arbitrage du 01/09/2026, go du 07/10/2026)
//!
//! | rang | lien | quand |
//! |---|---|---|
//! | 1 | [`Lien::Isrc`] | les deux ISRC sont connus et égaux une fois normalisés |
//! | 2 | [`Lien::MbidEnregistrement`] | les deux MBID d'enregistrement sont connus et égaux |
//! | 3 | [`Lien::TitreArtisteDuree`] | aucun identifiant ne contredit, et titre normalisé + artiste + durée à ±2 s concordent |
//!
//! Deux identifiants connus et DIFFÉRENTS (ISRC ou MBID) disent « deux
//! enregistrements » : c'est un veto, et aucun rapprochement par le titre ne
//! le lève ([`Relation::Distinct`]).
//!
//! # Une fusion à tort coûte plus qu'une fusion manquée
//!
//! C'est la règle posée sur l'issue le 08/09 et elle commande chaque garde :
//!
//! * le titre est comparé par son NOYAU ([`noyau_de_titre`]) : mêmes mots,
//!   même ordre, ponctuation ignorée. « Heroes - 2017 Remaster » n'est PAS
//!   « Heroes », alors que « Autres versions » les présente bien comme deux
//!   versions l'une de l'autre — proposer n'est pas regrouper ;
//! * un marqueur d'édition ([`MARQUEURS_D_EDITION`] : live, remaster, demo…)
//!   présent d'un côté et absent de l'autre, dans le titre OU l'album, refuse
//!   le rapprochement heuristique : le studio et le live du même morceau ont
//!   souvent le même titre nu et, par accident, la même durée ;
//! * une durée inconnue d'un côté refuse le rapprochement heuristique ;
//! * un candidat n'entre dans un groupe par l'heuristique que s'il concorde
//!   avec le fondateur ET avec chaque membre entré par l'heuristique — pas
//!   avec un seul. Sinon trois pistes à 1,9 s d'écart l'une de l'autre
//!   feraient dériver le groupe de 3,8 s ;
//! * un candidat que l'heuristique admettrait dans DEUX groupes reste seul.

use crate::library::quality::score_qualite;
use crate::library::track_matcher::normaliser_isrc;

/// Les services interrogés pour les versions d'un morceau : par
/// « Autres versions » (`tune-server/src/routes/versions.rs`) et par la règle
/// de lecture. Une seule liste, pour que la lecture ne cherche pas ailleurs que
/// l'écran.
pub const SERVICES_DE_VERSIONS: [&str; 4] = ["qobuz", "tidal", "deezer", "spotify"];

/// Écart de durée toléré par le rapprochement heuristique : ±2 s.
pub const TOLERANCE_DUREE_MS: u64 = 2_000;

/// Les mots qui signalent une AUTRE édition du même morceau.
///
/// Comparés jeton par jeton sur le noyau du titre et de l'album : `live`
/// n'attrape ni « Alive » ni « Oliver ». La liste est volontairement courte
/// et sans « deluxe », « edition » ni « version » — ceux-là habillent une
/// réédition du MÊME master, et les compter ferait manquer l'édition Qobuz
/// d'un album local, le cas que la demande vise d'abord.
///
/// Chaque jeton est rendu sous sa forme CANONIQUE : « Remastered » et
/// « Remaster » disent la même chose, et deux remasters du même master
/// doivent pouvoir se rejoindre.
pub const MARQUEURS_D_EDITION: [(&str, &str); 16] = [
    ("live", "live"),
    ("remaster", "remaster"),
    ("remastered", "remaster"),
    ("remasterise", "remaster"),
    ("remasterisé", "remaster"),
    ("remasterisée", "remaster"),
    ("remix", "remix"),
    ("mix", "mix"),
    ("demo", "demo"),
    ("acoustic", "acoustic"),
    ("acoustique", "acoustic"),
    ("unplugged", "unplugged"),
    ("instrumental", "instrumental"),
    ("edit", "edit"),
    ("mono", "mono"),
    ("karaoke", "karaoke"),
];

/// Le noyau d'un titre : ses jetons alphanumériques, en minuscules, avec
/// l'abréviation « pt(s) » dépliée.
///
/// Déplacé de `tune-server/src/routes/versions.rs` (#4443) : la proposition
/// des versions et leur regroupement doivent lire un titre de la même façon.
/// Deux titres n'ont le même noyau que s'ils portent exactement les mêmes mots
/// dans le même ordre : « Shine on You Crazy Diamond, Pts. 1-5 » et « Shine
/// On You Crazy Diamond (Parts 1-5) » se rejoignent, « Somebody » et
/// « Somebody To Love » non.
pub fn noyau_de_titre(titre: &str) -> String {
    titre
        .split(|c: char| !c.is_alphanumeric())
        .filter(|jeton| !jeton.is_empty())
        .map(|jeton| match jeton.to_lowercase().as_str() {
            "pts" => "parts".to_string(),
            "pt" => "part".to_string(),
            autre => autre.to_string(),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// La qualité d'un exemplaire, telle que la base ou le service la dit.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Qualite {
    /// `flac`, `mp3`… pour un fichier ; le codec annoncé pour un service.
    pub format: Option<String>,
    pub sample_rate: Option<i64>,
    pub bit_depth: Option<i64>,
}

impl Qualite {
    /// Le score de [`score_qualite`], ou `None` quand rien n'est connu : une
    /// qualité inconnue passe APRÈS toute qualité connue, au lieu de valoir
    /// le 44,1/16 que `score_qualite` suppose par défaut.
    pub fn score(&self) -> Option<(bool, i64)> {
        if self.format.is_none() && self.sample_rate.is_none() && self.bit_depth.is_none() {
            return None;
        }
        Some(score_qualite(
            self.format.as_deref(),
            self.sample_rate,
            self.bit_depth,
        ))
    }
}

/// Un exemplaire : une piste de la bibliothèque, ou un résultat de service.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Exemplaire {
    /// `local`, ou le nom du service (`qobuz`, `tidal`…), ou la source d'une
    /// piste de serveur réseau telle que `tracks.source` la porte.
    pub source: String,
    /// L'identifiant en bibliothèque, quand l'exemplaire y est.
    pub track_id: Option<i64>,
    /// L'identifiant chez le service.
    pub source_id: Option<String>,
    pub titre: String,
    pub artiste: String,
    pub album: String,
    pub isrc: Option<String>,
    pub mbid_enregistrement: Option<String>,
    pub duree_ms: Option<i64>,
    pub qualite: Option<Qualite>,
    /// `Some(false)` : le service dit que la piste ne se joue pas aujourd'hui.
    /// `None` : on ne sait pas, et on ne conclut pas.
    pub disponible: Option<bool>,
}

impl Exemplaire {
    pub fn est_local(&self) -> bool {
        self.source.eq_ignore_ascii_case("local")
    }

    fn isrc_normalise(&self) -> Option<String> {
        self.isrc
            .as_deref()
            .map(normaliser_isrc)
            .filter(|s| !s.is_empty())
    }

    fn mbid_normalise(&self) -> Option<String> {
        self.mbid_enregistrement
            .as_deref()
            .map(|m| m.trim().to_ascii_lowercase())
            .filter(|s| !s.is_empty())
    }

    fn score_qualite(&self) -> Option<(bool, i64)> {
        self.qualite.as_ref().and_then(Qualite::score)
    }
}

/// Ce qui a fait entrer un exemplaire dans un groupe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Lien {
    Isrc,
    MbidEnregistrement,
    TitreArtisteDuree,
}

impl Lien {
    /// Le nom publié dans le contrat de la route.
    pub fn nom(self) -> &'static str {
        match self {
            Lien::Isrc => "isrc",
            Lien::MbidEnregistrement => "mbid",
            Lien::TitreArtisteDuree => "title_artist_duration",
        }
    }

    fn par_identifiant(self) -> bool {
        matches!(self, Lien::Isrc | Lien::MbidEnregistrement)
    }
}

/// Ce que deux exemplaires sont l'un pour l'autre.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Relation {
    /// Le même enregistrement, et par quel signe.
    Lien(Lien),
    /// Deux identifiants connus disent « deux enregistrements » : veto.
    Distinct,
    /// Rien ne permet de conclure : pas dans le même groupe.
    Inconnue,
}

fn marqueurs(texte: &str) -> Vec<&'static str> {
    let noyau = noyau_de_titre(texte);
    let jetons: Vec<&str> = noyau.split(' ').collect();
    MARQUEURS_D_EDITION
        .iter()
        .filter(|(jeton, _)| jetons.contains(jeton))
        .map(|(_, canon)| *canon)
        .collect()
}

fn marqueurs_d_edition(e: &Exemplaire) -> Vec<&'static str> {
    let mut m = marqueurs(&e.titre);
    for x in marqueurs(&e.album) {
        if !m.contains(&x) {
            m.push(x);
        }
    }
    m.sort_unstable();
    m.dedup();
    m
}

/// La règle d'identité, appliquée à deux exemplaires. Voir l'en-tête.
pub fn relation(a: &Exemplaire, b: &Exemplaire) -> Relation {
    let (ia, ib) = (a.isrc_normalise(), b.isrc_normalise());
    let (ma, mb) = (a.mbid_normalise(), b.mbid_normalise());

    if let (Some(x), Some(y)) = (&ia, &ib)
        && x == y
    {
        return Relation::Lien(Lien::Isrc);
    }
    // Un enregistrement MusicBrainz peut porter plusieurs ISRC : un MBID égal
    // l'emporte sur deux ISRC différents.
    if let (Some(x), Some(y)) = (&ma, &mb)
        && x == y
    {
        return Relation::Lien(Lien::MbidEnregistrement);
    }
    if (ia.is_some() && ib.is_some()) || (ma.is_some() && mb.is_some()) {
        return Relation::Distinct;
    }

    let (ta, tb) = (noyau_de_titre(&a.titre), noyau_de_titre(&b.titre));
    let (aa, ab) = (noyau_de_titre(&a.artiste), noyau_de_titre(&b.artiste));
    if ta.is_empty() || ta != tb || aa.is_empty() || aa != ab {
        return Relation::Inconnue;
    }
    let duree_concorde = match (a.duree_ms, b.duree_ms) {
        (Some(x), Some(y)) if x > 0 && y > 0 => x.abs_diff(y) <= TOLERANCE_DUREE_MS,
        _ => false,
    };
    if !duree_concorde || marqueurs_d_edition(a) != marqueurs_d_edition(b) {
        return Relation::Inconnue;
    }
    Relation::Lien(Lien::TitreArtisteDuree)
}

/// Un membre d'un groupe : l'indice de l'exemplaire dans l'entrée, et le lien
/// qui l'a fait entrer (`None` pour le premier membre, qui fonde le groupe).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Membre {
    pub indice: usize,
    pub lien: Option<Lien>,
}

/// Un groupe : des exemplaires d'un même enregistrement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Groupe {
    pub membres: Vec<Membre>,
}

impl Groupe {
    /// Le lien le plus FAIBLE qui tient le groupe : `Isrc` si tous les
    /// membres y sont entrés par l'ISRC, `TitreArtisteDuree` dès qu'un seul y
    /// est entré par l'heuristique, `None` pour un groupe d'un seul membre.
    pub fn identite(&self) -> Option<Lien> {
        self.membres.iter().filter_map(|m| m.lien).max()
    }
}

/// Peut-on ajouter `candidat` à `groupe` ? Le lien retenu, ou `None`.
///
/// * un veto ([`Relation::Distinct`]) avec UN membre ferme le groupe ;
/// * un lien par identifiant avec un membre suffit ;
/// * sinon, l'heuristique doit concorder avec le FONDATEUR et avec chaque
///   membre entré par l'heuristique. Les membres entrés par identifiant en
///   sont dispensés : un single sous le même ISRC peut durer 15 s de moins,
///   l'identifiant a déjà tranché pour lui.
fn admission(exemplaires: &[Exemplaire], groupe: &Groupe, candidat: &Exemplaire) -> Option<Lien> {
    let relations: Vec<(Option<Lien>, Relation)> = groupe
        .membres
        .iter()
        .map(|m| (m.lien, relation(&exemplaires[m.indice], candidat)))
        .collect();
    if relations.iter().any(|(_, r)| *r == Relation::Distinct) {
        return None;
    }
    let par_identifiant = relations
        .iter()
        .filter_map(|(_, r)| match r {
            Relation::Lien(l) if l.par_identifiant() => Some(*l),
            _ => None,
        })
        .min();
    if par_identifiant.is_some() {
        return par_identifiant;
    }
    let concorde_partout = relations
        .iter()
        .filter(|(entree, _)| entree.is_none_or(|l| !l.par_identifiant()))
        .all(|(_, r)| *r == Relation::Lien(Lien::TitreArtisteDuree));
    concorde_partout.then_some(Lien::TitreArtisteDuree)
}

/// Regroupe les exemplaires. L'exemplaire d'indice 0 est la RÉFÉRENCE : son
/// groupe sort en premier.
///
/// Les exemplaires porteurs d'un identifiant sont placés avant les autres, à
/// rang égal dans l'ordre reçu : un groupe se fonde sur ce qui est sûr avant
/// d'accueillir ce qui est probable. Le résultat ne dépend que de l'entrée.
///
/// Un exemplaire que la SEULE heuristique admettrait dans deux groupes est
/// ambigu — l'original et son remaster, mêmes titre et durée, deux ISRC : un
/// résultat de service sans ISRC peut être l'un ou l'autre. Il reste seul.
pub fn grouper(exemplaires: &[Exemplaire]) -> Vec<Groupe> {
    let mut ordre: Vec<usize> = (0..exemplaires.len()).collect();
    ordre.sort_by_key(|&i| {
        let e = &exemplaires[i];
        let identifie = e.isrc_normalise().is_some() || e.mbid_normalise().is_some();
        (i != 0, !identifie, i)
    });
    let mut groupes: Vec<Groupe> = Vec::new();
    for i in ordre {
        let candidat = &exemplaires[i];
        let admis: Vec<(usize, Lien)> = groupes
            .iter()
            .enumerate()
            .filter_map(|(g, groupe)| admission(exemplaires, groupe, candidat).map(|l| (g, l)))
            .collect();
        let place = admis
            .iter()
            .find(|(_, l)| l.par_identifiant())
            .or_else(|| (admis.len() == 1).then(|| &admis[0]))
            .copied();
        match place {
            Some((g, lien)) => groupes[g].membres.push(Membre {
                indice: i,
                lien: Some(lien),
            }),
            None => groupes.push(Groupe {
                membres: vec![Membre {
                    indice: i,
                    lien: None,
                }],
            }),
        }
    }
    groupes
}

/// La règle qui désigne la version jouée par défaut dans un groupe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegleDeChoix {
    /// La bibliothèque d'abord, puis la meilleure qualité.
    PrefererLocal,
    /// La meilleure qualité connue, puis la bibliothèque.
    MeilleureQualite,
    /// Ce service d'abord, puis la bibliothèque, puis la meilleure qualité.
    PrefererService(String),
}

impl RegleDeChoix {
    /// La règle quand rien n'est réglé : la bibliothèque d'abord, ce qui ne
    /// change rien à ce que l'auditeur joue aujourd'hui.
    pub const DEFAUT: RegleDeChoix = RegleDeChoix::PrefererLocal;

    /// Lit `local`, `quality` ou `service:<nom>`. `None` pour toute autre
    /// forme : une règle mal écrite se refuse, elle ne se devine pas.
    pub fn depuis(texte: &str) -> Option<RegleDeChoix> {
        let t = texte.trim();
        match t {
            "local" => Some(RegleDeChoix::PrefererLocal),
            "quality" => Some(RegleDeChoix::MeilleureQualite),
            _ => {
                let nom = t.strip_prefix("service:")?.trim().to_ascii_lowercase();
                let valide = !nom.is_empty()
                    && nom.len() <= 32
                    && nom
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
                valide.then_some(RegleDeChoix::PrefererService(nom))
            }
        }
    }

    /// La forme écrite, l'inverse exact de [`RegleDeChoix::depuis`].
    pub fn texte(&self) -> String {
        match self {
            RegleDeChoix::PrefererLocal => "local".to_string(),
            RegleDeChoix::MeilleureQualite => "quality".to_string(),
            RegleDeChoix::PrefererService(s) => format!("service:{s}"),
        }
    }
}

/// L'exemplaire joué par défaut parmi `indices`, selon `regle`.
///
/// Un exemplaire que son service déclare indisponible (`disponible ==
/// Some(false)`) n'est jamais choisi ; `None` quand il ne reste rien. Le
/// départage final (source, puis identifiants) rend le choix total : deux
/// appels sur la même entrée choisissent le même exemplaire.
pub fn choisir(
    exemplaires: &[Exemplaire],
    indices: &[usize],
    regle: &RegleDeChoix,
) -> Option<usize> {
    choisir_parmi(exemplaires, indices, regle, true)
}

/// Le choix et ce qu'il dit du REPLI (décision 2 du 07/10/2026).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Choix {
    /// L'exemplaire joué.
    pub indice: usize,
    /// L'exemplaire que la règle aurait pris si tout était disponible, quand
    /// ce n'est PAS celui qui est joué : la version préférée est indisponible
    /// et on est passé à la suivante. `None` : pas de repli.
    pub prefere_indisponible: Option<usize>,
}

impl Choix {
    pub fn repli(&self) -> bool {
        self.prefere_indisponible.is_some()
    }
}

/// [`choisir`], en disant en plus si le choix est un REPLI.
///
/// La version préférée est celle que la règle prendrait si aucun exemplaire
/// n'était indisponible. Quand elle l'est, la règle passe à la suivante
/// disponible — et le dit, pour que la lecture le signale au lieu de changer
/// de version en silence.
pub fn choisir_avec_repli(
    exemplaires: &[Exemplaire],
    indices: &[usize],
    regle: &RegleDeChoix,
) -> Option<Choix> {
    let indice = choisir_parmi(exemplaires, indices, regle, true)?;
    let prefere = choisir_parmi(exemplaires, indices, regle, false);
    Some(Choix {
        indice,
        prefere_indisponible: prefere.filter(|&p| p != indice),
    })
}

fn choisir_parmi(
    exemplaires: &[Exemplaire],
    indices: &[usize],
    regle: &RegleDeChoix,
    respecter_la_disponibilite: bool,
) -> Option<usize> {
    use std::cmp::Reverse;
    indices
        .iter()
        .copied()
        .filter(|&i| !respecter_la_disponibilite || exemplaires[i].disponible != Some(false))
        .min_by_key(|&i| {
            let e = &exemplaires[i];
            // Chaque critère est un couple « plus grand = mieux ». Une qualité
            // inconnue vaut (0, 0) : elle passe après toute qualité connue.
            let qualite = e
                .score_qualite()
                .map(|(sans_perte, debit)| (1 + i64::from(sans_perte), debit))
                .unwrap_or((0, 0));
            let local = (i64::from(e.est_local()), 0);
            let priorite: Vec<(i64, i64)> = match regle {
                RegleDeChoix::PrefererLocal => vec![local, qualite],
                RegleDeChoix::MeilleureQualite => vec![qualite, local],
                RegleDeChoix::PrefererService(s) => vec![
                    (i64::from(e.source.eq_ignore_ascii_case(s)), 0),
                    local,
                    qualite,
                ],
            };
            (
                Reverse(priorite),
                e.source.to_ascii_lowercase(),
                e.track_id,
                e.source_id.clone(),
            )
        })
}

#[cfg(test)]
#[path = "groupes_versions_tests.rs"]
mod tests;
