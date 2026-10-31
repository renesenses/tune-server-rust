//! Modifier UN champ de métadonnées sur TOUT le résultat d'une vue filtrée
//! d'Oxygen, en un geste (#7539, GO de Bertrand du 30/10/2026) : genre,
//! année, label, artiste d'album ou compositeur.
//!
//! # Même désignation que l'étiquetage par filtre (#7128)
//!
//! Le navigateur n'envoie aucune liste d'identifiants : le serveur reçoit les
//! paramètres de facette de `GET /library/tracks` et désigne lui-même
//! l'ensemble ([`TrackRepo::sous_requete_du_filtre`]). Ce qui est modifié est
//! exactement ce que la vue montre — en cible « pistes », les pistes du
//! résultat ; en cible « albums », les albums du résultat ET toutes leurs
//! pistes. L'artiste d'album se pose toujours sur les albums.
//!
//! # Le plan, puis l'écriture, dans UNE transaction
//!
//! [`TrackRepo::planifier_edition_par_filtre`] calcule, sans rien écrire, ce
//! que le geste ferait : pistes et albums réellement changés, exemples
//! avant/après, pistes écartées. L'aperçu l'affiche tel quel.
//! [`TrackRepo::appliquer_edition_par_filtre`] RECALCULE ce plan dans la
//! transaction d'écriture (rien ne bouge entre la lecture et l'écriture),
//! refuse si le compte diffère de celui que l'utilisateur a vu, puis écrit
//! pistes, albums, tenues et journal — tout passe, ou rien.
//!
//! # Cohérence du genre (#5314, #6635)
//!
//! Un genre se pose comme la fiche album le recopie sur ses pistes
//! ([`genre_album_pistes::colonnes_de_piste`]) : `tracks.genre` reçoit le
//! premier genre, `tracks.genres` le tableau JSON de tous, `albums.genre` la
//! valeur saisie et `albums.genres` le même tableau. En cible « pistes », un
//! album ne change que si TOUTES ses pistes portent ensuite la nouvelle
//! valeur : une moitié d'album retaguée ne réécrit pas le genre de l'album.
//! Même règle pour l'année et le label.
//!
//! # Ce qui n'est jamais touché en silence
//!
//! Les pistes et albums distants (`source` autre que `local` : services,
//! serveurs UPnP) sont écartés et COMPTÉS dans l'aperçu. Les pistes d'une
//! feuille CUE sont modifiées en base mais leur fichier n'est pas réécrit
//! (quinze pistes, une image : voir `write_tags.rs`), et l'aperçu le dit.
//!
//! # Survivre à l'analyse suivante
//!
//! Chaque piste modifiée TIENT son champ ([`super::super::champs_tenus`]),
//! chaque album modifié le marque `edition_manuelle` : une « Analyse
//! complète » ne remet pas les anciennes balises.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use super::{Placeholders, SqlValue, ToSqlValue, TrackFilter, TrackRepo};
use crate::TuneError;
use crate::db::album_metadata_repo::CLE_EDITION_MANUELLE;
use crate::db::champs_tenus::{self, ChampsTenus};
use crate::db::engine::{Engine, PostgresDialect, SqlDialect, SqliteDialect};
use crate::db::genre_album_pistes::{self, CLE_GENRE_PISTES};
use crate::db::track_repo::CibleDuFiltre;

/// Nombre maximal de PISTES qu'un seul geste peut modifier. Plus bas que le
/// plafond de l'étiquetage (20 000) : une modification de métadonnées
/// réécrit des lignes et, réglage coché, des fichiers.
pub const PLAFOND_EDITION_PAR_FILTRE: i64 = 5_000;

/// Exemples avant/après rendus par l'aperçu.
pub const EXEMPLES_MAX: usize = 5;

/// Longueur maximale d'une valeur texte, en caractères.
pub const LONGUEUR_MAX: usize = 200;

/// Clé `settings` du journal des gestes (les 20 derniers).
pub const CLE_JOURNAL: &str = "edit_by_filter_journal";
const JOURNAL_MAX: usize = 20;

/// Les champs qu'on peut modifier en masse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChampDeMasse {
    Genre,
    Annee,
    Label,
    ArtisteAlbum,
    Compositeur,
}

impl ChampDeMasse {
    pub fn depuis_nom(nom: &str) -> Option<Self> {
        match nom {
            "genre" => Some(Self::Genre),
            "year" => Some(Self::Annee),
            "label" => Some(Self::Label),
            "album_artist" => Some(Self::ArtisteAlbum),
            "composer" => Some(Self::Compositeur),
            _ => None,
        }
    }

