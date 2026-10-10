//! La règle d'appariement — décision de Bertrand du 22/09/2026.
//!
//! > Quand l'identifiant de service ne correspond pas : **titre + artiste +
//! > durée à ±3 secondes**. Les trois doivent concorder. Ce qui ne concorde pas
//! > est déclaré **introuvable**, avec sa raison ; on ne devine pas.
//!
//! ## Ce que ce module écrit, et ce qu'il n'écrit PAS
//!
//! Il n'y a **aucun appariement de titre ou d'artiste ici**, et c'est
//! volontaire : l'épique (#4715) pose que « l'appariement écrit pour la fusion
//! (`rassembler_les_sources`, `best_stream_match`) est réutilisé, jamais
//! réécrit ». Le verdict titre+artiste reste donc celui de l'hôte —
//! `host_streaming_match_track`, qui est l'extraction littérale de ce que fait
//! la route de transfert (voir `tune_core::streaming::matching`). Le greffon
//! le lit dans le champ `approximate` que l'hôte rend avec le candidat :
//!
//! * `approximate == false` ⇔ score ≥ `MATCH_ACCEPT_SCORE` ⇒ **titre et
//!   artiste concordent**, au sens du seul appariement du projet ;
//! * `approximate == true` ⇒ le flou a trouvé quelque chose sans certitude.
//!   Deux des trois critères ne sont pas tenus : c'est **introuvable**.
//!
//! Ce module ajoute le **troisième critère**, celui que la décision du 22/09
//! introduit et que personne ne vérifiait : la durée. Il ne demande aucune
//! normalisation, donc il ne duplique rien — c'est une soustraction et une
//! comparaison.
//!
//! ## Pourquoi la durée inconnue vaut « introuvable »
//!
//! « Les trois doivent concorder. » Une durée absente (0 ms) des deux côtés
//! n'est pas une concordance : c'est une vérification qu'on n'a pas pu faire.
//! La traiter comme un succès reviendrait à transférer sur deux critères en
//! prétendant en avoir tenu trois — exactement le faux transfert que la
//! décision veut éviter. Elle sort donc en introuvable, avec sa raison, et
//! l'utilisateur voit pourquoi.
//!
//! ## Le remaster est un manque VOULU
//!
//! Le matcher partagé retire « (Remastered …) » du titre, si bien qu'un
//! remaster peut passer le critère titre+artiste. Sa durée, elle, diffère
//! presque toujours de plus de trois secondes : il ressort en
//! `duree_hors_tolerance`, avec l'écart mesuré. C'est le comportement demandé
//! — « mieux vaut un manque qu'un faux transfert ».

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::hote::Hote;
use crate::snapshots::LOCAL;

/// Tolérance sur la durée, en millisecondes. **±3 secondes** (Bertrand,
/// 22/09/2026). La borne est INCLUSIVE : 3000 ms d'écart concordent, 3001 non.
pub const TOLERANCE_DUREE_MS: u64 = 3_000;

/// Pourquoi un titre n'a pas été transféré.
///
/// Sérialisé en `{"code": "...", ...}` : le code est stable pour l'écran et
/// pour les essais, les champs qui l'accompagnent sont ce qu'il faut pour
/// comprendre sans rouvrir le service.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "code", rename_all = "snake_case")]
pub enum Raison {
    /// Le service n'a rien rendu du tout pour ce titre.
    AucunResultat,
    /// Le service a rendu un candidat, mais titre et artiste ne concordent pas
    /// avec assez de certitude (score sous le seuil d'acceptation du projet).
    AppariementApproximatif {
        candidat_titre: String,
        candidat_artiste: String,
        score: f64,
    },
    /// Titre et artiste concordent, la durée non.
    DureeHorsTolerance {
        candidat_titre: String,
        candidat_artiste: String,
        duree_source_ms: u64,
        duree_candidat_ms: u64,
        ecart_ms: u64,
    },
    /// Une des deux durées manque : le troisième critère n'a pas pu être
    /// vérifié, donc il n'est pas tenu.
    DureeInconnue {
        candidat_titre: String,
        candidat_artiste: String,
        duree_source_ms: u64,
        duree_candidat_ms: u64,
    },
    /// L'appel à l'hôte a échoué (service indisponible, jeton expiré…). La
    /// piste n'est pas déclarée absente du service : elle est déclarée non
    /// vérifiable, et le message de l'hôte est conservé.
    ServiceEnErreur { message: String },
}

