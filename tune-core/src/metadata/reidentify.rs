//! Effacer l'identification d'UN album, et pouvoir la remettre.
//!
//! # Pourquoi ce module existe
//!
//! Toute l'écriture d'enrichissement de Tune est *fill-only* : chaque colonne
//! est écrite en `COALESCE(colonne, ?)` — voir
//! [`super::enrichment::write_track_enrichment`]. La règle est bonne (une passe
//! de fond n'a pas à écraser ce qui est déjà là), mais elle a une conséquence
//! que personne n'avait tirée : **une identification fausse est définitive**.
//! Le mauvais MBID est en place, donc le `COALESCE` le garde, donc la passe
//! suivante repart du mauvais enregistrement, indéfiniment.
//!
//! Et le mauvais MBID ne vient pas forcément de MusicBrainz : le scan lit
//! `MUSICBRAINZ_TRACKID` / `MusicBrainz Album Id` dans les balises du fichier
//! (`scan_import.rs:163` et `:443`). Un tagueur tiers qui s'est trompé — le cas
//! du fil forum #1455, où Kodi avait écrit dans les FLAC à partir d'une fiche
//! erronée — plante donc l'identification *avant même* le premier appel réseau.
//! C'est ce qui rend le rescan inopérant : il relit les mêmes balises fausses.
//! Le seul recours connu était de dupliquer le dossier sous un autre nom pour
//! casser la correspondance, ce qui faisait perdre favoris et historique.
//!
//! # Ce que ce module fait, et ce qu'il ne fait pas
//!
//! Il n'efface QUE les trois clés d'identification, et seulement pour l'album
//! demandé :
//!
//! | table    | colonne                        | portée              |
//! |----------|--------------------------------|---------------------|
//! | `albums` | `musicbrainz_release_id`       | `WHERE id = ?`      |
//! | `albums` | `musicbrainz_release_group_id` | `WHERE id = ?`      |
//! | `tracks` | `musicbrainz_recording_id`     | `WHERE album_id = ?`|
//!
//! Ces trois colonnes ont une propriété qu'aucune autre ne partage : **aucune
//! interface de Tune ne permet de les saisir**. Ni `AlbumEdit` ni `TrackEdit`
//! (`routes/metadata.rs`) ne les portent. Les effacer ne peut donc jamais
//! détruire une saisie de l'utilisateur.
//!
//! Rien d'autre n'est touché. En particulier **ne sont pas effacés** : le
//! titre, l'artiste, l'année, le genre, le label, le compositeur, l'`isrc`
//! (celui-là vient des balises, `scan_import.rs:162`), la pochette, le
//! numéro de catalogue, le code-barres, la biographie. Aucune ligne n'est
//! supprimée ni recréée : les `id` d'album et de piste ne bougent pas, donc
//! favoris, notes, historique d'écoute, listes de lecture et collections —
//! qui s'y rattachent tous par `id` — restent en place par construction.
//! Aucun fichier n'est écrit sur le disque.
//!
//! Enfin, ce qui est effacé est **rendu** : [`ClearedIdentification`] est le
//! calque d'avant, et [`restore_album_identification`] le repose tel quel si la
//! nouvelle passe ne trouve rien. Une ré-identification qui échoue laisse
//! l'album exactement comme elle l'a trouvé.

use std::sync::Arc;

use crate::db::backend::{DbBackend, ToSqlValue};

/// L'identification d'un album telle qu'elle était avant l'effacement.
///
/// C'est un calque, pas un journal : il ne sert qu'à reposer l'état d'avant si
/// la nouvelle passe ne donne rien. Il porte aussi de quoi répondre à la
/// question « est-ce que ça a changé ? » sans relire la base.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClearedIdentification {
    /// `albums.musicbrainz_release_id` d'avant.
    pub release_id: Option<String>,
    /// `albums.musicbrainz_release_group_id` d'avant.
    pub release_group_id: Option<String>,
    /// `(track_id, musicbrainz_recording_id)` d'avant, pour les seules pistes
    /// de cet album qui en portaient un.
    pub recording_ids: Vec<(i64, String)>,
}

impl ClearedIdentification {
    /// Y avait-il quelque chose à effacer ? Un album jamais identifié rend
    /// `false`, et la ré-identification est alors une première identification —
    /// ce qui n'est pas une erreur, mais mérite d'être dit à l'utilisateur.
    pub fn was_identified(&self) -> bool {
        self.release_id.is_some()
            || self.release_group_id.is_some()
            || !self.recording_ids.is_empty()
    }