    pub fn nom(self) -> &'static str {
        match self {
            Self::Genre => "genre",
            Self::Annee => "year",
            Self::Label => "label",
            Self::ArtisteAlbum => "album_artist",
            Self::Compositeur => "composer",
        }
    }

    /// La cible réellement appliquée : l'artiste d'album est une propriété
    /// de l'album, il se pose toujours sur les albums du résultat.
    pub fn cible_effective(self, demandee: CibleDuFiltre) -> CibleDuFiltre {
        match self {
            Self::ArtisteAlbum => CibleDuFiltre::Albums,
            _ => demandee,
        }
    }

    fn champ_tenu(self) -> champs_tenus::Champ {
        match self {
            Self::Genre => champs_tenus::Champ::Genre,
            Self::Annee => champs_tenus::Champ::Annee,
            Self::Label => champs_tenus::Champ::Label,
            Self::ArtisteAlbum => champs_tenus::Champ::ArtisteAlbum,
            Self::Compositeur => champs_tenus::Champ::Compositeur,
        }
    }

    /// Le nom du champ dans `edition_manuelle` (celui de `batch_update_albums`).
    fn champ_album(self) -> Option<&'static str> {
        match self {
            Self::Genre => Some("genre"),
            Self::Annee => Some("year"),
            Self::Label => Some("label"),
            Self::ArtisteAlbum => Some("artist"),
            Self::Compositeur => None,
        }
    }
}

/// Une valeur validée.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValeurDeMasse {
    Texte(String),
    Annee(i32),
}

impl ValeurDeMasse {
    pub fn affichage(&self) -> String {
        match self {
            Self::Texte(s) => s.clone(),
            Self::Annee(a) => a.to_string(),
        }
    }
}

/// Pourquoi une modification par filtre est refusée avant toute écriture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefusEdition {
    FiltreVide,
    TropDePistes {
        nombre: i64,
        plafond: i64,
    },
    ValeurVide,
    AnneeInvalide,
    ValeurTropLongue {
        max: usize,
    },
    /// Le compte de la transaction diffère de celui de l'aperçu confirmé.
    ApercuPerime {
        pistes: i64,
        albums: i64,
    },
}

/// Valide la valeur saisie pour `champ`. Une valeur vide est refusée : ce
/// geste POSE une valeur, il n'efface pas un champ sur des milliers de pistes.
pub fn valider_valeur(champ: ChampDeMasse, brut: &str) -> Result<ValeurDeMasse, RefusEdition> {
    let v = brut.trim();
    if v.is_empty() {
        return Err(RefusEdition::ValeurVide);
    }
    if champ == ChampDeMasse::Annee {
        if v.len() != 4 || !v.bytes().all(|b| b.is_ascii_digit()) {
            return Err(RefusEdition::AnneeInvalide);
        }
        let a: i32 = v.parse().map_err(|_| RefusEdition::AnneeInvalide)?;
        if a < 1000 {
            return Err(RefusEdition::AnneeInvalide);
        }
        return Ok(ValeurDeMasse::Annee(a));
    }
    if v.chars().count() > LONGUEUR_MAX {
        return Err(RefusEdition::ValeurTropLongue { max: LONGUEUR_MAX });
    }
    if v.chars().any(|c| c.is_control()) {
        return Err(RefusEdition::ValeurVide);
    }
    Ok(ValeurDeMasse::Texte(v.to_string()))
}

/// Un exemple avant/après.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Exemple {
    pub id: i64,
    pub title: String,
    /// L'album d'une piste, l'artiste d'un album.
    pub context: Option<String>,
    pub before: Option<String>,
    pub after: String,
}

/// Une piste dont le fichier serait réécrit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FichierAEcrire {
    pub track_id: i64,
    pub file_path: String,
}

/// Ce que le geste ferait (aperçu) ou a fait (bilan).
#[derive(Debug, Clone, Default)]
pub struct PlanEdition {
    /// Pistes locales du périmètre (avant le tri « déjà à la valeur »).
    pub pistes_visees: i64,
    /// Pistes réellement changées.
    pub pistes_changees: Vec<i64>,
    /// Pistes du périmètre qui portaient déjà la valeur.
    pub pistes_deja: i64,
    /// Pistes distantes écartées.
    pub pistes_distantes: i64,
    /// Albums réellement changés.
    pub albums_changes: Vec<i64>,
    /// Albums distants écartés.
    pub albums_distants: i64,
    /// Fichiers que l'écriture réécrirait (pistes changées, fichier propre).
    pub fichiers: Vec<FichierAEcrire>,
    /// Pistes changées d'une feuille CUE : base seulement.
    pub fichiers_cue: i64,
    pub exemples_pistes: Vec<Exemple>,
    pub exemples_albums: Vec<Exemple>,
}

type Lecteur<'a> = &'a dyn Fn(&str, &[&dyn ToSqlValue]) -> Result<Vec<Vec<SqlValue>>, String>;

fn texte(v: Option<&SqlValue>) -> Option<String> {
    v.and_then(|v| v.as_string())
        .filter(|s| !s.trim().is_empty())
}