impl Raison {
    /// Le code stable, sans le détail. Pratique pour un résumé ou un essai.
    pub fn code(&self) -> &'static str {
        match self {
            Raison::AucunResultat => "aucun_resultat",
            Raison::AppariementApproximatif { .. } => "appariement_approximatif",
            Raison::DureeHorsTolerance { .. } => "duree_hors_tolerance",
            Raison::DureeInconnue { .. } => "duree_inconnue",
            Raison::ServiceEnErreur { .. } => "service_en_erreur",
        }
    }
}

/// Le candidat que l'hôte a rendu, réduit à ce dont la règle a besoin.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidat {
    pub id: String,
    pub titre: String,
    pub artiste: String,
    pub duree_ms: u64,
    pub score: f64,
    /// Verdict titre+artiste de l'hôte : `true` = sous le seuil d'acceptation.
    pub approximatif: bool,
}

/// Le verdict de la règle sur un candidat, pour une durée source donnée.
///
/// `Ok(candidat)` ⇒ les trois critères concordent. `Err(raison)` ⇒ introuvable,
/// et la raison dit lequel a manqué.
pub fn juger(duree_source_ms: u64, candidat: Option<Candidat>) -> Result<Candidat, Raison> {
    let Some(c) = candidat else {
        return Err(Raison::AucunResultat);
    };

    // Critères 1 et 2 — titre et artiste. Le verdict n'est pas recalculé ici :
    // c'est celui du matcher partagé, relayé par l'hôte.
    if c.approximatif {
        return Err(Raison::AppariementApproximatif {
            candidat_titre: c.titre,
            candidat_artiste: c.artiste,
            score: c.score,
        });
    }

    // Critère 3 — la durée. Une durée manquante n'est pas une concordance.
    if duree_source_ms == 0 || c.duree_ms == 0 {
        return Err(Raison::DureeInconnue {
            candidat_titre: c.titre,
            candidat_artiste: c.artiste,
            duree_source_ms,
            duree_candidat_ms: c.duree_ms,
        });
    }
    let ecart_ms = duree_source_ms.abs_diff(c.duree_ms);
    if ecart_ms > TOLERANCE_DUREE_MS {
        return Err(Raison::DureeHorsTolerance {
            candidat_titre: c.titre,
            candidat_artiste: c.artiste,
            duree_source_ms,
            duree_candidat_ms: c.duree_ms,
            ecart_ms,
        });
    }

    Ok(c)
}

/// Lire le candidat de TÊTE dans la réponse de `host_streaming_match_track`.
///
/// Forme rendue par l'hôte (#4716) :
/// `{"service": "...", "matched": <StreamTrack>|null, "score": f64,
///   "approximate": bool}`, où `StreamTrack` sérialise son identifiant sous
/// `source_id` (voir `tune_core::streaming::traits`).
pub fn candidat_de_la_reponse(reponse: &Value) -> Option<Candidat> {
    let piste = reponse.get("matched")?;
    if piste.is_null() {
        return None;
    }
    Some(candidat_de_la_piste(
        piste,
        reponse.get("score").and_then(Value::as_f64).unwrap_or(0.0),
        reponse
            .get("approximate")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        false,
    ))
}

/// Une piste rendue par l'hôte, réduite à ce que la règle juge.
///
/// `local` : en bibliothèque, l'identifiant qui compte est l'entier
/// `track_id` ; `source_id` y désigne, s'il existe, l'ORIGINE streaming de la
/// piste — l'écrire dans une playlist locale n'aurait aucun sens.
fn candidat_de_la_piste(piste: &Value, score: f64, approximatif: bool, local: bool) -> Candidat {
    let texte = |cles: &[&str]| {
        cles.iter()
            .find_map(|c| piste.get(*c).and_then(Value::as_str))
            .unwrap_or_default()
            .to_string()
    };
    let id_local = if local {
        piste
            .get("track_id")
            .and_then(Value::as_i64)
            .map(|n| n.to_string())
    } else {
        None
    };
    Candidat {
        id: id_local.unwrap_or_else(|| texte(&["source_id", "id"])),
        titre: texte(&["title"]),
        artiste: texte(&["artist_name", "artist"]),
        duree_ms: piste
            .get("duration_ms")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        score,
        approximatif,
    }
}

/// Un ISRC comparable : sans tirets ni espaces, en capitales — la règle de
/// `tune_core::library::track_matcher::normaliser_isrc`, que le greffon ne peut
/// pas importer (il ne dépend pas de `tune-core`). Les services ne
/// l'écrivent pas tous de la même façon (`GB-AYE-69-00001` / `gbaye6900001`).
fn isrc_normalise(isrc: &str) -> String {
    isrc.chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_uppercase())
        .collect()
}

