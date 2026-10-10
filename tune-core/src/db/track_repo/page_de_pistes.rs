//! La liste des pistes PAGINÉE CÔTÉ SERVEUR — onglet Titres de la
//! Bibliothèque (tune-web-client#1716).
//!
//! # Pourquoi
//!
//! L'onglet Titres chargeait la bibliothèque ENTIÈRE (37 700 pistes sur la
//! base de mesure, 30 Mo) avant d'afficher la première ligne, puis filtrait,
//! comptait et coupait à 500 lignes dans le navigateur. Après web#2001, il
//! restait 1,6 s de travail du navigateur à chaque affichage. Ici, le serveur
//! rend UNE page déjà triée, déjà filtrée, et les comptes par source qui
//! habillent les puces : le navigateur n'a plus que la fenêtre visible.
//!
//! # Ce que la page reprend de l'écran, mot pour mot
//!
//! - la **recherche** de l'onglet : sous-chaîne du TITRE ou de l'ARTISTE,
//!   insensible à la casse et aux accents (`fold` côté client). Ce n'est pas
//!   le texte libre d'Oxygen (`q`, qui compare aussi l'album, le label et le
//!   chemin) : l'onglet n'a jamais cherché là, et la même saisie doit rendre
//!   la même liste qu'avant ;
//! - la **provenance** : `local`, un service (`qobuz`…), `upnp` (tous les
//!   serveurs UPnP) ou `upnp:<UDN>` (un serveur), la clé de
//!   `provenanceBibliotheque.provenanceDe` ;
//! - les **comptes par provenance** sous la recherche, l'agrégat `upnp`
//!   compris (`compterSources`) ;
//! - le **tri** par les colonnes du tableau (`colonnesPistes.ts`).
//!
//! Les facettes d'Oxygen (`format`, `folder`, `genre`…) s'appliquent aussi :
//! le `WHERE` est celui de `list_filtered`, partagé
//! ([`TrackRepo::conditions_du_filtre`]), socle de la vue compris.

use super::{Placeholders, SqlValue, ToSqlValue, Track, TrackFilter, TrackRepo, sql};
use crate::TuneError;
use crate::db::engine::Engine;

/// Une colonne du tableau des pistes par laquelle le serveur sait trier.
///
/// La CLÉ est celle du client (`CleColonne` de `colonnesPistes.ts`) : le
/// navigateur envoie ce qu'il affiche, sans table de traduction à tenir.
///
/// Absentes, et volontairement : `plays` et `lastPlayed` (une jointure par
/// titre et artiste sur l'historique, `HistoryRepo::plays_for_tracks`), `dr`
/// (magasin ouvert `track_metadata`), `quality` (deux colonnes, aucun ordre
/// évident) et `num` (le RANG dans la liste, sur cet onglet). Une clé inconnue
/// est refusée par la route (400), jamais ignorée en silence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColonneDeTri {
    Titre,
    Artiste,
    Compositeur,
    Duree,
    Annee,
    Canaux,
    Bpm,
    Genre,
    Album,
    ArtisteAlbum,
    Disque,
    Label,
    Format,
    Frequence,
    Profondeur,
    Taille,
    Chemin,
    Isrc,
    Mbid,
    Commentaires,
    SousTitreDisque,
    Source,
    Modifie,
    Empreinte,
}

/// Les clés acceptées, dans l'ordre du catalogue du client.
pub const COLONNES_TRIABLES: [(&str, ColonneDeTri); 24] = [
    ("title", ColonneDeTri::Titre),
    ("artist", ColonneDeTri::Artiste),
    ("composer", ColonneDeTri::Compositeur),
    ("time", ColonneDeTri::Duree),
    ("year", ColonneDeTri::Annee),
    ("channels", ColonneDeTri::Canaux),
    ("bpm", ColonneDeTri::Bpm),
    ("genre", ColonneDeTri::Genre),
    ("album", ColonneDeTri::Album),
    ("albumArtist", ColonneDeTri::ArtisteAlbum),
    ("disc", ColonneDeTri::Disque),
    ("label", ColonneDeTri::Label),
    ("format", ColonneDeTri::Format),
    ("sampleRate", ColonneDeTri::Frequence),
    ("bitDepth", ColonneDeTri::Profondeur),
    ("size", ColonneDeTri::Taille),
    ("path", ColonneDeTri::Chemin),
    ("isrc", ColonneDeTri::Isrc),
    ("mbid", ColonneDeTri::Mbid),
    ("comments", ColonneDeTri::Commentaires),
    ("discSubtitle", ColonneDeTri::SousTitreDisque),
    ("source", ColonneDeTri::Source),
    ("modified", ColonneDeTri::Modifie),
    ("hash", ColonneDeTri::Empreinte),
];