fn liste_ids(ids: &[i64]) -> String {
    ids.iter()
        .map(|i| i.to_string())
        .collect::<Vec<_>>()
        .join(",")
}

/// Par tranches : une liste `IN (…)` de 5 000 entiers passe, mais rien
/// n'oblige à la construire d'un bloc.
const TRANCHE: usize = 500;

fn genres_affiches(genre: Option<&str>, genres: Option<&str>) -> Option<String> {
    let liste: Vec<String> = genres
        .and_then(|j| serde_json::from_str::<Vec<String>>(j).ok())
        .unwrap_or_default()
        .into_iter()
        .filter(|g| !g.trim().is_empty())
        .collect();
    if !liste.is_empty() {
        return Some(liste.join("; "));
    }
    genre.map(str::to_string).filter(|g| !g.trim().is_empty())
}

/// La forme de la valeur côté pistes et côté album.
struct Cible {
    champ: ChampDeMasse,
    valeur: ValeurDeMasse,
    /// Genre : `(tracks.genre, tracks.genres JSON)`.
    genre_pistes: Option<(String, String)>,
}

impl Cible {
    fn nouvelle(champ: ChampDeMasse, valeur: ValeurDeMasse) -> Self {
        let genre_pistes = match (&champ, &valeur) {
            (ChampDeMasse::Genre, ValeurDeMasse::Texte(g)) => {
                genre_album_pistes::colonnes_de_piste(g)
            }
            _ => None,
        };
        Self {
            champ,
            valeur,
            genre_pistes,
        }
    }

    fn texte(&self) -> String {
        self.valeur.affichage()
    }

    /// La valeur effective d'une piste pour ce champ, et si elle diffère de
    /// la nouvelle. Colonnes : genre, genres, year, label, composer,
    /// album_artist effectif.
    fn piste(&self, ligne: &LignePiste) -> (Option<String>, bool) {
        match self.champ {
            ChampDeMasse::Genre => {
                let avant = genres_affiches(ligne.genre.as_deref(), ligne.genres.as_deref());
                let differe = match &self.genre_pistes {
                    Some((premier, json)) => {
                        ligne.genre.as_deref() != Some(premier.as_str())
                            || ligne.genres.as_deref() != Some(json.as_str())
                    }
                    None => false,
                };
                (avant, differe)
            }
            ChampDeMasse::Annee => {
                let avant = ligne.year.map(|y| y.to_string());
                let differe = match self.valeur {
                    ValeurDeMasse::Annee(a) => ligne.year != Some(a as i64),
                    _ => false,
                };
                (avant, differe)
            }
            ChampDeMasse::Label => {
                let differe = ligne.label.as_deref() != Some(self.texte().as_str());
                (ligne.label.clone(), differe)
            }
            ChampDeMasse::Compositeur => {
                let differe = ligne.composer.as_deref() != Some(self.texte().as_str());
                (ligne.composer.clone(), differe)
            }
            ChampDeMasse::ArtisteAlbum => {
                let differe = ligne.album_artist.as_deref() != Some(self.texte().as_str());
                (ligne.album_artist.clone(), differe)
            }
        }
    }

    fn album(&self, a: &LigneAlbum) -> (Option<String>, bool) {
        match self.champ {
            ChampDeMasse::Genre => {
                let differe = match &self.genre_pistes {
                    Some((_, json)) => {
                        a.genre.as_deref() != Some(self.texte().as_str())
                            || a.genres.as_deref() != Some(json.as_str())
                    }
                    None => false,
                };
                (a.genre.clone(), differe)
            }
            ChampDeMasse::Annee => {
                let differe = match self.valeur {
                    ValeurDeMasse::Annee(v) => a.year != Some(v as i64),
                    _ => false,
                };
                (a.year.map(|y| y.to_string()), differe)
            }
            ChampDeMasse::Label => (
                a.label.clone(),
                a.label.as_deref() != Some(self.texte().as_str()),
            ),
            ChampDeMasse::ArtisteAlbum => (
                a.artiste.clone(),
                a.artiste.as_deref() != Some(self.texte().as_str()),
            ),
            ChampDeMasse::Compositeur => (None, false),
        }
    }
}

#[derive(Debug, Clone)]
struct LignePiste {
    id: i64,
    album_id: Option<i64>,
    locale: bool,
    file_path: Option<String>,
    cue_media: Option<String>,
    titre: String,
    album_titre: Option<String>,
    genre: Option<String>,
    genres: Option<String>,
    year: Option<i64>,
    label: Option<String>,
    composer: Option<String>,
    album_artist: Option<String>,
}

#[derive(Debug, Clone)]
struct LigneAlbum {
    id: i64,
    titre: String,
    locale: bool,
    genre: Option<String>,
    genres: Option<String>,
    year: Option<i64>,
    label: Option<String>,
    artiste: Option<String>,
}