/// Le classement complet rendu par l'hôte, verdict en tête, chaque candidat
/// avec son ISRC normalisé.
///
/// L'hôte rend `candidates` depuis #4716 (`[{track, score, approximate}]`) ;
/// une réponse sans ce champ (hôte plus ancien) se lit sur `matched` seul.
fn classement(reponse: &Value, local: bool) -> Vec<(Candidat, String)> {
    let isrc_de = |piste: &Value| {
        isrc_normalise(
            piste
                .get("isrc")
                .and_then(Value::as_str)
                .unwrap_or_default(),
        )
    };
    let liste = reponse
        .get("candidates")
        .and_then(Value::as_array)
        .filter(|l| !l.is_empty());
    match liste {
        Some(liste) => liste
            .iter()
            .filter_map(|entree| {
                let piste = entree.get("track").filter(|p| !p.is_null())?;
                let score = entree.get("score").and_then(Value::as_f64).unwrap_or(0.0);
                let approximatif = entree
                    .get("approximate")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                Some((
                    candidat_de_la_piste(piste, score, approximatif, local),
                    isrc_de(piste),
                ))
            })
            .collect(),
        None => match reponse.get("matched").filter(|p| !p.is_null()) {
            Some(piste) => {
                let mut c = candidat_de_la_reponse(reponse).expect("matched non nul");
                if local {
                    c = candidat_de_la_piste(piste, c.score, c.approximatif, true);
                }
                vec![(c, isrc_de(piste))]
            }
            None => Vec::new(),
        },
    }
}

/// Choisir, dans la réponse de l'hôte, le candidat à transférer (#4741).
///
/// L'ordre est celui de l'épique :
///
/// 1. **L'ISRC.** Un candidat dont l'ISRC est celui de la source désigne le
///    même enregistrement : le flou du titre (une autre langue, une autre
///    graphie) ne l'écarte pas. La **durée** reste exigée — la décision du
///    22/09 veut les trois critères, et un ISRC mal saisi chez un service
///    existe ; un écart de plus de 3 s le fait retomber dans l'étape 2.
/// 2. **Titre + artiste + durée.** Le premier candidat du classement qui tient
///    les trois, et pas seulement le premier tout court : quand le verdict de
///    tête rate la durée (un remaster), le suivant peut être la bonne édition.
/// 3. Sinon **introuvable**, avec la raison du verdict de TÊTE — c'est lui que
///    l'utilisateur reconnaîtra en lisant le rapport.
pub fn choisir(
    duree_source_ms: u64,
    isrc_source: &str,
    reponse: &Value,
    local: bool,
) -> Result<Candidat, Raison> {
    let classement = classement(reponse, local);

    let isrc = isrc_normalise(isrc_source);
    if !isrc.is_empty() {
        for (candidat, isrc_candidat) in &classement {
            if *isrc_candidat == isrc {
                let mut c = candidat.clone();
                c.approximatif = false;
                if let Ok(c) = juger(duree_source_ms, Some(c)) {
                    return Ok(c);
                }
            }
        }
    }

    for (candidat, _) in &classement {
        if let Ok(c) = juger(duree_source_ms, Some(candidat.clone())) {
            return Ok(c);
        }
    }

    juger(
        duree_source_ms,
        classement.into_iter().next().map(|(c, _)| c),
    )
}