/// Comment une valeur « vide » se reconnaît, pour la ranger en FIN de liste
/// dans les deux sens : une cellule vide en tête d'un tri descendant cacherait
/// ce qu'on cherche, et SQLite et PostgreSQL ne rangent pas `NULL` au même
/// endroit.
enum Vide {
    /// `NULL` ou chaîne blanche.
    Texte,
    /// `NULL` seulement.
    Nul,
    /// `NULL` ou `0` — la durée, la fréquence, la profondeur, l'année : le
    /// client affiche une cellule vide pour 0 (`valeurColonne`).
    NulOuZero,
}

impl ColonneDeTri {
    pub fn depuis_cle(cle: &str) -> Option<Self> {
        COLONNES_TRIABLES
            .iter()
            .find(|(c, _)| *c == cle)
            .map(|(_, col)| *col)
    }

    pub fn cle(self) -> &'static str {
        COLONNES_TRIABLES
            .iter()
            .find(|(_, col)| *col == self)
            .map(|(c, _)| *c)
            .unwrap_or("title")
    }

    /// L'expression de tri (alias de [`sql::track_from`]) et sa règle de vide.
    fn expression(self, engine: Engine) -> (String, Vide) {
        // Le texte se trie plié — casse et accents — comme la recherche :
        // « Été » se range avec les E.
        let plie = |col: &str| (format!("LOWER(unaccent({col}))"), Vide::Texte);
        match self {
            Self::Titre => plie("t.title"),
            Self::Artiste => plie("ar.name"),
            Self::Compositeur => plie("t.composer"),
            Self::Genre => plie("t.genre"),
            Self::Album => plie("al.title"),
            // Même repli que la colonne servie (`select_track`) : l'artiste
            // canonique de l'album quand le tag ALBUMARTIST manque.
            Self::ArtisteAlbum => plie("COALESCE(NULLIF(t.album_artist, ''), aal.name)"),
            Self::Label => plie("t.label"),
            Self::Commentaires => plie("t.comments"),
            Self::SousTitreDisque => plie("t.disc_subtitle"),
            Self::Format => ("LOWER(t.format)".into(), Vide::Texte),
            Self::Chemin => (
                "COALESCE(t.file_path, t.cue_media_path)".into(),
                Vide::Texte,
            ),
            Self::Isrc => ("t.isrc".into(), Vide::Texte),
            Self::Mbid => ("t.musicbrainz_recording_id".into(), Vide::Texte),
            Self::Empreinte => ("t.audio_hash".into(), Vide::Texte),
            Self::Source => (expression_provenance(engine), Vide::Texte),
            Self::Duree => ("t.duration_ms".into(), Vide::NulOuZero),
            Self::Annee => ("t.year".into(), Vide::NulOuZero),
            Self::Frequence => ("t.sample_rate".into(), Vide::NulOuZero),
            Self::Profondeur => ("t.bit_depth".into(), Vide::NulOuZero),
            Self::Canaux => ("t.channels".into(), Vide::Nul),
            Self::Bpm => ("t.bpm".into(), Vide::Nul),
            Self::Disque => ("CAST(t.disc_number AS INTEGER)".into(), Vide::Nul),
            Self::Taille => ("t.file_size".into(), Vide::Nul),
            Self::Modifie => ("t.file_mtime".into(), Vide::Nul),
        }
    }
}

/// L'ordre par défaut de la vue pistes — celui de `list_visible` et de
/// `list_filtered` —, qui départage aussi les ex æquo d'un tri choisi.
/// `t.id` en dernier : l'ordre est TOTAL, une page ne peut pas en recouvrir
/// une autre.
const ORDRE_PAR_DEFAUT: &str = "LOWER(ar.name), LOWER(al.title), \
     CAST(t.disc_number AS INTEGER), CAST(t.track_number AS INTEGER), t.id";

/// La clé de provenance d'une piste, en SQL (alias `t`) — le jumeau de
/// `provenanceDe` (`provenanceBibliotheque.ts`) :
///
/// ```text
/// source = item.source?.trim() || 'local'
/// source ≠ 'upnp'                         → source
/// [udn, identite] = source_id.split('|')
/// udn.trim() non vide ET identite non vide → 'upnp:' + udn.trim()
/// sinon                                    → 'upnp'
/// ```
pub fn expression_provenance(engine: Engine) -> String {
    let pos = match engine {
        Engine::Sqlite => "instr(t.source_id, '|')",
        Engine::Postgres => "strpos(t.source_id, '|')",
    };
    format!(
        "(CASE WHEN {src} = 'upnp' \
           AND {pos} > 1 \
           AND TRIM(substr(t.source_id, 1, {pos} - 1)) <> '' \
           AND substr(t.source_id, {pos} + 1, 1) NOT IN ('', '|') \
         THEN 'upnp:' || TRIM(substr(t.source_id, 1, {pos} - 1)) \
         ELSE {src} END)",
        src = SOURCE_NORMALISEE,
    )
}