const COLONNES_PISTE: &str = "t.id, t.album_id, t.source, t.file_path, t.cue_media_path, \
     t.cue_start_ms, t.title, al.title, t.genre, t.genres, t.year, t.label, t.composer, \
     COALESCE(NULLIF(t.album_artist, ''), aal.name)";

const FROM_PISTE: &str = "FROM tracks t LEFT JOIN albums al ON al.id = t.album_id \
     LEFT JOIN artists aal ON aal.id = al.artist_id";

fn est_locale(source: Option<String>) -> bool {
    source.is_none_or(|s| s.trim().is_empty() || s == "local")
}

fn ligne_piste(r: &[SqlValue]) -> Option<LignePiste> {
    Some(LignePiste {
        id: r.first()?.as_i64()?,
        album_id: r.get(1).and_then(|v| v.as_i64()),
        locale: est_locale(r.get(2).and_then(|v| v.as_string())),
        file_path: texte(r.get(3)),
        cue_media: texte(r.get(4)),
        titre: r.get(6).and_then(|v| v.as_string()).unwrap_or_default(),
        album_titre: texte(r.get(7)),
        genre: texte(r.get(8)),
        genres: texte(r.get(9)),
        year: r.get(10).and_then(|v| v.as_i64()),
        label: texte(r.get(11)),
        composer: texte(r.get(12)),
        album_artist: texte(r.get(13)),
    })
}

fn lire_albums(lire: Lecteur<'_>, ids: &[i64]) -> Result<Vec<LigneAlbum>, String> {
    let mut sortie = Vec::new();
    for tranche in ids.chunks(TRANCHE) {
        let sql = format!(
            "SELECT al.id, al.title, al.source, al.genre, al.genres, al.year, al.label, aal.name \
             FROM albums al LEFT JOIN artists aal ON aal.id = al.artist_id \
             WHERE al.id IN ({}) ORDER BY al.id",
            liste_ids(tranche)
        );
        for r in lire(&sql, &[])? {
            let Some(id) = r.first().and_then(|v| v.as_i64()) else {
                continue;
            };
            sortie.push(LigneAlbum {
                id,
                titre: r.get(1).and_then(|v| v.as_string()).unwrap_or_default(),
                locale: est_locale(r.get(2).and_then(|v| v.as_string())),
                genre: texte(r.get(3)),
                genres: texte(r.get(4)),
                year: r.get(5).and_then(|v| v.as_i64()),
                label: texte(r.get(6)),
                artiste: texte(r.get(7)),
            });
        }
    }
    Ok(sortie)
}

fn pistes_des_albums(lire: Lecteur<'_>, ids: &[i64]) -> Result<Vec<LignePiste>, String> {
    let mut sortie = Vec::new();
    for tranche in ids.chunks(TRANCHE) {
        let sql = format!(
            "SELECT {COLONNES_PISTE} {FROM_PISTE} WHERE t.album_id IN ({}) ORDER BY t.id",
            liste_ids(tranche)
        );
        sortie.extend(lire(&sql, &[])?.iter().filter_map(|r| ligne_piste(r)));
    }
    Ok(sortie)
}

fn marque(engine: Engine, n: usize) -> String {
    match engine {
        Engine::Sqlite => SqliteDialect.placeholder(n),
        Engine::Postgres => PostgresDialect.placeholder(n),
    }
}

impl TrackRepo {
    /// Le nombre de pistes du périmètre (distantes comprises) : la mesure du
    /// plafond, avant toute lecture des lignes.
    fn compter_pistes_du_perimetre(
        &self,
        lire: Lecteur<'_>,
        f: &TrackFilter,
        cible: CibleDuFiltre,
    ) -> Result<i64, String> {
        let mut ph = Placeholders::new(self.db.engine());
        let (sous, params) = self.sous_requete_du_filtre(f, cible, &mut ph);
        let colonne = match cible {
            CibleDuFiltre::Pistes => "t.id",
            CibleDuFiltre::Albums => "t.album_id",
        };
        let sql = format!("SELECT COUNT(*) FROM tracks t WHERE {colonne} IN ({sous})");
        let refs: Vec<&dyn ToSqlValue> = params.iter().map(|v| v as &dyn ToSqlValue).collect();
        Ok(lire(&sql, &refs)?
            .first()
            .and_then(|r| r.first())
            .and_then(|v| v.as_i64())
            .unwrap_or(0))
    }