    /// Le MBID d'enregistrement d'avant pour cette piste, s'il y en avait un.
    pub fn previous_recording_id(&self, track_id: i64) -> Option<&str> {
        self.recording_ids
            .iter()
            .find(|(id, _)| *id == track_id)
            .map(|(_, mbid)| mbid.as_str())
    }
}

/// Une chaîne vide compte pour « pas de valeur » : la base porte les deux
/// formes selon le chemin qui a écrit (le scan écrit `NULL`, certaines
/// remontées écrivent `''`), et les `WHERE ... IS NULL OR ... = ''` du dépôt
/// les traitent déjà à égalité.
fn non_empty(v: Option<&str>) -> Option<String> {
    v.map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Efface l'identification de l'album `album_id`, et rend ce qui a été effacé.
///
/// Trois `UPDATE`, tous bornés à cet album : deux `WHERE id = ?` sur `albums`,
/// un `WHERE album_id = ?` sur `tracks`. Aucun `DELETE`, aucun `INSERT`, aucune
/// autre table.
pub fn clear_album_identification(
    backend: &Arc<dyn DbBackend>,
    album_id: i64,
) -> Result<ClearedIdentification, String> {
    let mut cleared = ClearedIdentification::default();

    // 1. Relever l'album AVANT d'écrire.
    let album_row = backend.query_one(
        "SELECT musicbrainz_release_id, musicbrainz_release_group_id FROM albums WHERE id = ?",
        &[&album_id as &dyn ToSqlValue],
    )?;
    let Some(album_row) = album_row else {
        return Err(format!("album {album_id} introuvable"));
    };
    cleared.release_id = non_empty(album_row.first().and_then(|v| v.as_str()));
    cleared.release_group_id = non_empty(album_row.get(1).and_then(|v| v.as_str()));

    // 2. Relever les pistes de cet album, et d'elles seules.
    let track_rows = backend.query_many(
        "SELECT id, musicbrainz_recording_id FROM tracks WHERE album_id = ? ORDER BY id",
        &[&album_id as &dyn ToSqlValue],
    )?;
    for row in &track_rows {
        let Some(track_id) = row.first().and_then(|v| v.as_i64()) else {
            continue;
        };
        if let Some(mbid) = non_empty(row.get(1).and_then(|v| v.as_str())) {
            cleared.recording_ids.push((track_id, mbid));
        }
    }

    // 3. Effacer. `NullText` et pas `Null` : sur PostgreSQL un NULL non typé
    //    part en BIGINT et l'UPDATE échoue sur une colonne TEXT.
    let null_text = crate::db::backend::SqlValue::NullText;
    backend.execute(
        "UPDATE albums SET musicbrainz_release_id = ?, musicbrainz_release_group_id = ? \
         WHERE id = ?",
        &[
            &null_text as &dyn ToSqlValue,
            &null_text as &dyn ToSqlValue,
            &album_id as &dyn ToSqlValue,
        ],
    )?;
    backend.execute(
        "UPDATE tracks SET musicbrainz_recording_id = ? WHERE album_id = ?",
        &[&null_text as &dyn ToSqlValue, &album_id as &dyn ToSqlValue],
    )?;

    Ok(cleared)
}

/// Repose l'identification relevée par [`clear_album_identification`].
///
/// Appelée quand la nouvelle passe n'a rien trouvé : l'album doit se retrouver
/// exactement comme avant, pas amputé de ce qu'il avait. Bornée au même album
/// et aux mêmes pistes.
pub fn restore_album_identification(
    backend: &Arc<dyn DbBackend>,
    album_id: i64,
    cleared: &ClearedIdentification,
) -> Result<(), String> {
    let release_id = cleared.release_id.clone();
    let release_group_id = cleared.release_group_id.clone();
    backend.execute(
        "UPDATE albums SET musicbrainz_release_id = ?, musicbrainz_release_group_id = ? \
         WHERE id = ?",
        &[
            &release_id as &dyn ToSqlValue,
            &release_group_id as &dyn ToSqlValue,
            &album_id as &dyn ToSqlValue,
        ],
    )?;

    for (track_id, mbid) in &cleared.recording_ids {
        let mbid_val: Option<String> = Some(mbid.clone());
        // `AND album_id = ?` : la restitution ne peut pas déborder sur une
        // piste qui aurait changé d'album entre-temps.
        backend.execute(
            "UPDATE tracks SET musicbrainz_recording_id = ? WHERE id = ? AND album_id = ?",
            &[
                &mbid_val as &dyn ToSqlValue,
                track_id as &dyn ToSqlValue,
                &album_id as &dyn ToSqlValue,
            ],
        )?;
    }

    Ok(())
}

/// Une piste locale, réduite à ce qui sert à la faire correspondre.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalTrack {
    pub id: i64,
    pub disc: i32,
    pub position: i32,
    pub title: String,
}

