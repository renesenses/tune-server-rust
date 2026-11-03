//! Étape 2 — suggestions sur les PISTES d'un album : l'œuvre et le mouvement,
//! piste par piste (musique classique d'abord).
//!
//! # Où ça s'écrit
//!
//! Le magasin clé-valeur des pistes, `track_metadata`, sous les clés `work`
//! et `movement` — les mêmes noms que sur l'album (`album_metadata`). Aucune
//! colonne de `tracks` n'est touchée : le scan reconstruit une ligne de piste
//! depuis ses balises, il n'efface pas ce magasin.
//!
//! # Ce qui n'est JAMAIS écrasé
//!
//! Aucun écran ne permet encore de saisir à la main l'œuvre ou le mouvement
//! d'une piste : il n'existe donc pas de marqueur d'édition manuelle à lire.
//! La garde est plus stricte que sur l'album : **sur une piste, l'assistant ne
//! remplit que des VIDES**, ou remplace une valeur qu'il a lui-même posée
//! (`track_metadata.source_ia`). Toute autre valeur déjà présente — venue
//! d'un import, d'un service, d'un enrichissement — est tenue :
//! `deja_rempli`. Sur un album identifié par MusicBrainz, ou une piste qui
//! porte un `musicbrainz_recording_id`, un champ rempli est tenu même s'il
//! vient de l'assistant : `musicbrainz`.
//!
//! La garde joue à la suggestion et à l'application, comme pour l'album.
//!
//! # Annulable
//!
//! Une application est une opération du journal de l'ALBUM
//! ([`crate::suggestions::CLE_JOURNAL`]) dont les lignes portent un
//! `piste_id` ; `POST /albums/{id}/annuler` la défait comme les autres.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tune_core::db::backend::DbBackend;
use tune_core::db::track_metadata_repo::TrackMetadataRepo;
use tune_core::db::track_repo::TrackRepo;

use crate::suggestions::{
    self, Ecartee, EtatAlbum, LigneJournal, Operation, Rapport, identifiant_d_operation,
};

/// Les champs de piste que l'assistant peut proposer.
pub const CHAMPS_PISTE: &[&str] = &["work", "movement"];
/// La clé des champs posés par l'assistant, dans `track_metadata`.
pub const CLE_SOURCE_IA: &str = "source_ia";
/// Pistes montrées au modèle, au plus.
pub const PISTES_MAX: usize = 120;

fn non_vide(s: Option<&str>) -> Option<String> {
    s.map(str::trim).filter(|s| !s.is_empty()).map(String::from)
}

/// Une piste, telle que la base la dit.
pub struct EtatPiste {
    pub id: i64,
    pub disque: i32,
    pub numero: i32,
    pub titre: String,
    pub compositeur: Option<String>,
    pub enregistrement_mb: bool,
    pub meta: HashMap<String, String>,
    pub source_ia: BTreeSet<String>,
}

impl EtatPiste {
    pub fn valeur(&self, champ: &str) -> Option<String> {
        non_vide(self.meta.get(champ).map(String::as_str))
    }
}

/// L'album et ses pistes.
pub struct EtatPistes {
    pub album: EtatAlbum,
    pub pistes: Vec<EtatPiste>,
}

fn source_de(meta: &HashMap<String, String>) -> BTreeSet<String> {
    meta.get(CLE_SOURCE_IA)
        .and_then(|v| serde_json::from_str(v).ok())
        .unwrap_or_default()
}

impl EtatPistes {
    pub fn charger(db: &Arc<dyn DbBackend>, album_id: i64) -> Result<Option<Self>, String> {
        let Some(album) = EtatAlbum::charger(db, album_id)? else {
            return Ok(None);
        };
        let meta_repo = TrackMetadataRepo::with_backend(db.clone());
        let mut pistes = Vec::new();
        for t in TrackRepo::with_backend(db.clone())
            .list_by_album(album_id)
            .map_err(|e| e.to_string())?
            .into_iter()
            .take(PISTES_MAX)
        {
            let Some(id) = t.id else { continue };
            let meta = meta_repo.get_all(id)?;
            pistes.push(EtatPiste {
                id,
                disque: t.disc_number,
                numero: t.track_number,
                titre: t.title,
                compositeur: non_vide(t.composer.as_deref()),
                enregistrement_mb: non_vide(t.musicbrainz_recording_id.as_deref()).is_some(),
                source_ia: source_de(&meta),
                meta,
            });
        }
        Ok(Some(Self { album, pistes }))
    }