    fn lire_perimetre(
        &self,
        lire: Lecteur<'_>,
        f: &TrackFilter,
        cible: CibleDuFiltre,
    ) -> Result<Vec<LignePiste>, String> {
        let mut ph = Placeholders::new(self.db.engine());
        let (sous, params) = self.sous_requete_du_filtre(f, cible, &mut ph);
        let colonne = match cible {
            CibleDuFiltre::Pistes => "t.id",
            CibleDuFiltre::Albums => "t.album_id",
        };
        let sql = format!(
            "SELECT {COLONNES_PISTE} {FROM_PISTE} WHERE {colonne} IN ({sous}) ORDER BY t.id"
        );
        let refs: Vec<&dyn ToSqlValue> = params.iter().map(|v| v as &dyn ToSqlValue).collect();
        Ok(lire(&sql, &refs)?
            .iter()
            .filter_map(|r| ligne_piste(r))
            .collect())
    }

    /// Le plan du geste, lu par `lire` (la base, ou la transaction).
    fn planifier(
        &self,
        lire: Lecteur<'_>,
        f: &TrackFilter,
        cible: CibleDuFiltre,
        c: &Cible,
    ) -> Result<Result<PlanEdition, RefusEdition>, String> {
        if !f.is_active() {
            return Ok(Err(RefusEdition::FiltreVide));
        }
        let cible = c.champ.cible_effective(cible);
        let nombre = self.compter_pistes_du_perimetre(lire, f, cible)?;
        if nombre > PLAFOND_EDITION_PAR_FILTRE {
            return Ok(Err(RefusEdition::TropDePistes {
                nombre,
                plafond: PLAFOND_EDITION_PAR_FILTRE,
            }));
        }
        let lignes = self.lire_perimetre(lire, f, cible)?;
        let mut plan = PlanEdition::default();
        let mut dans_le_perimetre: BTreeSet<i64> = BTreeSet::new();
        let mut albums_candidats: BTreeSet<i64> = BTreeSet::new();
        let mut albums_des_distantes: BTreeSet<i64> = BTreeSet::new();
        for l in &lignes {
            if !l.locale {
                plan.pistes_distantes += 1;
                albums_des_distantes.extend(l.album_id);
                continue;
            }
            plan.pistes_visees += 1;
            dans_le_perimetre.insert(l.id);
            if let Some(a) = l.album_id {
                albums_candidats.insert(a);
            }
            let (avant, differe) = c.piste(l);
            if !differe {
                plan.pistes_deja += 1;
                continue;
            }
            plan.pistes_changees.push(l.id);
            match (&l.file_path, &l.cue_media) {
                (_, Some(_)) => plan.fichiers_cue += 1,
                (Some(chemin), None) => plan.fichiers.push(FichierAEcrire {
                    track_id: l.id,
                    file_path: chemin.clone(),
                }),
                (None, None) => {}
            }
            if plan.exemples_pistes.len() < EXEMPLES_MAX {
                plan.exemples_pistes.push(Exemple {
                    id: l.id,
                    title: l.titre.clone(),
                    context: l.album_titre.clone(),
                    before: avant,
                    after: c.texte(),
                });
            }
        }

        // Les albums distants du résultat : écartés, et COMPTÉS dans l'aperçu.
        let seuls_distants: Vec<i64> = albums_des_distantes
            .difference(&albums_candidats)
            .copied()
            .collect();
        if c.champ != ChampDeMasse::Compositeur && !seuls_distants.is_empty() {
            plan.albums_distants += lire_albums(lire, &seuls_distants)?
                .iter()
                .filter(|a| !a.locale)
                .count() as i64;
        }

        if c.champ != ChampDeMasse::Compositeur && !albums_candidats.is_empty() {
            let ids: Vec<i64> = albums_candidats.iter().copied().collect();
            let albums = lire_albums(lire, &ids)?;
            // Cible « pistes » : un album ne change que si toutes ses pistes
            // portent ensuite la valeur — celles du périmètre la reçoivent,
            // les autres doivent l'avoir déjà.
            let completes: Option<BTreeSet<i64>> = if cible == CibleDuFiltre::Pistes {
                let mut incompletes: BTreeSet<i64> = BTreeSet::new();
                for p in pistes_des_albums(lire, &ids)? {
                    if dans_le_perimetre.contains(&p.id) {
                        continue;
                    }
                    if c.piste(&p).1
                        && let Some(a) = p.album_id
                    {
                        incompletes.insert(a);
                    }
                }
                Some(
                    ids.iter()
                        .copied()
                        .filter(|a| !incompletes.contains(a))
                        .collect(),
                )
            } else {
                None
            };
            for a in &albums {
                if !a.locale {
                    plan.albums_distants += 1;
                    continue;
                }
                if let Some(ok) = &completes
                    && !ok.contains(&a.id)
                {
                    continue;
                }
                let (avant, differe) = c.album(a);
                if !differe {
                    continue;
                }
                plan.albums_changes.push(a.id);
                if plan.exemples_albums.len() < EXEMPLES_MAX {
                    plan.exemples_albums.push(Exemple {
                        id: a.id,
                        title: a.titre.clone(),
                        context: a.artiste.clone(),
                        before: avant,
                        after: c.texte(),
                    });
                }
            }
        }
        Ok(Ok(plan))
    }