/// Comparaison de titres tolérante à la casse, aux accents décoratifs et à la
/// ponctuation — « Round Midnight » et « 'Round Midnight! » sont le même titre.
fn normalize_title(s: &str) -> String {
    s.to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric() || c.is_whitespace())
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Associe chaque piste locale au MBID d'enregistrement de la piste
/// correspondante du pressage MusicBrainz.
///
/// D'abord par `(disque, position)`, qui est la correspondance de droit sur un
/// pressage : c'est la même œuvre au même rang. À défaut — numérotation locale
/// absente ou fausse, ce qui est fréquent sur les fichiers mal tagués, donc
/// précisément sur ceux qu'on ré-identifie — on retombe sur le titre, mais
/// **seulement si ce titre ne désigne qu'une piste de chaque côté**. Un album
/// qui répète un titre (reprises, prises alternatives) ne doit pas voir ses
/// pistes échangées en silence : dans ce cas on préfère ne rien associer.
///
/// Une piste sans correspondance n'est pas une erreur ; elle reste simplement
/// sans MBID, ce qui est un état honnête.
pub fn map_recording_ids(
    local: &[LocalTrack],
    mb: &[crate::metadata::musicbrainz_release::MBTrack],
) -> Vec<(i64, String)> {
    let mut out: Vec<(i64, String)> = Vec::new();
    let mut matched: Vec<bool> = vec![false; local.len()];

    // 1. Par (disque, position).
    for (i, lt) in local.iter().enumerate() {
        if lt.disc <= 0 || lt.position <= 0 {
            continue;
        }
        let hit = mb
            .iter()
            .find(|m| m.disc as i32 == lt.disc && m.position as i32 == lt.position)
            .and_then(|m| m.recording_id.as_ref());
        if let Some(rid) = hit {
            out.push((lt.id, rid.clone()));
            matched[i] = true;
        }
    }

    // 2. Par titre, pour le reste, et seulement si le titre est unique des deux
    //    cotes.
    for (i, lt) in local.iter().enumerate() {
        if matched[i] {
            continue;
        }
        let key = normalize_title(&lt.title);
        if key.is_empty() {
            continue;
        }
        let local_count = local
            .iter()
            .filter(|o| normalize_title(&o.title) == key)
            .count();
        if local_count != 1 {
            continue;
        }
        let mut candidates = mb
            .iter()
            .filter(|m| m.recording_id.is_some() && normalize_title(&m.title) == key);
        let Some(m) = candidates.next() else { continue };
        if candidates.next().is_some() {
            continue; // le titre designe plusieurs pistes du pressage
        }
        // Et ce MBID ne doit pas deja etre pris par une piste appariee au rang.
        if let Some(ref rid) = m.recording_id {
            if out.iter().any(|(_, r)| r == rid) {
                continue;
            }
            out.push((lt.id, rid.clone()));
        }
    }

    out.sort_by_key(|(id, _)| *id);
    out
}

/// Ce que la nouvelle identification a effectivement écrit.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AppliedIdentification {
    /// Nombre de pistes qui ont reçu un MBID d'enregistrement.
    pub tracks_matched: usize,
    /// Pistes de l'album restées sans correspondance dans le pressage.
    pub tracks_unmatched: usize,
    /// Champs descriptifs que le pressage aurait pu remplir mais qui portaient
    /// déjà une valeur — donc laissés tels quels. C'est ce qu'il faut dire à
    /// l'utilisateur : Tune n'a pas écrasé ce qui était là.
    pub fields_left_as_is: Vec<String>,
}

