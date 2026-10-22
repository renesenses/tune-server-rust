//! Suggestions d'enrichissement sur UN album, depuis sa fiche.
//!
//! # Les champs
//!
//! | champ | où il s'écrit |
//! |---|---|
//! | `title`, `artist`, `genre`, `label` | colonnes de `albums`, par `AlbumRepo::update` — le chemin de l'édition manuelle (`PUT /library/albums/{id}`) |
//! | `subgenre`, `work`, `movement`, `orchestra` | magasin clé-valeur `album_metadata` |
//! | `composer`, `conductor`, `performer`, `lyricist`, `producer` | `album_metadata`, sous les clés du catalogue des champs (Crédits) que la fiche affiche déjà |
//!
//! # Ce qui n'est JAMAIS proposé en écrasement
//!
//! - un champ tenu À LA MAIN : le marqueur `album_metadata.edition_manuelle`
//!   (C3, `AlbumMetadataRepo::champs_edites_a_la_main`) le nomme ;
//! - une donnée identifiée par MusicBrainz : sur un album qui porte un
//!   `musicbrainz_release_id`, tout champ DÉJÀ rempli est tenu — l'assistant
//!   ne peut que remplir un vide ; l'artiste est tenu dès que l'artiste de
//!   l'album porte un `musicbrainz_id`.
//!
//! La garde joue DEUX fois : quand la suggestion est rendue (y compris depuis
//! le cache) et quand elle est appliquée — l'état a pu changer entre les deux.
//!
//! # Annulable
//!
//! Chaque application est une OPÉRATION du journal
//! (`album_metadata.assistant_ia_journal`), qui garde la valeur précédente de
//! chaque ligne. L'annuler restaure ces valeurs, sauf pour un champ modifié
//! depuis (la valeur n'est plus celle que l'assistant a posée) : celui-là est
//! rendu dans `refusees`, jamais écrasé.
//!
//! # « source : IA »
//!
//! Les champs acceptés sont nommés dans `album_metadata.source_ia` (tableau
//! JSON trié) — le magasin clé-valeur est le seul endroit du schéma qui
//! permette de qualifier un champ sans migration. Une annulation les retire.
//!
//! # Le genre vaut pour les pistes (étape 2)
//!
//! Comme l'édition manuelle de la fiche (#5314,
//! `tune_core::db::genre_album_pistes`) : un genre ACCEPTÉ est recopié sur les
//! pistes de l'album (`tracks.genre`, `tracks.genres`, `albums.genres` et le
//! marqueur `genre_pistes` que le scan relit), sauf compilation aux genres de
//! pistes différents. L'opération du journal garde, piste par piste, la
//! valeur d'avant et celle d'après : l'annulation rend chaque piste dont le
//! genre est encore celui que la recopie a posé, et laisse les autres
//! (`pistes_modifiees_depuis`).
//!
//! # Les fichiers audio
//!
//! Comme l'édition manuelle de la fiche : la BASE seulement. Écrire dans les
//! balises reste le geste séparé de `POST /library/albums/{id}/edition/write-tags`.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tune_core::db::album_metadata_repo::AlbumMetadataRepo;
use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::artist_repo::ArtistRepo;
use tune_core::db::backend::{DbBackend, ToSqlValue};
use tune_core::db::engine::Engine;
use tune_core::db::genre_album_pistes::{self, CLE_GENRE_PISTES, Recopie};
use tune_core::db::models::Album;
use tune_core::db::track_repo::TrackRepo;

/// La clé du journal des opérations, dans `album_metadata`.
pub const CLE_JOURNAL: &str = "assistant_ia_journal";
/// La clé des champs posés par l'assistant, dans `album_metadata`.
pub const CLE_SOURCE_IA: &str = "source_ia";
/// Opérations gardées par album.
pub const OPERATIONS_MAX: usize = 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Lieu {
    Colonne,
    Cle,
}

/// Les champs que l'assistant peut proposer, et où ils s'écrivent.
const CHAMPS: &[(&str, Lieu)] = &[
    ("title", Lieu::Colonne),
    ("artist", Lieu::Colonne),
    ("genre", Lieu::Colonne),
    ("label", Lieu::Colonne),
    ("subgenre", Lieu::Cle),
    ("work", Lieu::Cle),
    ("movement", Lieu::Cle),
    ("composer", Lieu::Cle),
    ("conductor", Lieu::Cle),
    ("orchestra", Lieu::Cle),
    ("performer", Lieu::Cle),
    ("lyricist", Lieu::Cle),
    ("producer", Lieu::Cle),
];

pub fn champs() -> Vec<&'static str> {
    CHAMPS.iter().map(|(c, _)| *c).collect()
}

fn lieu(champ: &str) -> Option<Lieu> {
    CHAMPS.iter().find(|(c, _)| *c == champ).map(|(_, l)| *l)
}