    /// L'aperçu : ce que le geste ferait, sans rien écrire.
    pub fn planifier_edition_par_filtre(
        &self,
        f: &TrackFilter,
        cible: CibleDuFiltre,
        champ: ChampDeMasse,
        valeur: ValeurDeMasse,
    ) -> Result<Result<PlanEdition, RefusEdition>, TuneError> {
        let c = Cible::nouvelle(champ, valeur);
        let db = self.db.clone();
        let lire = move |s: &str, p: &[&dyn ToSqlValue]| db.query_many(s, p);
        self.planifier(&lire, f, cible, &c).map_err(TuneError::Db)
    }

    /// Applique le geste dans UNE transaction. `artiste_id` : l'artiste de la
    /// valeur, résolu (ou créé) par l'appelant AVANT, pour le champ artiste
    /// d'album. `attendu` : `(pistes, albums)` de l'aperçu confirmé — un
    /// écart refuse sans rien écrire.
    pub fn appliquer_edition_par_filtre(
        &self,
        f: &TrackFilter,
        cible: CibleDuFiltre,
        champ: ChampDeMasse,
        valeur: ValeurDeMasse,
        artiste_id: Option<i64>,
        attendu: Option<(i64, i64)>,
    ) -> Result<Result<PlanEdition, RefusEdition>, TuneError> {
        if champ == ChampDeMasse::ArtisteAlbum && artiste_id.is_none() {
            return Err(TuneError::Db("artiste d'album sans artiste résolu".into()));
        }
        let c = Cible::nouvelle(champ, valeur);
        let engine = self.db.engine();
        let mut sortie: Option<Result<PlanEdition, RefusEdition>> = None;
        self.db
            .write_tx(&mut |tx| {
                let lire = |s: &str, p: &[&dyn ToSqlValue]| tx.query_many(s, p);
                let plan = match self.planifier(&lire, f, cible, &c)? {
                    Ok(p) => p,
                    Err(r) => {
                        sortie = Some(Err(r));
                        return Ok(());
                    }
                };
                let pistes = plan.pistes_changees.len() as i64;
                let albums = plan.albums_changes.len() as i64;
                if let Some((p, a)) = attendu
                    && (p, a) != (pistes, albums)
                {
                    sortie = Some(Err(RefusEdition::ApercuPerime { pistes, albums }));
                    return Ok(());
                }
                ecrire_pistes(tx, engine, &c, &plan.pistes_changees)?;
                ecrire_albums(tx, engine, &c, &plan.albums_changes, artiste_id)?;
                tenir_les_pistes(tx, engine, c.champ, &plan.pistes_changees)?;
                marquer_les_albums(tx, engine, &c, &plan.albums_changes)?;
                journaliser(tx, engine, &c, cible, &plan)?;
                sortie = Some(Ok(plan));
                Ok(())
            })
            .map_err(TuneError::Db)?;
        let sortie = sortie.unwrap_or(Err(RefusEdition::FiltreVide));
        if let Ok(plan) = &sortie {
            tracing::info!(
                champ = champ.nom(),
                valeur = %c.texte(),
                cible = cible.item_type(),
                pistes = plan.pistes_changees.len(),
                albums = plan.albums_changes.len(),
                deja = plan.pistes_deja,
                distantes = plan.pistes_distantes,
                fichiers = plan.fichiers.len(),
                "edition_par_filtre_appliquee"
            );
        }
        Ok(sortie)
    }
}

fn ecrire_pistes(
    tx: &dyn crate::db::backend::DbTxHandle,
    engine: Engine,
    c: &Cible,
    ids: &[i64],
) -> Result<(), String> {
    let p1 = marque(engine, 1);
    let p2 = marque(engine, 2);
    for tranche in ids.chunks(TRANCHE) {
        let liste = liste_ids(tranche);
        match (c.champ, &c.valeur) {
            (ChampDeMasse::Genre, _) => {
                let Some((premier, json)) = &c.genre_pistes else {
                    return Err("genre sans valeur".into());
                };
                tx.execute(
                    &format!("UPDATE tracks SET genre = {p1}, genres = {p2} WHERE id IN ({liste})"),
                    &[premier as &dyn ToSqlValue, json as &dyn ToSqlValue],
                )?;
            }
            (ChampDeMasse::Annee, ValeurDeMasse::Annee(a)) => {
                let a = *a as i64;
                tx.execute(
                    &format!("UPDATE tracks SET year = {p1} WHERE id IN ({liste})"),
                    &[&a as &dyn ToSqlValue],
                )?;
            }
            (champ, ValeurDeMasse::Texte(v)) => {
                let colonne = match champ {
                    ChampDeMasse::Label => "label",
                    ChampDeMasse::Compositeur => "composer",
                    ChampDeMasse::ArtisteAlbum => "album_artist",
                    _ => return Err("champ et valeur incompatibles".into()),
                };
                tx.execute(
                    &format!("UPDATE tracks SET {colonne} = {p1} WHERE id IN ({liste})"),
                    &[v as &dyn ToSqlValue],
                )?;
            }
            _ => return Err("champ et valeur incompatibles".into()),
        }
    }
    Ok(())
}