    pub fn piste(&self, id: i64) -> Option<&EtatPiste> {
        self.pistes.iter().find(|p| p.id == id)
    }

    fn album_identifie(&self) -> bool {
        non_vide(self.album.album.musicbrainz_release_id.as_deref()).is_some()
    }

    /// Pourquoi ce champ de cette piste ne peut pas être écrit, ou `None`.
    pub fn protection(&self, piste: &EtatPiste, champ: &str) -> Option<&'static str> {
        if !CHAMPS_PISTE.contains(&champ) {
            return Some("champ_inconnu");
        }
        piste.valeur(champ)?;
        if self.album_identifie() || piste.enregistrement_mb {
            return Some("musicbrainz");
        }
        if !piste.source_ia.contains(champ) {
            return Some("deja_rempli");
        }
        None
    }

    /// L'album et ses pistes tels que le modèle les lit.
    pub fn contexte(&self) -> Value {
        let a = &self.album;
        json!({
            "album": {
                "title": a.valeur("title"),
                "artist": a.valeur("artist"),
                "composer": a.valeur("composer"),
                "work": a.valeur("work"),
                "genre": a.valeur("genre"),
                "year": a.album.year,
            },
            "tracks": self.pistes.iter().map(|p| json!({
                "id": p.id,
                "disc": p.disque,
                "n": p.numero,
                "title": p.titre,
                "composer": p.compositeur,
                "work": p.valeur("work"),
                "movement": p.valeur("movement"),
            })).collect::<Vec<_>>(),
        })
    }
}

/// La consigne des suggestions de pistes.
pub fn consigne(langue: &str) -> String {
    format!(
        "You help clean up the TRACK metadata of ONE album in a music library, mostly classical music. \
You receive the album and its tracks, each with its id. Reply with ONE JSON object and nothing else:\n\
{{\"tracks\": [{{\"id\": track id (integer, from the input), \"field\": \"work\" | \"movement\", \
\"value\": string, \"confidence\": number between 0 and 1, \"reason\": short string in {langue}}}]}}\n\
- work: the full title of the musical work the track belongs to (e.g. \"Symphony No. 9 in D minor, Op. 125\").\n\
- movement: the movement or part of that work this track is (e.g. \"II. Molto vivace\").\n\
Only suggest values for tracks that are part of a larger work. Never invent facts: when unsure, leave the track \
out or give a low confidence. An empty list is a valid answer."
    )
}

/// Une suggestion de piste validée, avec l'écart avant/après.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SuggestionPiste {
    pub piste_id: i64,
    pub titre: String,
    pub champ: String,
    pub avant: Option<String>,
    pub apres: String,
    pub confiance: f64,
    pub raison: Option<String>,
}

fn valeur_propre(v: Option<&Value>) -> Option<String> {
    v.and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty() && s.chars().count() <= 300)
        .map(String::from)
}

/// Valide la réponse du modèle contre l'état des pistes.
pub fn evaluer(
    brut: &Value,
    etat: &EtatPistes,
) -> Result<(Vec<SuggestionPiste>, Vec<Ecartee>), &'static str> {
    let liste = brut
        .get("tracks")
        .and_then(Value::as_array)
        .ok_or("reponse_invalide")?;
    let mut retenues: Vec<SuggestionPiste> = Vec::new();
    let mut ecartees = Vec::new();
    for s in liste {
        let champ = s.get("field").and_then(Value::as_str).unwrap_or("").trim();
        let valeur = valeur_propre(s.get("value"));
        let id = s.get("id").and_then(Value::as_i64);
        let mut ecarter = |raison| {
            ecartees.push(Ecartee {
                champ: champ.to_string(),
                valeur: valeur.clone(),
                raison,
                piste_id: id,
            })
        };
        let Some(piste) = id.and_then(|i| etat.piste(i)) else {
            ecarter("piste_inconnue");
            continue;
        };
        if !CHAMPS_PISTE.contains(&champ) {
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
        let avant = piste.valeur(champ);
        if avant.as_deref() == Some(apres.as_str()) {
            ecarter("identique");
            continue;
        }
        if let Some(raison) = etat.protection(piste, champ) {
            ecarter(raison);
            continue;
        }
        let sugg = SuggestionPiste {
            piste_id: piste.id,
            titre: piste.titre.clone(),
            champ: champ.to_string(),
            avant,
            apres,
            confiance: confiance.clamp(0.0, 1.0),
            raison: s
                .get("reason")
                .and_then(Value::as_str)
                .map(|r| r.trim().chars().take(300).collect()),
        };
        match retenues
            .iter_mut()
            .find(|r| r.piste_id == sugg.piste_id && r.champ == sugg.champ)
        {
            Some(r) if r.confiance < sugg.confiance => *r = sugg,
            Some(_) => {}
            None => retenues.push(sugg),
        }
    }
    Ok((retenues, ecartees))
}