/// Ce que la base dit de l'album, au moment de la lecture.
pub struct EtatAlbum {
    pub album: Album,
    pub artiste_mbid: bool,
    pub meta: HashMap<String, String>,
    pub tenus: Vec<String>,
    pub pistes: Vec<Value>,
}

fn non_vide(s: Option<&str>) -> Option<String> {
    s.map(str::trim).filter(|s| !s.is_empty()).map(String::from)
}

impl EtatAlbum {
    pub fn charger(db: &Arc<dyn DbBackend>, album_id: i64) -> Result<Option<Self>, String> {
        let Some(album) = AlbumRepo::with_backend(db.clone())
            .get(album_id)
            .map_err(|e| e.to_string())?
        else {
            return Ok(None);
        };
        let artiste_mbid = match album.artist_id {
            Some(aid) => ArtistRepo::with_backend(db.clone())
                .get(aid)
                .map_err(|e| e.to_string())?
                .and_then(|a| non_vide(a.musicbrainz_id.as_deref()))
                .is_some(),
            None => false,
        };
        let meta_repo = AlbumMetadataRepo::with_backend(db.clone());
        let meta = meta_repo.get_all(album_id)?;
        let tenus = meta_repo.champs_edites_a_la_main(album_id)?;
        let pistes = TrackRepo::with_backend(db.clone())
            .list_by_album(album_id)
            .map_err(|e| e.to_string())?
            .into_iter()
            .take(80)
            .map(|t| {
                json!({
                    "disc": t.disc_number,
                    "n": t.track_number,
                    "title": t.title,
                    "artist": t.artist_name,
                    "composer": t.composer,
                })
            })
            .collect();
        Ok(Some(Self {
            album,
            artiste_mbid,
            meta,
            tenus,
            pistes,
        }))
    }

    pub fn valeur(&self, champ: &str) -> Option<String> {
        match champ {
            "title" => non_vide(Some(&self.album.title)),
            "artist" => non_vide(self.album.artist_name.as_deref()),
            "genre" => non_vide(self.album.genre.as_deref()),
            "label" => non_vide(self.album.label.as_deref()),
            autre => non_vide(self.meta.get(autre).map(String::as_str)),
        }
    }

    fn identifie_par_musicbrainz(&self) -> bool {
        non_vide(self.album.musicbrainz_release_id.as_deref()).is_some()
    }

    /// Pourquoi ce champ ne peut pas recevoir `nouvelle`, ou `None`.
    pub fn protection(&self, champ: &str) -> Option<&'static str> {
        lieu(champ)?;
        if self.tenus.iter().any(|t| t == champ) {
            return Some("edition_manuelle");
        }
        let rempli = self.valeur(champ).is_some();
        if champ == "artist" && self.artiste_mbid {
            return Some("musicbrainz");
        }
        if rempli && self.identifie_par_musicbrainz() {
            return Some("musicbrainz");
        }
        None
    }

    /// Le genre ou les crédits disent-ils « classique » ?
    pub fn semble_classique(&self) -> bool {
        let g = format!(
            "{} {}",
            self.album.genre.as_deref().unwrap_or(""),
            self.album.genres.as_deref().unwrap_or("")
        )
        .to_lowercase();
        g.contains("classi") || g.contains("opera") || g.contains("baroque")
    }

    /// L'album tel que le modèle le lit.
    pub fn contexte(&self) -> Value {
        let mut valeurs = serde_json::Map::new();
        for c in champs() {
            valeurs.insert(c.into(), json!(self.valeur(c)));
        }
        json!({
            "fields": valeurs,
            "year": self.album.year,
            "is_compilation": self.album.is_compilation,
            "tracks": self.pistes,
        })
    }
}

/// La consigne des suggestions.
pub fn consigne(langue: &str) -> String {
    format!(
        "You help clean up the metadata of ONE album in a music library. \
You receive its current fields and tracks. Reply with ONE JSON object and nothing else:\n\
{{\"suggestions\": [{{\"field\": ..., \"value\": string, \"confidence\": number between 0 and 1, \"reason\": short string in {langue}}}]}}\n\
Allowed fields: {}.\n\
- genre: the main genre; subgenre: a more precise style.\n\
- For classical music: work, movement, composer, performer (soloists), conductor, orchestra.\n\
- Missing credits: composer, conductor, performer, lyricist, producer — only when you are confident.\n\
- Name normalisation (title, artist, label): fix case, spelling and the usual form of a name; never translate a title.\n\
Only suggest a change when it improves the field. Never invent facts: when unsure, leave the field out \
or give a low confidence. An empty list is a valid answer.",
        champs().join(", ")
    )
}

/// Une suggestion validée, avec l'écart avant/après.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Suggestion {
    pub champ: String,
    pub avant: Option<String>,
    pub apres: String,
    pub confiance: f64,
    pub raison: Option<String>,
}