/// Pose la nouvelle identification sur l'album `album_id`.
///
/// Deux régimes, et la frontière entre les deux est la seule chose à retenir :
///
/// - **Remplacement** pour les trois clés d'identification
///   (`musicbrainz_release_id`, `musicbrainz_release_group_id`,
///   `tracks.musicbrainz_recording_id`). C'est la demande explicite de
///   l'utilisateur, et aucune interface ne permet de les saisir : on ne peut
///   donc rien écraser qu'il ait tapé.
/// - **Remplissage seul** (`COALESCE`) pour les champs descriptifs que le
///   pressage apporte au passage (`label`, `catalog_number`, `release_date`,
///   `year`). Ceux-là, l'utilisateur PEUT les avoir saisis (`AlbumEdit`), donc
///   on ne comble que les trous — exactement la règle du reste du dépôt.
///
/// Tout est borné à cet album : `WHERE id = ?` sur `albums`, et pour chaque
/// piste `WHERE id = ? AND album_id = ?`.
pub fn apply_album_identification(
    backend: &Arc<dyn DbBackend>,
    album_id: i64,
    release_id: &str,
    release_group_id: Option<&str>,
    recordings: &[(i64, String)],
    track_total: usize,
    detail: Option<&crate::metadata::musicbrainz_release::MBReleaseDetail>,
) -> Result<AppliedIdentification, String> {
    let mut applied = AppliedIdentification {
        tracks_matched: recordings.len(),
        tracks_unmatched: track_total.saturating_sub(recordings.len()),
        fields_left_as_is: Vec::new(),
    };

    // 1. Les clés, en remplacement.
    let rel: Option<String> = Some(release_id.to_string());
    let rg: Option<String> = release_group_id.map(str::to_string);
    backend.execute(
        "UPDATE albums SET musicbrainz_release_id = ?, musicbrainz_release_group_id = ? \
         WHERE id = ?",
        &[
            &rel as &dyn ToSqlValue,
            &rg as &dyn ToSqlValue,
            &album_id as &dyn ToSqlValue,
        ],
    )?;

    for (track_id, rid) in recordings {
        let rid_val: Option<String> = Some(rid.clone());
        backend.execute(
            "UPDATE tracks SET musicbrainz_recording_id = ? WHERE id = ? AND album_id = ?",
            &[
                &rid_val as &dyn ToSqlValue,
                track_id as &dyn ToSqlValue,
                &album_id as &dyn ToSqlValue,
            ],
        )?;
    }

    // 2. Le descriptif, en remplissage seul.
    let Some(detail) = detail else {
        return Ok(applied);
    };

    let before = backend
        .query_one(
            "SELECT label, catalog_number, release_date, year FROM albums WHERE id = ?",
            &[&album_id as &dyn ToSqlValue],
        )?
        .ok_or_else(|| format!("album {album_id} introuvable"))?;

    let label: Option<String> = detail.label.clone();
    let catalog: Option<String> = detail.catalog_number.clone();
    let date: Option<String> = detail.date.clone();
    let year: Option<i64> = detail.year.map(i64::from);

    // Dire ce qu'on n'a PAS ecrase, et seulement quand le pressage avait
    // vraiment quelque chose a proposer : annoncer un champ « conserve » alors
    // que MusicBrainz ne donnait rien serait un mensonge poli.
    for (idx, name, proposed) in [
        (0usize, "label", label.is_some()),
        (1, "catalog_number", catalog.is_some()),
        (2, "release_date", date.is_some()),
        (3, "year", year.is_some()),
    ] {
        let occupied = match idx {
            3 => before.get(idx).and_then(|v| v.as_i64()).is_some(),
            _ => non_empty(before.get(idx).and_then(|v| v.as_str())).is_some(),
        };
        if proposed && occupied {
            applied.fields_left_as_is.push(name.to_string());
        }
    }

    backend.execute(
        "UPDATE albums SET \
         label = COALESCE(label, ?), \
         catalog_number = COALESCE(catalog_number, ?), \
         release_date = COALESCE(release_date, ?), \
         year = COALESCE(year, ?) \
         WHERE id = ?",
        &[
            &label as &dyn ToSqlValue,
            &catalog as &dyn ToSqlValue,
            &date as &dyn ToSqlValue,
            &year as &dyn ToSqlValue,
            &album_id as &dyn ToSqlValue,
        ],
    )?;

    Ok(applied)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::migrations;
    use crate::db::sqlite::SqliteDb;
    use crate::metadata::musicbrainz_release::{MBReleaseDetail, MBTrack};

    /// Deux albums, deux pistes chacun, tous identifiés. Le second album est le
    /// témoin : il ne doit jamais bouger.
    fn setup() -> Arc<dyn DbBackend> {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();
        let backend: Arc<dyn DbBackend> = Arc::new(db);

        backend
            .execute_batch(
                "INSERT INTO albums (id, title, musicbrainz_release_id, musicbrainz_release_group_id, genre, year, label, catalog_number, barcode, cover_path) \
                   VALUES (1, 'Album vise', 'rel-FAUX', 'rg-FAUX', 'Jazz', 1971, 'Label A', 'CAT-1', 'BAR-1', '/couv/1.jpg'); \
                 INSERT INTO albums (id, title, musicbrainz_release_id, musicbrainz_release_group_id) \
                   VALUES (2, 'Album temoin', 'rel-TEMOIN', 'rg-TEMOIN'); \
                 INSERT INTO tracks (id, title, album_id, musicbrainz_recording_id, isrc, composer, genre, year) \
                   VALUES (10, 'A1', 1, 'rec-FAUX-1', 'FRZ121', 'Ravel', 'Jazz', 1971); \
                 INSERT INTO tracks (id, title, album_id, musicbrainz_recording_id, isrc) \
                   VALUES (11, 'A2', 1, 'rec-FAUX-2', 'FRZ122'); \
                 INSERT INTO tracks (id, title, album_id, musicbrainz_recording_id, isrc) \
                   VALUES (20, 'B1', 2, 'rec-TEMOIN', 'FRZ200');",
            )
            .unwrap();
        backend
    }

    fn album_ids(backend: &Arc<dyn DbBackend>, id: i64) -> (Option<String>, Option<String>) {
        let row = backend
            .query_one(
                "SELECT musicbrainz_release_id, musicbrainz_release_group_id FROM albums WHERE id = ?",
                &[&id as &dyn ToSqlValue],
            )
            .unwrap()
            .unwrap();
        (
            row.first().and_then(|v| v.as_string()),
            row.get(1).and_then(|v| v.as_string()),
        )
    }

    fn recording_id(backend: &Arc<dyn DbBackend>, track_id: i64) -> Option<String> {
        backend
            .query_one(
                "SELECT musicbrainz_recording_id FROM tracks WHERE id = ?",
                &[&track_id as &dyn ToSqlValue],
            )
            .unwrap()
            .unwrap()
            .first()
            .and_then(|v| v.as_string())
    }

    /// Le geste de base : les trois clés partent, et elles sont rendues.
    #[test]
    fn efface_les_trois_cles_et_rend_ce_qu_il_efface() {
        let backend = setup();

        let cleared = clear_album_identification(&backend, 1).unwrap();

        assert_eq!(cleared.release_id.as_deref(), Some("rel-FAUX"));
        assert_eq!(cleared.release_group_id.as_deref(), Some("rg-FAUX"));
        assert_eq!(
            cleared.recording_ids,
            vec![
                (10, "rec-FAUX-1".to_string()),
                (11, "rec-FAUX-2".to_string())
            ]
        );
        assert!(cleared.was_identified());

        assert_eq!(album_ids(&backend, 1), (None, None));
        assert_eq!(recording_id(&backend, 10), None);
        assert_eq!(recording_id(&backend, 11), None);
    }

    /// ⚠ Le point qui compte : l'effet est BORNÉ à l'album demandé.
    #[test]
    fn n_efface_rien_en_dehors_de_l_album_vise() {
        let backend = setup();

        clear_album_identification(&backend, 1).unwrap();

        assert_eq!(
            album_ids(&backend, 2),
            (
                Some("rel-TEMOIN".to_string()),
                Some("rg-TEMOIN".to_string())
            ),
            "l'album temoin a perdu son identification"
        );
        assert_eq!(
            recording_id(&backend, 20),
            Some("rec-TEMOIN".to_string()),
            "la piste de l'album temoin a perdu son MBID"
        );
    }

    /// ⚠ L'autre point qui compte : rien d'autre que les clés ne part.
    #[test]
    fn ne_touche_ni_aux_champs_saisissables_ni_a_l_isrc_ni_a_la_pochette() {
        let backend = setup();

        clear_album_identification(&backend, 1).unwrap();

        let row = backend
            .query_one(
                "SELECT title, genre, year, label, catalog_number, barcode, cover_path \
                 FROM albums WHERE id = 1",
                &[],
            )
            .unwrap()
            .unwrap();
        assert_eq!(row[0].as_string().as_deref(), Some("Album vise"));
        assert_eq!(row[1].as_string().as_deref(), Some("Jazz"));
        assert_eq!(row[2].as_i64(), Some(1971));
        assert_eq!(row[3].as_string().as_deref(), Some("Label A"));
        assert_eq!(row[4].as_string().as_deref(), Some("CAT-1"));
        assert_eq!(row[5].as_string().as_deref(), Some("BAR-1"));
        assert_eq!(row[6].as_string().as_deref(), Some("/couv/1.jpg"));

        let t = backend
            .query_one(
                "SELECT isrc, composer, genre, year, title FROM tracks WHERE id = 10",
                &[],
            )
            .unwrap()
            .unwrap();
        assert_eq!(t[0].as_string().as_deref(), Some("FRZ121"), "isrc efface");
        assert_eq!(t[1].as_string().as_deref(), Some("Ravel"));
        assert_eq!(t[2].as_string().as_deref(), Some("Jazz"));
        assert_eq!(t[3].as_i64(), Some(1971));
        assert_eq!(t[4].as_string().as_deref(), Some("A1"));
    }

    /// Aucune ligne n'est supprimee ni recreee : les `id` sont la clef de
    /// jointure des favoris, des notes et de l'historique.
    #[test]
    fn ne_supprime_ni_ne_renumerote_aucune_ligne() {
        let backend = setup();

        clear_album_identification(&backend, 1).unwrap();

        let ids = backend
            .query_many("SELECT id FROM tracks ORDER BY id", &[])
            .unwrap();
        let ids: Vec<i64> = ids.iter().filter_map(|r| r[0].as_i64()).collect();
        assert_eq!(ids, vec![10, 11, 20]);

        let albums = backend
            .query_many("SELECT id FROM albums ORDER BY id", &[])
            .unwrap();
        let albums: Vec<i64> = albums.iter().filter_map(|r| r[0].as_i64()).collect();
        assert_eq!(albums, vec![1, 2]);
    }

    /// Une passe qui ne trouve rien doit laisser l'album comme elle l'a trouve.
    #[test]
    fn la_restitution_repose_exactement_l_etat_d_avant() {
        let backend = setup();

        let cleared = clear_album_identification(&backend, 1).unwrap();
        restore_album_identification(&backend, 1, &cleared).unwrap();

        assert_eq!(
            album_ids(&backend, 1),
            (Some("rel-FAUX".to_string()), Some("rg-FAUX".to_string()))
        );
        assert_eq!(recording_id(&backend, 10), Some("rec-FAUX-1".to_string()));
        assert_eq!(recording_id(&backend, 11), Some("rec-FAUX-2".to_string()));
    }

    /// Un album jamais identifie s'efface sans erreur, et le dit.
    #[test]
    fn album_jamais_identifie_rend_un_calque_vide() {
        let backend = setup();
        backend
            .execute_batch(
                "INSERT INTO albums (id, title) VALUES (3, 'Jamais identifie'); \
                 INSERT INTO tracks (id, title, album_id) VALUES (30, 'C1', 3);",
            )
            .unwrap();

        let cleared = clear_album_identification(&backend, 3).unwrap();

        assert!(!cleared.was_identified());
        assert!(cleared.recording_ids.is_empty());
    }

    /// Une chaine vide n'est pas une identification.
    #[test]
    fn les_chaines_vides_ne_comptent_pas_pour_une_identification() {
        let backend = setup();
        backend
            .execute_batch(
                "INSERT INTO albums (id, title, musicbrainz_release_id) VALUES (4, 'Vide', ''); \
                 INSERT INTO tracks (id, title, album_id, musicbrainz_recording_id) \
                   VALUES (40, 'D1', 4, '   ');",
            )
            .unwrap();

        let cleared = clear_album_identification(&backend, 4).unwrap();

        assert!(!cleared.was_identified());
    }

    #[test]
    fn album_inconnu_rend_une_erreur_au_lieu_d_ecrire() {
        let backend = setup();
        assert!(clear_album_identification(&backend, 999).is_err());
        // Et rien n'a bouge.
        assert_eq!(recording_id(&backend, 10), Some("rec-FAUX-1".to_string()));
    }

    #[test]
    fn previous_recording_id_retrouve_le_mbid_d_avant() {
        let backend = setup();
        let cleared = clear_album_identification(&backend, 1).unwrap();
        assert_eq!(cleared.previous_recording_id(10), Some("rec-FAUX-1"));
        assert_eq!(cleared.previous_recording_id(20), None);
    }

    // ---- correspondance des pistes ------------------------------------

    fn mb_track(disc: u32, position: u32, title: &str, rid: Option<&str>) -> MBTrack {
        MBTrack {
            position,
            disc,
            number: None,
            title: title.to_string(),
            length_ms: None,
            recording_id: rid.map(str::to_string),
            artist: None,
        }
    }

    fn local(id: i64, disc: i32, position: i32, title: &str) -> LocalTrack {
        LocalTrack {
            id,
            disc,
            position,
            title: title.to_string(),
        }
    }

    #[test]
    fn correspondance_par_disque_et_rang() {
        let l = vec![
            local(1, 1, 1, "A"),
            local(2, 1, 2, "B"),
            local(3, 2, 1, "C"),
        ];
        let m = vec![
            mb_track(1, 1, "A", Some("r1")),
            mb_track(1, 2, "B", Some("r2")),
            mb_track(2, 1, "C", Some("r3")),
        ];
        assert_eq!(
            map_recording_ids(&l, &m),
            vec![
                (1, "r1".to_string()),
                (2, "r2".to_string()),
                (3, "r3".to_string())
            ]
        );
    }

    /// Le rang prime sur le titre : deux pressages peuvent nommer autrement.
    #[test]
    fn le_rang_prime_sur_le_titre() {
        let l = vec![local(1, 1, 1, "Titre local different")];
        let m = vec![mb_track(1, 1, "Autre titre", Some("r1"))];
        assert_eq!(map_recording_ids(&l, &m), vec![(1, "r1".to_string())]);
    }

    /// Numerotation locale absente — le cas des fichiers mal tagues, donc
    /// exactement ceux qu'on re-identifie : on retombe sur le titre.
    #[test]
    fn repli_par_titre_quand_la_numerotation_manque() {
        let l = vec![
            local(1, 0, 0, "'Round Midnight!"),
            local(2, 0, 0, "So What"),
        ];
        let m = vec![
            mb_track(1, 1, "Round Midnight", Some("r1")),
            mb_track(1, 2, "So What", Some("r2")),
        ];
        assert_eq!(
            map_recording_ids(&l, &m),
            vec![(1, "r1".to_string()), (2, "r2".to_string())]
        );
    }

    /// ⚠ Un titre repete ne doit PAS produire une association au hasard.
    #[test]
    fn un_titre_ambigu_n_associe_rien() {
        let l = vec![local(1, 0, 0, "Reprise"), local(2, 0, 0, "Reprise")];
        let m = vec![
            mb_track(1, 1, "Reprise", Some("r1")),
            mb_track(1, 2, "Reprise", Some("r2")),
        ];
        assert!(map_recording_ids(&l, &m).is_empty());
    }

    /// ⚠ Et l'ambiguïté du côté LOCAL compte autant : deux pistes locales de
    /// même titre face à un seul enregistrement, c'est un tirage au sort
    /// déguisé en correspondance. On préfère n'en désigner aucune.
    #[test]
    fn deux_pistes_locales_de_meme_titre_n_en_designent_aucune() {
        let l = vec![local(1, 0, 0, "Reprise"), local(2, 0, 0, "Reprise")];
        let m = vec![mb_track(1, 1, "Reprise", Some("r1"))];
        assert!(
            map_recording_ids(&l, &m).is_empty(),
            "un MBID a ete attribue a l'une des deux pistes homonymes, au hasard de l'ordre"
        );
    }

    #[test]
    fn une_piste_sans_correspondance_reste_sans_mbid() {
        let l = vec![local(1, 1, 1, "A"), local(2, 1, 9, "Bonus inconnu")];
        let m = vec![mb_track(1, 1, "A", Some("r1"))];
        assert_eq!(map_recording_ids(&l, &m), vec![(1, "r1".to_string())]);
    }

    #[test]
    fn une_piste_du_pressage_sans_mbid_est_ignoree() {
        let l = vec![local(1, 1, 1, "A")];
        let m = vec![mb_track(1, 1, "A", None)];
        assert!(map_recording_ids(&l, &m).is_empty());
    }

    /// Le repli par titre ne doit pas reattribuer un MBID deja pose au rang.
    #[test]
    fn le_repli_ne_vole_pas_un_mbid_deja_attribue() {
        let l = vec![local(1, 1, 1, "A"), local(2, 0, 0, "A bis")];
        let m = vec![mb_track(1, 1, "A bis", Some("r1"))];
        // La piste 1 prend r1 par le rang ; la piste 2 ne peut pas le reprendre.
        assert_eq!(map_recording_ids(&l, &m), vec![(1, "r1".to_string())]);
    }

    // ---- pose de la nouvelle identification ---------------------------

    fn detail(
        label: Option<&str>,
        catalog: Option<&str>,
        date: Option<&str>,
        year: Option<u32>,
    ) -> MBReleaseDetail {
        MBReleaseDetail {
            release_id: "rel-BON".into(),
            title: "Titre MB".into(),
            artist: "Artiste MB".into(),
            date: date.map(str::to_string),
            year,
            country: None,
            label: label.map(str::to_string),
            catalog_number: catalog.map(str::to_string),
            disc_count: 1,
            tracks: Vec::new(),
        }
    }

    /// Les cles sont REMPLACEES — c'est tout l'objet de la manoeuvre.
    #[test]
    fn les_cles_sont_remplacees_meme_si_elles_avaient_une_valeur() {
        let backend = setup();
        // Sans effacement prealable : on verifie bien un remplacement.
        let applied = apply_album_identification(
            &backend,
            1,
            "rel-BON",
            Some("rg-BON"),
            &[(10, "rec-BON-1".to_string())],
            2,
            None,
        )
        .unwrap();

        assert_eq!(applied.tracks_matched, 1);
        assert_eq!(applied.tracks_unmatched, 1);
        assert_eq!(
            album_ids(&backend, 1),
            (Some("rel-BON".to_string()), Some("rg-BON".to_string()))
        );
        assert_eq!(recording_id(&backend, 10), Some("rec-BON-1".to_string()));
    }

    /// ⚠ Et le descriptif, lui, n'est QUE comble.
    #[test]
    fn le_descriptif_est_comble_jamais_ecrase() {
        let backend = setup();
        // L'album 1 a deja label='Label A', year=1971, pas de release_date.
        let applied = apply_album_identification(
            &backend,
            1,
            "rel-BON",
            None,
            &[],
            2,
            Some(&detail(
                Some("Label MB"),
                Some("CAT-MB"),
                Some("1969-08-01"),
                Some(1969),
            )),
        )
        .unwrap();

        let row = backend
            .query_one(
                "SELECT label, catalog_number, release_date, year FROM albums WHERE id = 1",
                &[],
            )
            .unwrap()
            .unwrap();
        assert_eq!(
            row[0].as_string().as_deref(),
            Some("Label A"),
            "label ecrase"
        );
        assert_eq!(
            row[1].as_string().as_deref(),
            Some("CAT-1"),
            "catalogue ecrase"
        );
        assert_eq!(
            row[2].as_string().as_deref(),
            Some("1969-08-01"),
            "le trou n'a pas ete comble"
        );
        assert_eq!(row[3].as_i64(), Some(1971), "annee ecrasee");

        // Et on le DIT : trois champs conserves, la date n'en est pas.
        let mut kept = applied.fields_left_as_is.clone();
        kept.sort();
        assert_eq!(kept, vec!["catalog_number", "label", "year"]);
    }

    /// Un champ que MusicBrainz ne fournit pas n'est pas annonce « conserve ».
    #[test]
    fn un_champ_non_propose_n_est_pas_annonce_conserve() {
        let backend = setup();
        let applied = apply_album_identification(
            &backend,
            1,
            "rel-BON",
            None,
            &[],
            2,
            Some(&detail(None, None, None, None)),
        )
        .unwrap();
        assert!(applied.fields_left_as_is.is_empty());
    }

    /// ⚠ Borne : poser l'identification de l'album 1 ne touche pas l'album 2.
    #[test]
    fn la_pose_est_bornee_a_l_album_vise() {
        let backend = setup();
        apply_album_identification(
            &backend,
            1,
            "rel-BON",
            Some("rg-BON"),
            // Une piste de l'AUTRE album, glissee dans la liste : le
            // `AND album_id = ?` doit la refuser.
            &[
                (10, "rec-BON-1".to_string()),
                (20, "rec-PIRATE".to_string()),
            ],
            2,
            None,
        )
        .unwrap();

        assert_eq!(
            album_ids(&backend, 2),
            (
                Some("rel-TEMOIN".to_string()),
                Some("rg-TEMOIN".to_string())
            )
        );
        assert_eq!(
            recording_id(&backend, 20),
            Some("rec-TEMOIN".to_string()),
            "une piste d'un autre album a ete reecrite"
        );
    }
}