/// Une ligne de piste ACCEPTÉE.
#[derive(Clone, Debug, Deserialize)]
pub struct LignePiste {
    #[serde(alias = "track_id")]
    pub piste_id: i64,
    #[serde(alias = "field")]
    pub champ: String,
    #[serde(alias = "value")]
    pub valeur: String,
}

fn ranger_source(
    repo: &TrackMetadataRepo,
    piste_id: i64,
    source: &BTreeSet<String>,
) -> Result<(), String> {
    if source.is_empty() {
        repo.delete(piste_id, CLE_SOURCE_IA)
    } else {
        repo.set(
            piste_id,
            CLE_SOURCE_IA,
            &serde_json::to_string(source).map_err(|e| e.to_string())?,
        )
    }
}

/// Applique les lignes acceptées. La garde est rejouée sur l'état présent.
pub fn appliquer(
    db: &Arc<dyn DbBackend>,
    album_id: i64,
    lignes: &[LignePiste],
) -> Result<Option<Rapport>, String> {
    let Some(etat) = EtatPistes::charger(db, album_id)? else {
        return Ok(None);
    };
    let repo = TrackMetadataRepo::with_backend(db.clone());
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
                piste_id: Some(l.piste_id),
            })
        };
        let Some(piste) = etat.piste(l.piste_id) else {
            refuser("piste_inconnue");
            continue;
        };
        if !CHAMPS_PISTE.contains(&champ) {
            refuser("champ_inconnu");
            continue;
        }
        if valeur.is_empty() || valeur.chars().count() > 300 {
            refuser("valeur_invalide");
            continue;
        }
        if !vus.insert((l.piste_id, champ.to_string())) {
            refuser("doublon");
            continue;
        }
        let avant = piste.valeur(champ);
        if avant.as_deref() == Some(valeur) {
            refuser("identique");
            continue;
        }
        if let Some(raison) = etat.protection(piste, champ) {
            refuser(raison);
            continue;
        }
        repo.set(piste.id, champ, valeur)?;
        let mut source = source_de(&repo.get_all(piste.id)?);
        source.insert(champ.to_string());
        ranger_source(&repo, piste.id, &source)?;
        rapport.appliquees.push(LigneJournal {
            champ: champ.to_string(),
            avant,
            apres: valeur.to_string(),
            piste_id: Some(piste.id),
        });
    }
    if !rapport.appliquees.is_empty() {
        let id = identifiant_d_operation();
        let mut ops = suggestions::journal(db, album_id)?;
        ops.push(Operation {
            id: id.clone(),
            horodatage: chrono::Utc::now().to_rfc3339(),
            lignes: rapport.appliquees.clone(),
            annulee: false,
            genre_pistes: None,
        });
        suggestions::ranger_journal(db, album_id, &ops)?;
        rapport.operation_id = Some(id);
    }
    Ok(Some(rapport))
}