/// Une suggestion écartée par la garde.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Ecartee {
    pub champ: String,
    pub valeur: Option<String>,
    pub raison: &'static str,
    /// Étape 2 : la piste visée, pour une ligne de piste.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub piste_id: Option<i64>,
}

/// L'artiste d'un album se désigne par son NOM, comparé sans la casse
/// (`ArtistRepo::get_by_name`) : changer seulement la casse retomberait sur
/// le même artiste et ne changerait rien. C'est un renommage de l'ARTISTE, qui
/// vaut pour tous ses albums — un geste de la fiche artiste, pas d'ici.
fn casse_d_artiste(champ: &str, avant: Option<&str>, apres: &str) -> bool {
    champ == "artist" && avant.is_some_and(|a| a.to_lowercase() == apres.to_lowercase())
}

fn valeur_propre(v: Option<&Value>) -> Option<String> {
    v.and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty() && s.chars().count() <= 300)
        .map(String::from)
}

/// Valide la réponse du modèle contre l'état de l'album.
pub fn evaluer(
    brut: &Value,
    etat: &EtatAlbum,
) -> Result<(Vec<Suggestion>, Vec<Ecartee>), &'static str> {
    let liste = brut
        .get("suggestions")
        .and_then(Value::as_array)
        .ok_or("reponse_invalide")?;
    let mut retenues: Vec<Suggestion> = Vec::new();
    let mut ecartees = Vec::new();
    for s in liste {
        let champ = s.get("field").and_then(Value::as_str).unwrap_or("").trim();
        let valeur = valeur_propre(s.get("value"));
        let mut ecarter = |raison| {
            ecartees.push(Ecartee {
                champ: champ.to_string(),
                valeur: valeur.clone(),
                raison,
                piste_id: None,
            })
        };
        if lieu(champ).is_none() {
            ecarter("champ_inconnu");
            continue;
        }
        let Some(apres) = valeur.clone() else {
            ecarter("valeur_invalide");
            continue;
        };
        let Some(confiance) = s
            .get("confidence")
            .and_then(Value::as_f64)
            .filter(|c| c.is_finite())
        else {
            ecarter("valeur_invalide");
            continue;
        };
        let avant = etat.valeur(champ);
        if avant.as_deref() == Some(apres.as_str()) {
            ecarter("identique");
            continue;
        }
        if let Some(raison) = etat.protection(champ) {
            ecarter(raison);
            continue;
        }
        if casse_d_artiste(champ, avant.as_deref(), &apres) {
            ecarter("renommer_l_artiste");
            continue;
        }
        let sugg = Suggestion {
            champ: champ.to_string(),
            avant,
            apres,
            confiance: confiance.clamp(0.0, 1.0),
            raison: s
                .get("reason")
                .and_then(Value::as_str)
                .map(|r| r.trim().chars().take(300).collect()),
        };
        // Un champ proposé deux fois : la plus confiante gagne.
        match retenues.iter_mut().find(|r| r.champ == sugg.champ) {
            Some(r) if r.confiance < sugg.confiance => *r = sugg,
            Some(_) => {}
            None => retenues.push(sugg),
        }
    }
    Ok((retenues, ecartees))
}

// ---------------------------------------------------------------------------
// Appliquer et annuler
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LigneJournal {
    pub champ: String,
    pub avant: Option<String>,
    pub apres: String,
    /// Étape 2 : la ligne porte sur une PISTE (œuvre, mouvement), pas sur
    /// l'album. Absent des opérations de l'étape 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub piste_id: Option<i64>,
}

/// Le genre d'une piste avant et après la recopie.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GenrePiste {
    pub id: i64,
    pub genre_avant: Option<String>,
    pub genres_avant: Option<String>,
    pub genre_apres: Option<String>,
    pub genres_apres: Option<String>,
}