fn ecrire_albums(
    tx: &dyn crate::db::backend::DbTxHandle,
    engine: Engine,
    c: &Cible,
    ids: &[i64],
    artiste_id: Option<i64>,
) -> Result<(), String> {
    let p1 = marque(engine, 1);
    let p2 = marque(engine, 2);
    for tranche in ids.chunks(TRANCHE) {
        let liste = liste_ids(tranche);
        match (c.champ, &c.valeur) {
            (ChampDeMasse::Genre, ValeurDeMasse::Texte(g)) => {
                let Some((_, json)) = &c.genre_pistes else {
                    return Err("genre sans valeur".into());
                };
                tx.execute(
                    &format!("UPDATE albums SET genre = {p1}, genres = {p2} WHERE id IN ({liste})"),
                    &[g as &dyn ToSqlValue, json as &dyn ToSqlValue],
                )?;
            }
            (ChampDeMasse::Annee, ValeurDeMasse::Annee(a)) => {
                let a = *a as i64;
                tx.execute(
                    &format!("UPDATE albums SET year = {p1} WHERE id IN ({liste})"),
                    &[&a as &dyn ToSqlValue],
                )?;
            }
            (ChampDeMasse::Label, ValeurDeMasse::Texte(v)) => {
                tx.execute(
                    &format!("UPDATE albums SET label = {p1} WHERE id IN ({liste})"),
                    &[v as &dyn ToSqlValue],
                )?;
            }
            (ChampDeMasse::ArtisteAlbum, _) => {
                let Some(aid) = artiste_id else {
                    return Err("artiste d'album sans artiste résolu".into());
                };
                tx.execute(
                    &format!("UPDATE albums SET artist_id = {p1} WHERE id IN ({liste})"),
                    &[&aid as &dyn ToSqlValue],
                )?;
            }
            (ChampDeMasse::Compositeur, _) => {}
            _ => return Err("champ et valeur incompatibles".into()),
        }
    }
    Ok(())
}

/// Les tenues (`track_metadata.champs_tenus`) des pistes changées, dans la
/// transaction : la ligne RELUE après l'écriture fournit la valeur tenue.
fn tenir_les_pistes(
    tx: &dyn crate::db::backend::DbTxHandle,
    engine: Engine,
    champ: ChampDeMasse,
    ids: &[i64],
) -> Result<(), String> {
    let upsert = match engine {
        Engine::Sqlite => crate::db::track_metadata_repo::sql::upsert(&SqliteDialect),
        Engine::Postgres => crate::db::track_metadata_repo::sql::upsert(&PostgresDialect),
    };
    let p1 = marque(engine, 1);
    let tenu = champ.champ_tenu();
    for tranche in ids.chunks(TRANCHE) {
        let liste = liste_ids(tranche);
        let mut existantes: HashMap<i64, ChampsTenus> = HashMap::new();
        for r in tx.query_many(
            &format!(
                "SELECT track_id, value FROM track_metadata WHERE key = {p1} AND track_id IN ({liste})"
            ),
            &[&champs_tenus::CLE as &dyn ToSqlValue],
        )? {
            if let (Some(id), Some(v)) = (
                r.first().and_then(|v| v.as_i64()),
                r.get(1).and_then(|v| v.as_string()),
            ) && let Ok(t) = serde_json::from_str::<ChampsTenus>(&v)
            {
                existantes.insert(id, t);
            }
        }
        let lignes = tx.query_many(
            &format!(
                "SELECT t.id, t.file_path, t.cue_media_path, t.cue_start_ms, t.genre, t.genres, \
                 t.year, t.label, t.composer, t.album_artist FROM tracks t WHERE t.id IN ({liste})"
            ),
            &[],
        )?;
        for r in lignes {
            let Some(id) = r.first().and_then(|v| v.as_i64()) else {
                continue;
            };
            let mut t = existantes.remove(&id).unwrap_or_default();
            t.chemin = r.get(1).and_then(|v| v.as_string());
            t.cue_media = r.get(2).and_then(|v| v.as_string());
            t.cue_debut_ms = r.get(3).and_then(|v| v.as_i64());
            match tenu {
                champs_tenus::Champ::Genre => {
                    t.genre = r.get(4).and_then(|v| v.as_string());
                    t.genres = r.get(5).and_then(|v| v.as_string());
                }
                champs_tenus::Champ::Annee => {
                    t.year = r.get(6).and_then(|v| v.as_i64()).map(|y| y as i32);
                }
                champs_tenus::Champ::Label => t.label = r.get(7).and_then(|v| v.as_string()),
                champs_tenus::Champ::Compositeur => {
                    t.composer = r.get(8).and_then(|v| v.as_string());
                }
                champs_tenus::Champ::ArtisteAlbum => {
                    t.album_artist = r.get(9).and_then(|v| v.as_string());
                }
                _ => {}
            }
            let json = serde_json::to_string(&t).map_err(|e| e.to_string())?;
            tx.execute(
                &upsert,
                &[
                    &id as &dyn ToSqlValue,
                    &champs_tenus::CLE as &dyn ToSqlValue,
                    &json as &dyn ToSqlValue,
                ],
            )?;
        }
    }
    Ok(())
}