/// Défait UNE ligne de piste. `Ok(Err(présent))` : la valeur a changé depuis,
/// elle n'est pas touchée.
pub(crate) fn defaire_ligne(
    db: &Arc<dyn DbBackend>,
    piste_id: i64,
    l: &LigneJournal,
) -> Result<Result<(), Option<String>>, String> {
    let repo = TrackMetadataRepo::with_backend(db.clone());
    let meta = repo.get_all(piste_id)?;
    let present = non_vide(meta.get(&l.champ).map(String::as_str));
    if present.as_deref() != Some(l.apres.as_str()) {
        return Ok(Err(present));
    }
    match &l.avant {
        Some(v) => repo.set(piste_id, &l.champ, v)?,
        None => repo.delete(piste_id, &l.champ)?,
    }
    let mut source = source_de(&meta);
    source.remove(&l.champ);
    ranger_source(&repo, piste_id, &source)?;
    Ok(Ok(()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::suggestions::tests::base;

    fn sugg(id: i64, champ: &str, valeur: &str) -> Value {
        json!({"id": id, "field": champ, "value": valeur, "confidence": 0.9, "reason": "r"})
    }

    #[test]
    fn une_piste_deja_remplie_n_est_jamais_proposee_ni_ecrite() {
        let db = base();
        // Piste 1 (album 1) : une œuvre venue d'ailleurs (import).
        TrackMetadataRepo::with_backend(db.clone())
            .set(1, "work", "Symphonie n° 9")
            .unwrap();
        let etat = EtatPistes::charger(&db, 1).unwrap().unwrap();
        let (retenues, ecartees) = evaluer(
            &json!({"tracks": [
                sugg(1, "work", "Symphony No. 9 in D minor, Op. 125"),
                sugg(1, "movement", "I. Allegro ma non troppo"),
                sugg(999, "movement", "II."),
                sugg(1, "mood", "épique"),
            ]}),
            &etat,
        )
        .unwrap();
        assert_eq!(
            retenues
                .iter()
                .map(|s| (s.piste_id, s.champ.as_str()))
                .collect::<Vec<_>>(),
            vec![(1, "movement")]
        );
        let raisons: Vec<(&str, &str)> = ecartees
            .iter()
            .map(|e| (e.champ.as_str(), e.raison))
            .collect();
        assert_eq!(
            raisons,
            vec![
                ("work", "deja_rempli"),
                ("movement", "piste_inconnue"),
                ("mood", "champ_inconnu"),
            ]
        );
        // Et à l'application, même si l'écran l'envoie.
        let r = appliquer(
            &db,
            1,
            &[LignePiste {
                piste_id: 1,
                champ: "work".into(),
                valeur: "Autre".into(),
            }],
        )
        .unwrap()
        .unwrap();
        assert!(r.appliquees.is_empty());
        assert_eq!(r.refusees[0].raison, "deja_rempli");
        assert_eq!(
            TrackMetadataRepo::with_backend(db.clone())
                .get_all(1)
                .unwrap()
                .get("work")
                .map(String::as_str),
            Some("Symphonie n° 9")
        );
    }

    #[test]
    fn une_piste_d_un_album_musicbrainz_garde_sa_valeur_meme_posee_par_l_ia() {
        let db = base();
        // Album 2 est identifié par MusicBrainz ; on y range une piste.
        db.execute_batch(
            "INSERT INTO tracks (id, title, album_id, artist_id, file_path) VALUES (20, 'So What', 2, 2, '/m/20.flac');",
        )
        .unwrap();
        let repo = TrackMetadataRepo::with_backend(db.clone());
        repo.set(20, "work", "Kind of Blue").unwrap();
        repo.set(20, CLE_SOURCE_IA, "[\"work\"]").unwrap();
        let etat = EtatPistes::charger(&db, 2).unwrap().unwrap();
        let (retenues, ecartees) = evaluer(
            &json!({"tracks": [sugg(20, "work", "Autre"), sugg(20, "movement", "I.")]}),
            &etat,
        )
        .unwrap();
        assert_eq!(retenues.len(), 1, "remplir un vide reste permis");
        assert_eq!(ecartees[0].raison, "musicbrainz");
    }

    #[test]
    fn appliquer_puis_annuler_une_ligne_de_piste() {
        let db = base();
        let r = appliquer(
            &db,
            1,
            &[LignePiste {
                piste_id: 1,
                champ: "movement".into(),
                valeur: "I. Allegro ma non troppo".into(),
            }],
        )
        .unwrap()
        .unwrap();
        assert_eq!(r.appliquees.len(), 1);
        let repo = TrackMetadataRepo::with_backend(db.clone());
        let meta = repo.get_all(1).unwrap();
        assert_eq!(
            meta.get("movement").map(String::as_str),
            Some("I. Allegro ma non troppo")
        );
        assert_eq!(
            meta.get(CLE_SOURCE_IA).map(String::as_str),
            Some("[\"movement\"]")
        );

        let a = suggestions::annuler(&db, 1, None).unwrap().unwrap();
        assert_eq!(a.appliquees.len(), 1);
        assert_eq!(a.appliquees[0].piste_id, Some(1));
        let meta = repo.get_all(1).unwrap();
        assert!(!meta.contains_key("movement"));
        assert!(!meta.contains_key(CLE_SOURCE_IA));
    }
}