/// Apparier un titre CHEZ un service, ou dans la bibliothèque locale.
///
/// Le seul chemin d'appariement du greffon : le transfert (#4717) et les liens
/// (#4719) passent tous deux par ici. Une erreur de l'hôte devient une RAISON,
/// pas un arrêt : le reste de la playlist s'apparie quand même.
pub fn apparier_chez<H: Hote + ?Sized>(
    hote: &H,
    service: &str,
    titre: &str,
    artiste: &str,
    isrc: &str,
    duree_ms: u64,
) -> Result<Candidat, Raison> {
    let local = service == LOCAL;
    let reponse = if local {
        hote.library_match_track(titre, artiste, isrc, duree_ms)
    } else {
        hote.streaming_match_track(service, titre, artiste, isrc, duree_ms)
    }
    .map_err(|message| Raison::ServiceEnErreur { message })?;
    choisir(duree_ms, isrc, &reponse, local)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidat(duree_ms: u64) -> Candidat {
        Candidat {
            id: "t-1".into(),
            titre: "Imagine".into(),
            artiste: "John Lennon".into(),
            duree_ms,
            score: 0.95,
            approximatif: false,
        }
    }

    #[test]
    fn les_trois_criteres_concordent() {
        let v = juger(183_000, Some(candidat(184_000)));
        assert_eq!(v.unwrap().id, "t-1");
    }

    /// La borne est inclusive des deux côtés : c'est « ±3 secondes », pas
    /// « moins de 3 secondes ».
    #[test]
    fn trois_secondes_pile_concordent_dans_les_deux_sens() {
        assert!(juger(183_000, Some(candidat(186_000))).is_ok());
        assert!(juger(183_000, Some(candidat(180_000))).is_ok());
    }

    /// Une milliseconde de plus et c'est introuvable — la contre-épreuve de la
    /// borne. Sans elle, un `>=` à la place d'un `>` passerait inaperçu.
    #[test]
    fn trois_secondes_et_une_milliseconde_sont_introuvables() {
        let erreur = juger(183_000, Some(candidat(186_001))).unwrap_err();
        assert_eq!(erreur.code(), "duree_hors_tolerance");
        match erreur {
            Raison::DureeHorsTolerance { ecart_ms, .. } => assert_eq!(ecart_ms, 3_001),
            autre => panic!("raison inattendue : {autre:?}"),
        }
    }

    /// Le cas que Bertrand annonce comme voulu : le remaster a le même titre
    /// une fois « (Remastered) » retiré par le matcher partagé, mais 9 s de
    /// plus. Il sort en introuvable, avec l'écart mesuré.
    #[test]
    fn un_remaster_sort_en_introuvable_avec_son_ecart() {
        let erreur = juger(183_000, Some(candidat(192_000))).unwrap_err();
        match erreur {
            Raison::DureeHorsTolerance {
                ecart_ms,
                duree_candidat_ms,
                ..
            } => {
                assert_eq!(ecart_ms, 9_000);
                assert_eq!(duree_candidat_ms, 192_000);
            }
            autre => panic!("raison inattendue : {autre:?}"),
        }
    }

    #[test]
    fn un_appariement_approximatif_ne_se_transfere_pas() {
        let mut c = candidat(183_000);
        c.approximatif = true;
        c.score = 0.62;
        let erreur = juger(183_000, Some(c)).unwrap_err();
        assert_eq!(erreur.code(), "appariement_approximatif");
    }

    /// Le flou passe AVANT la durée : un candidat douteux dont la durée colle
    /// reste douteux. Deux critères sur trois ne suffisent pas.
    #[test]
    fn le_flou_prime_sur_une_duree_parfaite() {
        let mut c = candidat(183_000);
        c.approximatif = true;
        assert_eq!(
            juger(183_000, Some(c)).unwrap_err().code(),
            "appariement_approximatif"
        );
    }

    #[test]
    fn duree_absente_dun_cote_vaut_introuvable() {
        assert_eq!(
            juger(0, Some(candidat(183_000))).unwrap_err().code(),
            "duree_inconnue"
        );
        assert_eq!(
            juger(183_000, Some(candidat(0))).unwrap_err().code(),
            "duree_inconnue"
        );
    }

    #[test]
    fn aucun_candidat_vaut_aucun_resultat() {
        assert_eq!(juger(183_000, None).unwrap_err().code(), "aucun_resultat");
    }

    #[test]
    fn la_reponse_de_l_hote_se_relit_sous_source_id() {
        let reponse = serde_json::json!({
            "service": "qobuz",
            "matched": {
                "source_id": "q-42",
                "title": "Imagine",
                "artist_name": "John Lennon",
                "duration_ms": 183_000u64,
            },
            "score": 0.95,
            "approximate": false,
        });
        let c = candidat_de_la_reponse(&reponse).expect("un candidat");
        assert_eq!(c.id, "q-42");
        assert_eq!(c.duree_ms, 183_000);
        assert!(!c.approximatif);
    }

    #[test]
    fn matched_null_ne_rend_aucun_candidat() {
        let reponse = serde_json::json!({ "service": "qobuz", "matched": null });
        assert!(candidat_de_la_reponse(&reponse).is_none());
    }

    /// La raison se sérialise avec son code : c'est ce que l'écran lit.
    #[test]
    fn la_raison_porte_son_code_en_json() {
        let json = serde_json::to_value(Raison::DureeHorsTolerance {
            candidat_titre: "Imagine".into(),
            candidat_artiste: "John Lennon".into(),
            duree_source_ms: 183_000,
            duree_candidat_ms: 192_000,
            ecart_ms: 9_000,
        })
        .unwrap();
        assert_eq!(json["code"], "duree_hors_tolerance");
        assert_eq!(json["ecart_ms"], 9_000);
    }
}