/// `edition_manuelle` des albums changés, et, pour le genre, le marqueur de
/// recopie sur les pistes (#5314) : toutes leurs pistes portent ce genre.
fn marquer_les_albums(
    tx: &dyn crate::db::backend::DbTxHandle,
    engine: Engine,
    c: &Cible,
    ids: &[i64],
) -> Result<(), String> {
    let Some(nom) = c.champ.champ_album() else {
        return Ok(());
    };
    let upsert = match engine {
        Engine::Sqlite => crate::db::album_metadata_repo::sql::upsert(&SqliteDialect),
        Engine::Postgres => crate::db::album_metadata_repo::sql::upsert(&PostgresDialect),
    };
    let p1 = marque(engine, 1);
    let genre = c.texte();
    for tranche in ids.chunks(TRANCHE) {
        let liste = liste_ids(tranche);
        let mut tenus: BTreeMap<i64, Vec<String>> =
            tranche.iter().map(|id| (*id, Vec::new())).collect();
        for r in tx.query_many(
            &format!(
                "SELECT album_id, value FROM album_metadata WHERE key = {p1} AND album_id IN ({liste})"
            ),
            &[&CLE_EDITION_MANUELLE as &dyn ToSqlValue],
        )? {
            if let (Some(id), Some(v)) = (
                r.first().and_then(|v| v.as_i64()),
                r.get(1).and_then(|v| v.as_string()),
            ) && let Ok(l) = serde_json::from_str::<Vec<String>>(&v)
            {
                tenus.insert(id, l);
            }
        }
        for (id, mut champs) in tenus {
            if !champs.iter().any(|c| c == nom) {
                champs.push(nom.to_string());
            }
            champs.sort();
            let json = serde_json::to_string(&champs).map_err(|e| e.to_string())?;
            tx.execute(
                &upsert,
                &[
                    &id as &dyn ToSqlValue,
                    &CLE_EDITION_MANUELLE as &dyn ToSqlValue,
                    &json as &dyn ToSqlValue,
                ],
            )?;
            if c.champ == ChampDeMasse::Genre {
                tx.execute(
                    &upsert,
                    &[
                        &id as &dyn ToSqlValue,
                        &CLE_GENRE_PISTES as &dyn ToSqlValue,
                        &genre as &dyn ToSqlValue,
                    ],
                )?;
            }
        }
    }
    Ok(())
}

/// Le journal des gestes : les [`JOURNAL_MAX`] derniers, dans `settings`.
fn journaliser(
    tx: &dyn crate::db::backend::DbTxHandle,
    engine: Engine,
    c: &Cible,
    cible: CibleDuFiltre,
    plan: &PlanEdition,
) -> Result<(), String> {
    let p1 = marque(engine, 1);
    let p2 = marque(engine, 2);
    let mut journal: Vec<serde_json::Value> = tx
        .query_one(
            &format!("SELECT value FROM settings WHERE key = {p1}"),
            &[&CLE_JOURNAL as &dyn ToSqlValue],
        )?
        .and_then(|r| r.first().and_then(|v| v.as_string()))
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    let maintenant = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    journal.push(serde_json::json!({
        "at": maintenant,
        "field": c.champ.nom(),
        "value": c.texte(),
        "item_type": cible.item_type(),
        "tracks": plan.pistes_changees.len(),
        "albums": plan.albums_changes.len(),
        "unchanged": plan.pistes_deja,
        "remote_skipped": plan.pistes_distantes,
        "track_examples": plan.exemples_pistes,
    }));
    let debut = journal.len().saturating_sub(JOURNAL_MAX);
    let journal: Vec<_> = journal.into_iter().skip(debut).collect();
    let json = serde_json::to_string(&journal).map_err(|e| e.to_string())?;
    tx.execute(
        &format!(
            "INSERT INTO settings (key, value) VALUES ({p1}, {p2}) \
             ON CONFLICT (key) DO UPDATE SET value = excluded.value"
        ),
        &[&CLE_JOURNAL as &dyn ToSqlValue, &json as &dyn ToSqlValue],
    )?;
    Ok(())
}

#[cfg(test)]
#[path = "edition_par_filtre_tests.rs"]
mod tests;