/// Ce que la recopie du genre sur les pistes a changé : de quoi la défaire.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PropagationGenre {
    pub album_genres_avant: Option<String>,
    pub album_genres_apres: Option<String>,
    /// Le marqueur `genre_pistes` d'avant (`None` : absent).
    pub marqueur_avant: Option<String>,
    /// Les seules pistes que la recopie a changées.
    pub pistes: Vec<GenrePiste>,
    #[serde(default)]
    pub compilation_epargnee: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Operation {
    pub id: String,
    pub horodatage: String,
    pub lignes: Vec<LigneJournal>,
    #[serde(default)]
    pub annulee: bool,
    /// Étape 2 : la recopie du genre accepté sur les pistes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub genre_pistes: Option<PropagationGenre>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct LigneAAppliquer {
    #[serde(alias = "field")]
    pub champ: String,
    #[serde(alias = "value")]
    pub valeur: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Rapport {
    pub appliquees: Vec<LigneJournal>,
    pub refusees: Vec<Ecartee>,
    pub operation_id: Option<String>,
    /// Étape 2 : pistes dont le genre a été recopié (application) ou rendu
    /// (annulation).
    #[serde(default)]
    pub pistes_genre: usize,
    /// Compilation aux genres de pistes différents : pistes épargnées.
    #[serde(default)]
    pub compilation_epargnee: bool,
    /// Annulation : pistes dont le genre a changé depuis la recopie,
    /// laissées telles quelles.
    #[serde(default)]
    pub pistes_modifiees_depuis: usize,
}

pub fn journal(db: &Arc<dyn DbBackend>, album_id: i64) -> Result<Vec<Operation>, String> {
    let meta = AlbumMetadataRepo::with_backend(db.clone()).get_all(album_id)?;
    Ok(meta
        .get(CLE_JOURNAL)
        .and_then(|v| serde_json::from_str(v).ok())
        .unwrap_or_default())
}

pub(crate) fn ranger_journal(
    db: &Arc<dyn DbBackend>,
    album_id: i64,
    ops: &[Operation],
) -> Result<(), String> {
    let debut = ops.len().saturating_sub(OPERATIONS_MAX);
    let json = serde_json::to_string(&ops[debut..]).map_err(|e| e.to_string())?;
    AlbumMetadataRepo::with_backend(db.clone()).set(album_id, CLE_JOURNAL, &json)
}

fn champs_source_ia(db: &Arc<dyn DbBackend>, album_id: i64) -> Result<BTreeSet<String>, String> {
    let meta = AlbumMetadataRepo::with_backend(db.clone()).get_all(album_id)?;
    Ok(meta
        .get(CLE_SOURCE_IA)
        .and_then(|v| serde_json::from_str(v).ok())
        .unwrap_or_default())
}

fn ranger_source_ia(
    db: &Arc<dyn DbBackend>,
    album_id: i64,
    champs: &BTreeSet<String>,
) -> Result<(), String> {
    let repo = AlbumMetadataRepo::with_backend(db.clone());
    if champs.is_empty() {
        repo.delete(album_id, CLE_SOURCE_IA)
    } else {
        repo.set(
            album_id,
            CLE_SOURCE_IA,
            &serde_json::to_string(champs).map_err(|e| e.to_string())?,
        )
    }
}

/// Écrit UNE valeur (ou l'efface, `None`) — même chemin que l'édition
/// manuelle pour les colonnes, sans poser le marqueur d'édition manuelle :
/// l'utilisateur a ACCEPTÉ une valeur, il ne l'a pas saisie.
fn ecrire(
    db: &Arc<dyn DbBackend>,
    album_id: i64,
    champ: &str,
    valeur: Option<&str>,
) -> Result<(), String> {
    match lieu(champ) {
        Some(Lieu::Cle) => {
            let repo = AlbumMetadataRepo::with_backend(db.clone());
            match valeur {
                Some(v) => repo.set(album_id, champ, v),
                None => repo.delete(album_id, champ),
            }
        }
        Some(Lieu::Colonne) => {
            let repo = AlbumRepo::with_backend(db.clone());
            let mut album = repo
                .get(album_id)
                .map_err(|e| e.to_string())?
                .ok_or("album introuvable")?;
            match champ {
                "title" => album.title = valeur.unwrap_or("").to_string(),
                "genre" => album.genre = valeur.map(String::from),
                "label" => album.label = valeur.map(String::from),
                "artist" => {
                    let artistes = ArtistRepo::with_backend(db.clone());
                    album.artist_id = match valeur {
                        Some(nom) => match artistes.get_by_name(nom).map_err(|e| e.to_string())? {
                            Some(a) => a.id,
                            None => {
                                artistes
                                    .get_or_create(nom, None, None)
                                    .map_err(|e| e.to_string())?
                                    .id
                            }
                        },
                        None => None,
                    };
                }
                _ => return Err(format!("champ inconnu : {champ}")),
            }
            repo.update(&album).map_err(|e| e.to_string())
        }
        None => Err(format!("champ inconnu : {champ}")),
    }
}

/// La marque de paramètre n° `n` du moteur de la base.
pub(crate) fn marque(db: &Arc<dyn DbBackend>, n: usize) -> String {
    match db.engine() {
        Engine::Sqlite => "?".into(),
        Engine::Postgres => format!("${n}"),
    }
}

type GenresDePiste = (Option<String>, Option<String>);

fn genres_des_pistes(
    db: &Arc<dyn DbBackend>,
    album_id: i64,
) -> Result<Vec<(i64, GenresDePiste)>, String> {
    let lignes = db.query_many(
        &format!(
            "SELECT id, genre, genres FROM tracks WHERE album_id = {} ORDER BY id",
            marque(db, 1)
        ),
        &[&album_id as &dyn ToSqlValue],
    )?;
    Ok(lignes
        .iter()
        .filter_map(|l| {
            let id = l.first().and_then(|v| v.as_i64())?;
            Some((
                id,
                (
                    l.get(1).and_then(|v| v.as_string()),
                    l.get(2).and_then(|v| v.as_string()),
                ),
            ))
        })
        .collect())
}

fn genres_de_l_album(db: &Arc<dyn DbBackend>, album_id: i64) -> Result<Option<String>, String> {
    Ok(db
        .query_one(
            &format!("SELECT genres FROM albums WHERE id = {}", marque(db, 1)),
            &[&album_id as &dyn ToSqlValue],
        )?
        .and_then(|l| l.first().and_then(|v| v.as_string())))
}

/// Recopie le genre (déjà écrit) de l'album sur ses pistes, comme
/// l'édition manuelle (#5314), et rend de quoi la défaire.
fn propager_genre(db: &Arc<dyn DbBackend>, album_id: i64) -> Result<PropagationGenre, String> {
    let avant: HashMap<i64, GenresDePiste> = genres_des_pistes(db, album_id)?.into_iter().collect();
    let album_genres_avant = genres_de_l_album(db, album_id)?;
    let marqueur_avant = AlbumMetadataRepo::with_backend(db.clone())
        .get_all(album_id)?
        .get(CLE_GENRE_PISTES)
        .cloned();
    let recopie = genre_album_pistes::recopier(db, album_id, false)?;
    let pistes = genres_des_pistes(db, album_id)?
        .into_iter()
        .filter_map(|(id, apres)| {
            let av = avant.get(&id).cloned().unwrap_or_default();
            (av != apres).then_some(GenrePiste {
                id,
                genre_avant: av.0,
                genres_avant: av.1,
                genre_apres: apres.0,
                genres_apres: apres.1,
            })
        })
        .collect();
    Ok(PropagationGenre {
        album_genres_avant,
        album_genres_apres: genres_de_l_album(db, album_id)?,
        marqueur_avant,
        pistes,
        compilation_epargnee: recopie == Recopie::CompilationEpargnee,
    })
}

/// Défait une recopie : chaque piste dont le genre est ENCORE celui que la
/// recopie a posé retrouve le sien ; les autres sont comptées. Rend
/// `(rendues, modifiées depuis)`.
fn defaire_propagation(
    db: &Arc<dyn DbBackend>,
    album_id: i64,
    p: &PropagationGenre,
) -> Result<(usize, usize), String> {
    let present: HashMap<i64, GenresDePiste> =
        genres_des_pistes(db, album_id)?.into_iter().collect();
    let (mut rendues, mut modifiees) = (0, 0);
    let sql = format!(
        "UPDATE tracks SET genre = {}, genres = {} WHERE id = {}",
        marque(db, 1),
        marque(db, 2),
        marque(db, 3)
    );
    for gp in &p.pistes {
        let apres = (gp.genre_apres.clone(), gp.genres_apres.clone());
        if present.get(&gp.id) != Some(&apres) {
            modifiees += 1;
            continue;
        }
        db.execute(
            &sql,
            &[
                &gp.genre_avant as &dyn ToSqlValue,
                &gp.genres_avant as &dyn ToSqlValue,
                &gp.id as &dyn ToSqlValue,
            ],
        )?;
        rendues += 1;
    }
    if genres_de_l_album(db, album_id)? == p.album_genres_apres {
        db.execute(
            &format!(
                "UPDATE albums SET genres = {} WHERE id = {}",
                marque(db, 1),
                marque(db, 2)
            ),
            &[
                &p.album_genres_avant as &dyn ToSqlValue,
                &album_id as &dyn ToSqlValue,
            ],
        )?;
    }
    let meta = AlbumMetadataRepo::with_backend(db.clone());
    match &p.marqueur_avant {
        Some(v) => meta.set(album_id, CLE_GENRE_PISTES, v)?,
        None => meta.delete(album_id, CLE_GENRE_PISTES)?,
    }
    Ok((rendues, modifiees))
}

pub(crate) fn identifiant_d_operation() -> String {
    let mut o = [0u8; 8];
    let _ = getrandom::getrandom(&mut o);
    o.iter().map(|b| format!("{b:02x}")).collect()
}

/// Applique les lignes ACCEPTÉES. La garde est rejouée sur l'état présent.
pub fn appliquer(
    db: &Arc<dyn DbBackend>,
    album_id: i64,
    lignes: &[LigneAAppliquer],
) -> Result<Option<Rapport>, String> {
    let Some(etat) = EtatAlbum::charger(db, album_id)? else {
        return Ok(None);
    };
    let mut rapport = Rapport::default();
    let mut vus = BTreeSet::new();
    for l in lignes {
        let champ = l.champ.trim();
        let valeur = l.valeur.trim();
        let mut refuser = |raison| {
            rapport.refusees.push(Ecartee {
                champ: champ.to_string(),
                valeur: Some(valeur.to_string()),
                raison,
                piste_id: None,
            })
        };
        if lieu(champ).is_none() {
            refuser("champ_inconnu");
            continue;
        }
        if valeur.is_empty() || valeur.chars().count() > 300 {
            refuser("valeur_invalide");
            continue;
        }
        if !vus.insert(champ.to_string()) {
            refuser("doublon");
            continue;
        }
        if let Some(raison) = etat.protection(champ) {
            refuser(raison);
            continue;
        }
        let avant = etat.valeur(champ);
        if avant.as_deref() == Some(valeur) {
            refuser("identique");
            continue;
        }
        if casse_d_artiste(champ, avant.as_deref(), valeur) {
            refuser("renommer_l_artiste");
            continue;
        }
        ecrire(db, album_id, champ, Some(valeur))?;
        rapport.appliquees.push(LigneJournal {
            champ: champ.to_string(),
            avant,
            apres: valeur.to_string(),
            piste_id: None,
        });
    }
    // Étape 2 : le genre accepté vaut pour les pistes (#5314).
    let genre_pistes = if rapport.appliquees.iter().any(|l| l.champ == "genre") {
        let p = propager_genre(db, album_id)?;
        rapport.pistes_genre = p.pistes.len();
        rapport.compilation_epargnee = p.compilation_epargnee;
        Some(p)
    } else {
        None
    };
    if !rapport.appliquees.is_empty() {
        let id = identifiant_d_operation();
        let mut ops = journal(db, album_id)?;
        ops.push(Operation {
            id: id.clone(),
            horodatage: chrono::Utc::now().to_rfc3339(),
            lignes: rapport.appliquees.clone(),
            annulee: false,
            genre_pistes,
        });
        ranger_journal(db, album_id, &ops)?;
        let mut source = champs_source_ia(db, album_id)?;
        source.extend(rapport.appliquees.iter().map(|l| l.champ.clone()));
        ranger_source_ia(db, album_id, &source)?;
        rapport.operation_id = Some(id);
    }
    Ok(Some(rapport))
}

/// Annule une opération (la dernière non annulée si `operation_id` est
/// absent).
pub fn annuler(
    db: &Arc<dyn DbBackend>,
    album_id: i64,
    operation_id: Option<&str>,
) -> Result<Option<Rapport>, String> {
    let Some(etat) = EtatAlbum::charger(db, album_id)? else {
        return Ok(None);
    };
    let mut ops = journal(db, album_id)?;
    let indice = match operation_id {
        Some(id) => ops.iter().position(|o| o.id == id && !o.annulee),
        None => ops.iter().rposition(|o| !o.annulee),
    };
    let Some(indice) = indice else {
        return Ok(Some(Rapport::default()));
    };
    let mut rapport = Rapport {
        operation_id: Some(ops[indice].id.clone()),
        ..Default::default()
    };
    let mut source = champs_source_ia(db, album_id)?;
    for l in ops[indice].lignes.clone() {
        if let Some(piste_id) = l.piste_id {
            // Une ligne de PISTE (étape 2) : même règle, sur la piste.
            match crate::pistes::defaire_ligne(db, piste_id, &l)? {
                Ok(()) => rapport.appliquees.push(LigneJournal {
                    champ: l.champ.clone(),
                    avant: Some(l.apres.clone()),
                    apres: l.avant.clone().unwrap_or_default(),
                    piste_id: Some(piste_id),
                }),
                Err(present) => rapport.refusees.push(Ecartee {
                    champ: l.champ.clone(),
                    valeur: present,
                    raison: "modifie_depuis",
                    piste_id: None,
                }),
            }
            continue;
        }
        // Modifié depuis : la valeur n'est plus celle que l'assistant a posée.
        if etat.valeur(&l.champ).as_deref() != Some(l.apres.as_str()) {
            rapport.refusees.push(Ecartee {
                champ: l.champ.clone(),
                valeur: etat.valeur(&l.champ),
                raison: "modifie_depuis",
                piste_id: None,
            });
            continue;
        }
        ecrire(db, album_id, &l.champ, l.avant.as_deref())?;
        source.remove(&l.champ);
        rapport.appliquees.push(LigneJournal {
            champ: l.champ.clone(),
            avant: Some(l.apres.clone()),
            apres: l.avant.clone().unwrap_or_default(),
            piste_id: None,
        });
    }
    // Le genre des pistes ne se rend que si celui de l'album l'a été : un
    // genre d'album retouché depuis tient ses pistes avec lui.
    if let Some(p) = ops[indice].genre_pistes.clone()
        && rapport
            .appliquees
            .iter()
            .any(|l| l.champ == "genre" && l.piste_id.is_none())
    {
        let (rendues, modifiees) = defaire_propagation(db, album_id, &p)?;
        rapport.pistes_genre = rendues;
        rapport.pistes_modifiees_depuis = modifiees;
    }
    ops[indice].annulee = true;
    ranger_journal(db, album_id, &ops)?;
    ranger_source_ia(db, album_id, &source)?;
    Ok(Some(rapport))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use tune_core::db::sqlite::SqliteDb;

    pub(crate) fn base() -> Arc<dyn DbBackend> {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        tune_core::db::migrations::run_migrations(&db).unwrap();
        db.execute_batch(
            "INSERT INTO artists (id, name, musicbrainz_id) VALUES (1, 'Karajan', NULL);
             INSERT INTO artists (id, name, musicbrainz_id) VALUES (2, 'Miles Davis', 'mb-artist-miles');
             INSERT INTO albums (id, title, artist_id, genre, label) VALUES (1, 'beethoven 9', 1, 'Classical', NULL);
             INSERT INTO albums (id, title, artist_id, genre, label, musicbrainz_release_id)
                    VALUES (2, 'Kind of Blue', 2, 'Jazz', NULL, 'mb-release-kob');
             INSERT INTO tracks (id, title, album_id, artist_id, file_path) VALUES (1, 'Allegro', 1, 1, '/m/1.flac');",
        )
        .unwrap();
        Arc::new(db)
    }

    fn sugg(champ: &str, valeur: &str) -> Value {
        json!({"field": champ, "value": valeur, "confidence": 0.9, "reason": "r"})
    }

    #[test]
    fn une_donnee_musicbrainz_n_est_jamais_proposee_en_ecrasement() {
        let db = base();
        let etat = EtatAlbum::charger(&db, 2).unwrap().unwrap();
        let brut = json!({"suggestions": [
            sugg("title", "Kind Of Blue (Remastered)"),
            sugg("genre", "Modal Jazz"),
            sugg("artist", "Miles Davis Quintet"),
            // Vide : remplir est permis, même sur un album identifié.
            sugg("label", "Columbia"),
        ]});
        let (retenues, ecartees) = evaluer(&brut, &etat).unwrap();
        assert_eq!(
            retenues
                .iter()
                .map(|s| s.champ.as_str())
                .collect::<Vec<_>>(),
            vec!["label"]
        );
        let raisons: Vec<(&str, &str)> = ecartees
            .iter()
            .map(|e| (e.champ.as_str(), e.raison))
            .collect();
        assert_eq!(
            raisons,
            vec![
                ("title", "musicbrainz"),
                ("genre", "musicbrainz"),
                ("artist", "musicbrainz")
            ]
        );
        // Et la même garde à l'application, même si l'écran l'envoie.
        let r = appliquer(
            &db,
            2,
            &[LigneAAppliquer {
                champ: "genre".into(),
                valeur: "Modal Jazz".into(),
            }],
        )
        .unwrap()
        .unwrap();
        assert!(r.appliquees.is_empty());
        assert_eq!(r.refusees[0].raison, "musicbrainz");
        let album = AlbumRepo::with_backend(db.clone()).get(2).unwrap().unwrap();
        assert_eq!(album.genre.as_deref(), Some("Jazz"));
    }

    #[test]
    fn un_champ_edite_a_la_main_n_est_jamais_propose() {
        let db = base();
        AlbumMetadataRepo::with_backend(db.clone())
            .marquer_edition_manuelle(1, &["genre"])
            .unwrap();
        let etat = EtatAlbum::charger(&db, 1).unwrap().unwrap();
        let (retenues, ecartees) = evaluer(
            &json!({"suggestions": [sugg("genre", "Symphony"), sugg("composer", "Ludwig van Beethoven")]}),
            &etat,
        )
        .unwrap();
        assert_eq!(retenues.len(), 1);
        assert_eq!(retenues[0].champ, "composer");
        assert_eq!(ecartees[0].raison, "edition_manuelle");
    }

    #[test]
    fn appliquer_puis_annuler_rend_la_valeur_precedente() {
        let db = base();
        let r = appliquer(
            &db,
            1,
            &[
                LigneAAppliquer {
                    champ: "title".into(),
                    valeur: "Symphonie n° 9".into(),
                },
                LigneAAppliquer {
                    champ: "artist".into(),
                    valeur: "Herbert von Karajan".into(),
                },
                LigneAAppliquer {
                    champ: "conductor".into(),
                    valeur: "Herbert von Karajan".into(),
                },
                LigneAAppliquer {
                    champ: "mood".into(),
                    valeur: "épique".into(),
                },
            ],
        )
        .unwrap()
        .unwrap();
        assert_eq!(r.appliquees.len(), 3);
        assert_eq!(r.refusees[0].raison, "champ_inconnu");
        let etat = EtatAlbum::charger(&db, 1).unwrap().unwrap();
        assert_eq!(etat.valeur("title").as_deref(), Some("Symphonie n° 9"));
        assert_eq!(
            etat.valeur("artist").as_deref(),
            Some("Herbert von Karajan")
        );
        assert_eq!(
            etat.valeur("conductor").as_deref(),
            Some("Herbert von Karajan")
        );
        // « source : IA »
        assert_eq!(
            champs_source_ia(&db, 1)
                .unwrap()
                .into_iter()
                .collect::<Vec<_>>(),
            vec!["artist", "conductor", "title"]
        );
        // Pas d'édition manuelle posée : l'utilisateur a accepté, pas saisi.
        assert!(etat.tenus.is_empty());

        let a = annuler(&db, 1, None).unwrap().unwrap();
        assert_eq!(a.appliquees.len(), 3);
        let etat = EtatAlbum::charger(&db, 1).unwrap().unwrap();
        assert_eq!(etat.valeur("title").as_deref(), Some("beethoven 9"));
        assert_eq!(etat.valeur("artist").as_deref(), Some("Karajan"));
        assert_eq!(etat.valeur("conductor"), None);
        assert!(champs_source_ia(&db, 1).unwrap().is_empty());
        assert!(journal(&db, 1).unwrap()[0].annulee);
    }

    fn genre_de_piste(db: &Arc<dyn DbBackend>, id: i64) -> (Option<String>, Option<String>) {
        let l = db
            .query_one(
                "SELECT genre, genres FROM tracks WHERE id = ?",
                &[&id as &dyn ToSqlValue],
            )
            .unwrap()
            .unwrap();
        (
            l.first().and_then(|v| v.as_string()),
            l.get(1).and_then(|v| v.as_string()),
        )
    }

    #[test]
    fn le_genre_accepte_vaut_pour_les_pistes_et_son_annulation_les_rend() {
        let db = base();
        db.execute_batch(
            "UPDATE tracks SET genre = 'Classique', genres = '[\"Classique\"]' WHERE id = 1;
             INSERT INTO tracks (id, title, album_id, artist_id, file_path, genre)
                    VALUES (2, 'Scherzo', 1, 1, '/m/2.flac', NULL);",
        )
        .unwrap();
        let r = appliquer(
            &db,
            1,
            &[LigneAAppliquer {
                champ: "genre".into(),
                valeur: "Symphonic".into(),
            }],
        )
        .unwrap()
        .unwrap();
        assert_eq!(r.pistes_genre, 2, "{r:?}");
        assert_eq!(genre_de_piste(&db, 1).0.as_deref(), Some("Symphonic"));
        assert_eq!(genre_de_piste(&db, 2).0.as_deref(), Some("Symphonic"));
        let op = &journal(&db, 1).unwrap()[0];
        assert_eq!(op.genre_pistes.as_ref().unwrap().pistes.len(), 2);

        // L'utilisateur retouche une piste entre-temps : elle reste à lui.
        db.execute("UPDATE tracks SET genre = 'Opéra' WHERE id = 2", &[])
            .unwrap();
        let a = annuler(&db, 1, None).unwrap().unwrap();
        assert_eq!(a.pistes_genre, 1);
        assert_eq!(a.pistes_modifiees_depuis, 1);
        assert_eq!(
            genre_de_piste(&db, 1),
            (Some("Classique".into()), Some("[\"Classique\"]".into()))
        );
        assert_eq!(genre_de_piste(&db, 2).0.as_deref(), Some("Opéra"));
        // Le marqueur que le scan relit est retiré : il n'existait pas avant.
        assert!(
            !AlbumMetadataRepo::with_backend(db.clone())
                .get_all(1)
                .unwrap()
                .contains_key(CLE_GENRE_PISTES)
        );
    }

    #[test]
    fn une_valeur_modifiee_depuis_n_est_pas_ecrasee_par_l_annulation() {
        let db = base();
        appliquer(
            &db,
            1,
            &[LigneAAppliquer {
                champ: "genre".into(),
                valeur: "Symphonic".into(),
            }],
        )
        .unwrap();
        // L'utilisateur repasse derrière.
        let repo = AlbumRepo::with_backend(db.clone());
        let mut album = repo.get(1).unwrap().unwrap();
        album.genre = Some("Orchestral".into());
        repo.update(&album).unwrap();
        let a = annuler(&db, 1, None).unwrap().unwrap();
        assert!(a.appliquees.is_empty());
        assert_eq!(a.refusees[0].raison, "modifie_depuis");
        assert_eq!(
            repo.get(1).unwrap().unwrap().genre.as_deref(),
            Some("Orchestral")
        );
    }
}