/// `source` telle que la lit le client : blanche ou absente, c'est `local`.
const SOURCE_NORMALISEE: &str = "COALESCE(NULLIF(TRIM(t.source), ''), 'local')";

/// Ce que demande l'écran.
#[derive(Clone, Debug, Default)]
pub struct DemandeDePistes {
    /// Les facettes d'Oxygen, `folder` (portée Répertoires) compris.
    pub filtre: TrackFilter,
    /// La recherche de l'onglet : titre OU artiste.
    pub recherche: Option<String>,
    /// La clé de provenance (`local`, `upnp`, `upnp:<UDN>`, `qobuz`…).
    pub provenance: Option<String>,
    /// `None` : l'ordre par défaut de la vue.
    pub tri: Option<ColonneDeTri>,
    pub descendant: bool,
    pub limit: i64,
    pub offset: i64,
    /// Rendre aussi les comptes par provenance (sous la recherche et les
    /// facettes, SANS la provenance : ce sont les choix du menu Source).
    pub avec_comptes: bool,
}

/// Les comptes par provenance que rend une page qui les demande.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ComptesParProvenance {
    /// Clé de provenance → effectif, trié par clé. `upnp` porte l'AGRÉGAT de
    /// tous les serveurs UPnP, comme `compterSources` côté client.
    pub comptes: Vec<(String, i64)>,
    /// Toutes provenances confondues — « Toutes les sources ». Chaque piste
    /// n'a qu'une provenance : ce n'est PAS la somme de `comptes`, qui compte
    /// une piste UPnP deux fois (son serveur et l'agrégat).
    pub total: i64,
}

#[derive(Clone, Debug, Default)]
pub struct PageDePistes {
    pub pistes: Vec<Track>,
    /// Effectif de l'ensemble filtré, avant LIMIT/OFFSET.
    pub total: i64,
    pub comptes: Option<ComptesParProvenance>,
}

/// La recherche de l'onglet Titres : sous-chaîne du titre ou du nom de
/// l'artiste, pliée des deux côtés. La saisie est LITTÉRALE (`%` et `_`
/// échappés), comme le `includes` du navigateur.
fn condition_titre_ou_artiste(ph: &mut Placeholders, saisie: &str) -> (String, Vec<SqlValue>) {
    let motif = format!("%{}%", super::echapper_jokers_like(saisie));
    let esc = super::like_escape_clause();
    let titre = ph.take();
    let artiste = ph.take();
    (
        format!(
            "(LOWER(unaccent(t.title)) LIKE LOWER(unaccent({titre})){esc} \
             OR t.artist_id IN (SELECT id FROM artists \
                                WHERE LOWER(unaccent(name)) LIKE LOWER(unaccent({artiste})){esc}))"
        ),
        vec![SqlValue::Text(motif.clone()), SqlValue::Text(motif)],
    )
}

/// Le prédicat de provenance. `upnp` désigne TOUS les serveurs UPnP
/// (`sourceCorrespond`) ; toute autre clé se compare exactement.
fn condition_provenance(
    ph: &mut Placeholders,
    engine: Engine,
    cle: &str,
) -> (String, Vec<SqlValue>) {
    if cle == "upnp" {
        return (format!("{SOURCE_NORMALISEE} = 'upnp'"), Vec::new());
    }
    (
        format!("{} = {}", expression_provenance(engine), ph.take()),
        vec![SqlValue::Text(cle.to_string())],
    )
}

fn refs(valeurs: &[SqlValue]) -> Vec<&dyn ToSqlValue> {
    valeurs.iter().map(|v| v as &dyn ToSqlValue).collect()
}

impl TrackRepo {
    /// Une page de pistes triée, filtrée, et son total — tune-web-client#1716.
    ///
    /// Deux temps, comme [`Self::list_visible_avec_total`] : les IDENTIFIANTS
    /// de la page avec `COUNT(*) OVER ()` (le tri ne porte que des lignes
    /// étroites), puis les pistes de ces identifiants dans cet ordre.
    pub fn page_de_pistes(&self, d: &DemandeDePistes) -> Result<PageDePistes, TuneError> {
        let engine = self.db.engine();
        let mut ph = Placeholders::new(engine);
        let (mut conditions, mut valeurs) = self.conditions_du_filtre(&d.filtre, &mut ph);
        if let Some(saisie) = d
            .recherche
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            let (c, v) = condition_titre_ou_artiste(&mut ph, saisie);
            conditions.push(c);
            valeurs.extend(v);
        }

