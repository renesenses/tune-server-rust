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

/// Lire le candidat dans la réponse de `host_streaming_match_track`.
///
/// Forme rendue par l'hôte (#4716) :
/// `{"service": "...", "matched": <StreamTrack>|null, "score": f64,
///   "approximate": bool}`, où `StreamTrack` sérialise son identifiant sous
/// `source_id` (voir `tune_core::streaming::traits`).
pub fn candidat_de_la_reponse(reponse: &serde_json::Value) -> Option<Candidat> {
    let piste = reponse.get("matched")?;
    if piste.is_null() {
        return None;
    }
    Some(Candidat {
        id: piste
            .get("source_id")
            .or_else(|| piste.get("id"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string(),
        titre: piste
            .get("title")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string(),
        artiste: piste
            .get("artist_name")
            .or_else(|| piste.get("artist"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string(),
        duree_ms: piste
            .get("duration_ms")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0),
        score: reponse
            .get("score")
            .and_then(serde_json::Value::as_f64)
            .unwrap_or(0.0),
        approximatif: reponse
            .get("approximate")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
    })
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