        // Les comptes se lisent AVANT la provenance : ce sont les choix du
        // menu Source, chacun compté sous la recherche. Les marqueurs de la
        // provenance viennent après ceux-ci, la numérotation PostgreSQL reste
        // donc valable pour les deux requêtes.
        let comptes = if d.avec_comptes {
            Some(self.comptes_par_provenance(&conditions, &valeurs)?)
        } else {
            None
        };

        if let Some(cle) = d
            .provenance
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            let (c, v) = condition_provenance(&mut ph, engine, cle);
            conditions.push(c);
            valeurs.extend(v);
        }

        let ordre = match d.tri {
            None => ORDRE_PAR_DEFAUT.to_string(),
            Some(col) => {
                let (expr, vide) = col.expression(engine);
                let test_vide = match vide {
                    Vide::Texte => format!("NULLIF(TRIM({expr}), '') IS NULL"),
                    Vide::Nul => format!("{expr} IS NULL"),
                    Vide::NulOuZero => format!("({expr} IS NULL OR {expr} = 0)"),
                };
                let sens = if d.descendant { "DESC" } else { "ASC" };
                format!(
                    "CASE WHEN {test_vide} THEN 1 ELSE 0 END, {expr} {sens}, {ORDRE_PAR_DEFAUT}"
                )
            }
        };
        let where_clause = conditions.join(" AND ");
        let limite = ph.take();
        let decalage = ph.take();
        let sql_ids = format!(
            "SELECT t.id, COUNT(*) OVER (){} WHERE {where_clause} ORDER BY {ordre} \
             LIMIT {limite} OFFSET {decalage}",
            sql::track_from()
        );
        let mut avec_page = valeurs.clone();
        avec_page.push(SqlValue::Int(d.limit));
        avec_page.push(SqlValue::Int(d.offset));
        let lignes = self.db.query_many(&sql_ids, &refs(&avec_page))?;
        let ids: Vec<i64> = lignes
            .iter()
            .filter_map(|r| r.first().and_then(|v| v.as_i64()))
            .collect();
        let total = match lignes
            .first()
            .and_then(|r| r.get(1))
            .and_then(|v| v.as_i64())
        {
            Some(n) => n,
            // Page vide (décalage au-delà de la fin) : on recompte.
            None => self
                .db
                .query_one(
                    &format!("SELECT COUNT(*){} WHERE {where_clause}", sql::track_from()),
                    &refs(&valeurs),
                )?
                .as_ref()
                .and_then(|c| c.first().and_then(|v| v.as_i64()))
                .unwrap_or(0),
        };
        Ok(PageDePistes {
            pistes: self.hydrater_dans_l_ordre(&ids)?,
            total,
            comptes,
        })
    }

    fn comptes_par_provenance(
        &self,
        conditions: &[String],
        valeurs: &[SqlValue],
    ) -> Result<ComptesParProvenance, TuneError> {
        let prov = expression_provenance(self.db.engine());
        let sql_comptes = format!(
            "SELECT {prov}, COUNT(*){} WHERE {} GROUP BY {prov}",
            sql::track_from(),
            conditions.join(" AND ")
        );
        let lignes = self.db.query_many(&sql_comptes, &refs(valeurs))?;
        let mut comptes: std::collections::BTreeMap<String, i64> = lignes
            .iter()
            .filter_map(|r| {
                Some((
                    r.first()?.as_string()?,
                    r.get(1).and_then(|v| v.as_i64()).unwrap_or(0),
                ))
            })
            .collect();
        let total = comptes.values().sum();
        let upnp: i64 = comptes
            .iter()
            .filter(|(cle, _)| cle.as_str() == "upnp" || cle.starts_with("upnp:"))
            .map(|(_, n)| n)
            .sum();
        if upnp > 0 {
            comptes.insert("upnp".into(), upnp);
        }
        Ok(ComptesParProvenance {
            comptes: comptes.into_iter().collect(),
            total,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::album_repo::AlbumRepo;
    use crate::db::artist_repo::ArtistRepo;
    use crate::db::models::Artist;
    use crate::db::sqlite::SqliteDb;

    /// Une petite bibliothèque : deux artistes locaux, un serveur UPnP, un
    /// service, des accents et des cellules vides.
    pub(crate) fn bibliotheque(repo: &TrackRepo, artistes: &ArtistRepo, albums: &AlbumRepo) {
        let abba = artistes.create(&Artist::new("ABBA".into())).unwrap();
        let ella = artistes.create(&Artist::new("Éla Ferré".into())).unwrap();
        let alb_a = albums.get_or_create("Arrival", abba, None).unwrap().id;
        let alb_e = albums.get_or_create("Zénith", ella, None).unwrap().id;
        let piste = |titre: &str,
                     artiste: i64,
                     album: Option<i64>,
                     n: i32,
                     duree: i64,
                     source: &str,
                     source_id: Option<&str>| {
            let mut t = Track::new(titre.to_string());
            t.artist_id = Some(artiste);
            t.album_id = album;
            t.track_number = n;
            t.duration_ms = duree;
            t.source = source.to_string();
            t.source_id = source_id.map(str::to_string);
            t.file_path = Some(format!("/m/{source}/{titre}.flac"));
            repo.create(&t).unwrap();
        };
        piste("Dancing Queen", abba, alb_a, 1, 230_000, "local", None);
        piste(
            "Money, Money, Money",
            abba,
            alb_a,
            2,
            185_000,
            "local",
            None,
        );
        piste("Été indien", ella, alb_e, 1, 0, "local", None);
        piste("Azur 100%", ella, alb_e, 2, 300_000, "local", None);
        piste(
            "Azur distant",
            ella,
            None,
            1,
            120_000,
            "upnp",
            Some("uuid-nas|42"),
        );
        piste("Orphelin", ella, None, 2, 90_000, "upnp", Some("|43"));
        piste("Azur qobuz", abba, None, 1, 200_000, "qobuz", Some("q-1"));
    }

    fn base() -> (TrackRepo, ArtistRepo, AlbumRepo) {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        (
            TrackRepo::new(db.clone()),
            ArtistRepo::new(db.clone()),
            AlbumRepo::new(db),
        )
    }

    fn titres(page: &PageDePistes) -> Vec<String> {
        page.pistes.iter().map(|t| t.title.clone()).collect()
    }

    fn demande() -> DemandeDePistes {
        DemandeDePistes {
            limit: 50,
            ..Default::default()
        }
    }

    /// Sans tri ni recherche, la page est celle de la vue par défaut : mêmes
    /// lignes, même ordre que `list_visible`.
    #[test]
    fn sans_tri_la_page_est_celle_de_la_vue_par_defaut() {
        let (repo, ar, al) = base();
        bibliotheque(&repo, &ar, &al);
        let page = repo.page_de_pistes(&demande()).unwrap();
        let attendu: Vec<String> = repo
            .list_visible(50, 0)
            .unwrap()
            .into_iter()
            .map(|t| t.title)
            .collect();
        assert_eq!(titres(&page), attendu);
        assert_eq!(page.total, 7);
        assert!(page.comptes.is_none());
    }

    #[test]
    fn le_tri_par_titre_plie_les_accents_et_suit_le_sens() {
        let (repo, ar, al) = base();
        bibliotheque(&repo, &ar, &al);
        let mut d = demande();
        d.tri = Some(ColonneDeTri::Titre);
        let page = repo.page_de_pistes(&d).unwrap();
        assert_eq!(
            titres(&page),
            [
                "Azur 100%",
                "Azur distant",
                "Azur qobuz",
                "Dancing Queen",
                "Été indien",
                "Money, Money, Money",
                "Orphelin"
            ]
        );
        d.descendant = true;
        let page = repo.page_de_pistes(&d).unwrap();
        assert_eq!(titres(&page).first().map(String::as_str), Some("Orphelin"));
        assert_eq!(titres(&page).last().map(String::as_str), Some("Azur 100%"));
    }

    /// Une durée nulle est une cellule VIDE : en fin de liste dans les deux
    /// sens.
    #[test]
    fn une_valeur_vide_reste_en_fin_de_liste_dans_les_deux_sens() {
        let (repo, ar, al) = base();
        bibliotheque(&repo, &ar, &al);
        let mut d = demande();
        d.tri = Some(ColonneDeTri::Duree);
        let asc = titres(&repo.page_de_pistes(&d).unwrap());
        assert_eq!(asc.first().map(String::as_str), Some("Orphelin"));
        assert_eq!(asc.last().map(String::as_str), Some("Été indien"));
        d.descendant = true;
        let desc = titres(&repo.page_de_pistes(&d).unwrap());
        assert_eq!(desc.first().map(String::as_str), Some("Azur 100%"));
        assert_eq!(desc.last().map(String::as_str), Some("Été indien"));
    }

    /// La recherche de l'onglet : titre OU artiste, pliée, littérale.
    #[test]
    fn la_recherche_porte_sur_le_titre_ou_l_artiste_pliee_et_litterale() {
        let (repo, ar, al) = base();
        bibliotheque(&repo, &ar, &al);
        let mut d = demande();
        d.tri = Some(ColonneDeTri::Titre);
        d.recherche = Some("azur".into());
        assert_eq!(
            titres(&repo.page_de_pistes(&d).unwrap()),
            ["Azur 100%", "Azur distant", "Azur qobuz"]
        );
        // L'artiste, accent omis dans la saisie.
        d.recherche = Some("ela fer".into());
        assert_eq!(repo.page_de_pistes(&d).unwrap().total, 4);
        // `%` est un caractère, pas un joker.
        d.recherche = Some("100%".into());
        assert_eq!(titres(&repo.page_de_pistes(&d).unwrap()), ["Azur 100%"]);
        d.recherche = Some("r%d".into());
        assert_eq!(repo.page_de_pistes(&d).unwrap().total, 0);
    }

    /// Les clés de `provenanceDe`, l'agrégat `upnp` de `compterSources`, et
    /// « Toutes les sources » qui ne compte chaque piste qu'une fois.
    #[test]
    fn les_comptes_par_provenance_suivent_le_client() {
        let (repo, ar, al) = base();
        bibliotheque(&repo, &ar, &al);
        let mut d = demande();
        d.avec_comptes = true;
        d.recherche = Some("azur".into());
        let page = repo.page_de_pistes(&d).unwrap();
        let comptes = page.comptes.unwrap();
        assert_eq!(
            comptes.comptes,
            vec![
                ("local".to_string(), 1),
                ("qobuz".to_string(), 1),
                ("upnp".to_string(), 1),
                ("upnp:uuid-nas".to_string(), 1),
            ]
        );
        assert_eq!(comptes.total, 3);

        d.recherche = None;
        let comptes = repo.page_de_pistes(&d).unwrap().comptes.unwrap();
        // « |43 » n'a pas d'UDN : provenance `upnp` nue, comptée dans l'agrégat.
        assert!(comptes.comptes.contains(&("upnp".to_string(), 2)));
        assert_eq!(comptes.total, 7);
    }

    /// La provenance filtre la page, pas les comptes (ce sont les choix du
    /// menu), et `upnp` désigne tous les serveurs.
    #[test]
    fn la_provenance_filtre_la_page_et_pas_les_comptes() {
        let (repo, ar, al) = base();
        bibliotheque(&repo, &ar, &al);
        let mut d = demande();
        d.avec_comptes = true;
        d.provenance = Some("upnp".into());
        let page = repo.page_de_pistes(&d).unwrap();
        assert_eq!(page.total, 2);
        assert_eq!(page.comptes.unwrap().total, 7);
        d.provenance = Some("upnp:uuid-nas".into());
        assert_eq!(titres(&repo.page_de_pistes(&d).unwrap()), ["Azur distant"]);
        d.provenance = Some("local".into());
        d.recherche = Some("azur".into());
        assert_eq!(titres(&repo.page_de_pistes(&d).unwrap()), ["Azur 100%"]);
    }

    /// Les pages se suivent sans se recouvrir, et une page au-delà de la fin
    /// garde le vrai total.
    #[test]
    fn les_pages_se_suivent_et_le_total_survit_a_une_page_vide() {
        let (repo, ar, al) = base();
        bibliotheque(&repo, &ar, &al);
        let mut d = demande();
        d.tri = Some(ColonneDeTri::Artiste);
        d.limit = 3;
        let mut vus = Vec::new();
        for offset in [0, 3, 6] {
            d.offset = offset;
            let page = repo.page_de_pistes(&d).unwrap();
            assert_eq!(page.total, 7);
            vus.extend(titres(&page));
        }
        assert_eq!(vus.len(), 7);
        let uniques: std::collections::HashSet<_> = vus.iter().collect();
        assert_eq!(uniques.len(), 7, "deux pages se recouvrent : {vus:?}");
        d.offset = 40;
        let page = repo.page_de_pistes(&d).unwrap();
        assert!(page.pistes.is_empty());
        assert_eq!(page.total, 7);
    }

    /// Les facettes d'Oxygen et le socle de la vue s'appliquent : le `WHERE`
    /// est celui de `list_filtered`.
    #[test]
    fn les_facettes_et_le_socle_de_la_vue_s_appliquent() {
        let (repo, ar, al) = base();
        bibliotheque(&repo, &ar, &al);
        let mut d = demande();
        d.filtre.folder = Some("/m/local".into());
        d.tri = Some(ColonneDeTri::Titre);
        let page = repo.page_de_pistes(&d).unwrap();
        let (liste, total) = repo.list_filtered(&d.filtre, 50, 0).unwrap();
        assert_eq!(page.total, total);
        let mut attendu: Vec<String> = liste.into_iter().map(|t| t.title).collect();
        attendu.sort_by_key(|t| crate::db::engine::fold_diacritics(t).to_lowercase());
        assert_eq!(titres(&page), attendu);
    }

    /// Chaque clé du catalogue se trie sans erreur SQL, dans les deux sens.
    #[test]
    fn chaque_colonne_triable_produit_un_sql_valide() {
        let (repo, ar, al) = base();
        bibliotheque(&repo, &ar, &al);
        for (cle, col) in COLONNES_TRIABLES {
            assert_eq!(ColonneDeTri::depuis_cle(cle), Some(col));
            assert_eq!(col.cle(), cle);
            for descendant in [false, true] {
                let mut d = demande();
                d.tri = Some(col);
                d.descendant = descendant;
                let page = repo
                    .page_de_pistes(&d)
                    .unwrap_or_else(|e| panic!("tri {cle} : {e}"));
                assert_eq!(page.total, 7, "tri {cle}");
            }
        }
        assert_eq!(ColonneDeTri::depuis_cle("plays"), None);
    }

    /// Le MÊME scénario sur PostgreSQL : `strpos` au lieu d'`instr`,
    /// `unaccent` de l'extension, `NULL` rangés à l'autre bout par défaut,
    /// marqueurs numérotés (`$n`) partagés entre la page et ses comptes.
    ///
    /// Doctrine de `postgres_e2e.rs` : `TUNE_TEST_PG_URL` absente ⇒ saut
    /// annoncé ; posée mais injoignable ⇒ l'épreuve TOMBE. Elle vide
    /// `tracks`, `albums`, `artists` et `hidden_items` : à lancer seule
    /// (`-- pg_1716 --test-threads=1`).
    #[cfg(feature = "postgres")]
    #[tokio::test(flavor = "multi_thread")]
    async fn pg_1716_la_page_triee_et_ses_comptes_sur_postgresql() {
        use crate::db::backend::{DbBackend, PostgresBackend};
        use std::sync::Arc;
        let Ok(url) = std::env::var("TUNE_TEST_PG_URL") else {
            eprintln!(
                "SAUT : TUNE_TEST_PG_URL non posée — pg_1716 rend la main sans toucher aucune base."
            );
            return;
        };
        let pool = sqlx::PgPool::connect(&url)
            .await
            .unwrap_or_else(|e| panic!("TUNE_TEST_PG_URL posée ({url}) mais injoignable : {e}"));
        let db: Arc<dyn DbBackend> = Arc::new(PostgresBackend::new(pool));
        for table in ["hidden_items", "tracks", "albums", "artists"] {
            db.execute(
                &format!("TRUNCATE TABLE {table} RESTART IDENTITY CASCADE"),
                &[],
            )
            .unwrap_or_else(|e| panic!("TRUNCATE {table} : {e}"));
        }
        let repo = TrackRepo::with_backend(db.clone());
        bibliotheque(
            &repo,
            &ArtistRepo::with_backend(db.clone()),
            &AlbumRepo::with_backend(db.clone()),
        );

        // Tri par titre, accents pliés, dans les deux sens.
        let mut d = demande();
        d.tri = Some(ColonneDeTri::Titre);
        assert_eq!(
            titres(&repo.page_de_pistes(&d).unwrap()),
            [
                "Azur 100%",
                "Azur distant",
                "Azur qobuz",
                "Dancing Queen",
                "Été indien",
                "Money, Money, Money",
                "Orphelin"
            ]
        );
        // Cellule vide en fin de liste dans les deux sens, malgré `NULLS
        // FIRST` implicite de PostgreSQL en ordre décroissant.
        d.tri = Some(ColonneDeTri::Duree);
        d.descendant = true;
        let desc = titres(&repo.page_de_pistes(&d).unwrap());
        assert_eq!(desc.first().map(String::as_str), Some("Azur 100%"));
        assert_eq!(desc.last().map(String::as_str), Some("Été indien"));

        // Recherche, comptes et provenance : les marqueurs `$n` de la page et
        // des comptes ne se marchent pas dessus.
        let mut d = demande();
        d.avec_comptes = true;
        d.recherche = Some("azur".into());
        d.provenance = Some("upnp:uuid-nas".into());
        let page = repo.page_de_pistes(&d).unwrap();
        assert_eq!(titres(&page), ["Azur distant"]);
        let comptes = page.comptes.unwrap();
        assert_eq!(
            comptes.comptes,
            vec![
                ("local".to_string(), 1),
                ("qobuz".to_string(), 1),
                ("upnp".to_string(), 1),
                ("upnp:uuid-nas".to_string(), 1),
            ]
        );
        assert_eq!(comptes.total, 3);
        d.recherche = Some("ela fer".into());
        d.provenance = Some("upnp".into());
        assert_eq!(repo.page_de_pistes(&d).unwrap().total, 2);

        // Chaque colonne, chaque sens : un SQL que PostgreSQL accepte.
        for (cle, col) in COLONNES_TRIABLES {
            for descendant in [false, true] {
                let mut d = demande();
                d.tri = Some(col);
                d.descendant = descendant;
                d.offset = 5;
                let page = repo
                    .page_de_pistes(&d)
                    .unwrap_or_else(|e| panic!("tri {cle} sur PostgreSQL : {e}"));
                assert_eq!(page.total, 7, "tri {cle}");
                assert_eq!(page.pistes.len(), 2, "tri {cle}");
            }
        }
        // Page au-delà de la fin : le total survit.
        let mut d = demande();
        d.offset = 50;
        assert_eq!(repo.page_de_pistes(&d).unwrap().total, 7);
    }

    /// BANC de mesure (tune-web-client#1716), ignoré par défaut : la page
    /// d'avant (`list_visible_avec_total`, pages de 200 et de 5 000 — ce
    /// que chargeait l'onglet) contre la page triée, sur une base réelle.
    ///
    /// `TUNE_BANC_SQLITE=<chemin de tune.db>` et/ou
    /// `TUNE_BANC_PG_URL=<url>` ; `cargo test --release … banc_1716 -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn banc_1716() {
        fn mesurer(moteur: &str, repo: &TrackRepo) {
            let chrono = |nom: &str, f: &dyn Fn() -> usize| {
                let mut durees = Vec::new();
                let mut n = 0;
                for _ in 0..5 {
                    let t = std::time::Instant::now();
                    n = f();
                    durees.push(t.elapsed().as_secs_f64() * 1e3);
                }
                durees.sort_by(|a, b| a.partial_cmp(b).unwrap());
                println!(
                    "BANC {moteur} {nom} : médiane {:.0} ms (min {:.0}, max {:.0}) — {n} pistes",
                    durees[2], durees[0], durees[4]
                );
            };
            chrono("avant page 200", &|| {
                repo.list_visible_avec_total(200, 0).unwrap().0.len()
            });
            chrono("avant page 5000", &|| {
                repo.list_visible_avec_total(5000, 0).unwrap().0.len()
            });
            let page = |d: DemandeDePistes| move || repo.page_de_pistes(&d).unwrap().pistes.len();
            let base = DemandeDePistes {
                limit: 200,
                ..Default::default()
            };
            chrono(
                "après défaut + comptes",
                &page(DemandeDePistes {
                    avec_comptes: true,
                    ..base.clone()
                }),
            );
            chrono(
                "après défaut offset 20000",
                &page(DemandeDePistes {
                    offset: 20_000,
                    ..base.clone()
                }),
            );
            chrono(
                "après tri titre asc",
                &page(DemandeDePistes {
                    tri: Some(ColonneDeTri::Titre),
                    ..base.clone()
                }),
            );
            chrono(
                "après tri titre desc offset 30000",
                &page(DemandeDePistes {
                    tri: Some(ColonneDeTri::Titre),
                    descendant: true,
                    offset: 30_000,
                    ..base.clone()
                }),
            );
            chrono(
                "après tri durée",
                &page(DemandeDePistes {
                    tri: Some(ColonneDeTri::Duree),
                    ..base.clone()
                }),
            );
            chrono(
                "après tri source",
                &page(DemandeDePistes {
                    tri: Some(ColonneDeTri::Source),
                    ..base.clone()
                }),
            );
            chrono(
                "après recherche « Azur » + comptes",
                &page(DemandeDePistes {
                    recherche: Some("Azur".into()),
                    avec_comptes: true,
                    ..base.clone()
                }),
            );
            chrono(
                "après provenance upnp + tri artiste",
                &page(DemandeDePistes {
                    provenance: Some("upnp".into()),
                    tri: Some(ColonneDeTri::Artiste),
                    ..base.clone()
                }),
            );
        }
        if let Ok(chemin) = std::env::var("TUNE_BANC_SQLITE") {
            let db = SqliteDb::open(&chemin).expect("base SQLite du banc");
            mesurer("sqlite", &TrackRepo::new(db));
        }
        #[cfg(feature = "postgres")]
        if let Ok(url) = std::env::var("TUNE_BANC_PG_URL") {
            let rt = tokio::runtime::Runtime::new().unwrap();
            let pool = rt
                .block_on(sqlx::PgPool::connect(&url))
                .expect("base PostgreSQL du banc");
            let db: std::sync::Arc<dyn crate::db::backend::DbBackend> =
                std::sync::Arc::new(crate::db::backend::PostgresBackend::new(pool));
            // `PostgresBackend` passe par `block_in_place` : un fil de l'exécuteur.
            rt.block_on(async {
                tokio::task::spawn_blocking(move || {
                    mesurer("postgres", &TrackRepo::with_backend(db))
                })
                .await
                .unwrap()
            });
        }
    }
}
