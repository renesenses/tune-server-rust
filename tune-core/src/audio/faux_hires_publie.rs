//! Le verdict des faux hi-res tel qu'il se PUBLIE (#6285, deuxième étape) :
//! dans la liste des albums, son filtre `?hires_verdict=` et le critère
//! `hires_verdict` des règles intelligentes.
//!
//! La mesure et le verdict de chaque piste sont ceux de la première étape
//! ([`super::faux_hires`], [`super::faux_hires_passe`]) ; le verdict d'un
//! album suit [`verdict_d_album`]. Ce module ne mesure rien : il dit ce qu'on
//! en montre, et l'écrit en SQL pour qu'un filtre rende exactement les albums
//! qui portent le badge.
//!
//! # « Douteux » masqué par défaut
//!
//! L'issue demande de mesurer le taux de faux positifs sur une vraie
//! bibliothèque AVANT de montrer un badge « douteux ». Tant que le réglage
//! [`REGLAGE_DOUTEUX_VISIBLE`] ne vaut pas `true`, un album (ou une piste)
//! douteux se publie comme `non_analyse` : aucun badge, et il ne se distingue
//! pas d'un album pas encore mesuré — ni dans la liste, ni par le filtre, ni
//! par une règle, ni sur la fiche `GET /library/albums/{id}/hires`. Le
//! verdict reste en base, intact : poser le réglage le montre partout.

use std::sync::Arc;

use super::faux_hires_passe::verdict_d_album;
use crate::db::backend::DbBackend;
use crate::db::settings_repo::SettingsRepo;

/// Le réglage qui montre le verdict « douteux » (`settings`). Absent ou autre
/// que `true` : il est masqué.
pub const REGLAGE_DOUTEUX_VISIBLE: &str = "faux_hires_douteux_visible";

/// Le verdict « douteux » est-il montré ?
pub fn douteux_visible(db: &Arc<dyn DbBackend>) -> bool {
    SettingsRepo::with_backend(db.clone())
        .get(REGLAGE_DOUTEUX_VISIBLE)
        .ok()
        .flatten()
        .is_some_and(|v| v.trim() == "true")
}

/// Montre ou masque le verdict « douteux ».
pub fn montrer_douteux(db: &Arc<dyn DbBackend>, visible: bool) -> Result<(), String> {
    SettingsRepo::with_backend(db.clone()).set(
        REGLAGE_DOUTEUX_VISIBLE,
        if visible { "true" } else { "false" },
    )
}

/// Le prédicat de « douteux » tant qu'il est masqué : constamment faux, et
/// distinct du `1 = 0` qu'écrivent les règles intelligentes pour une règle
/// qu'elles ne savent pas traduire.
pub const DOUTEUX_MASQUE: &str = "0 = 1";

/// Un verdict publié : ce que lit le badge, ce que choisit le filtre.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerdictPublie {
    Verifie,
    Conforme,
    Douteux,
    /// Aucune mesure, rien de concluant, ou un « douteux » masqué.
    NonAnalyse,
}

impl VerdictPublie {
    /// Le mot d'un filtre ou d'une règle ; `None` pour un mot inconnu.
    pub fn lire(mot: &str) -> Option<Self> {
        match mot.trim().to_ascii_lowercase().as_str() {
            "verifie" => Some(Self::Verifie),
            "conforme" => Some(Self::Conforme),
            "douteux" => Some(Self::Douteux),
            "non_analyse" => Some(Self::NonAnalyse),
            _ => None,
        }
    }

    pub fn code(self) -> &'static str {
        match self {
            Self::Verifie => "verifie",
            Self::Conforme => "conforme",
            Self::Douteux => "douteux",
            Self::NonAnalyse => "non_analyse",
        }
    }
}

/// Le verdict d'un album et sa raison, tels qu'ils se publient, depuis les
/// verdicts de ses pistes mesurées ([`verdict_d_album`]).
pub fn publier_l_album(
    pistes: &[(String, Option<String>)],
    douteux_visible: bool,
) -> (&'static str, Option<String>) {
    let (verdict, raison) = verdict_d_album(pistes);
    match VerdictPublie::lire(&verdict) {
        Some(VerdictPublie::Douteux) if douteux_visible => ("douteux", raison),
        Some(VerdictPublie::Douteux) | Some(VerdictPublie::NonAnalyse) | None => {
            ("non_analyse", None)
        }
        Some(v) => (v.code(), None),
    }
}

/// Verdict et raison de chaque piste mesurée, rangés par identifiant d'album.
pub type PistesMesureesParAlbum = std::collections::HashMap<i64, Vec<(String, Option<String>)>>;

/// Les verdicts des pistes mesurées de chaque album de `ids`, en une requête
/// groupée par tranche (même forme que `dynamic_range_by_ids`). Un album sans
/// aucune mesure n'a pas d'entrée.
pub fn pistes_mesurees_par_album(
    db: &Arc<dyn DbBackend>,
    ids: &[i64],
) -> Result<PistesMesureesParAlbum, crate::TuneError> {
    let mut par_album: PistesMesureesParAlbum = Default::default();
    for tranche in ids.chunks(5000) {
        let liste = tranche
            .iter()
            .map(|id| id.to_string())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT ht.album_id, hv.verdict, hv.raison \
               FROM tracks ht JOIN hires_verifications hv ON hv.track_id = ht.id \
              WHERE ht.album_id IN ({liste}) ORDER BY ht.album_id, hv.track_id"
        );
        for r in &db.query_many(&sql, &[])? {
            let (Some(album), Some(verdict)) = (
                r.first().and_then(|v| v.as_i64()),
                r.get(1).and_then(|v| v.as_string()),
            ) else {
                continue;
            };
            let raison = r.get(2).and_then(|v| v.as_string());
            par_album.entry(album).or_default().push((verdict, raison));
        }
    }
    Ok(par_album)
}

/// Une piste de l'album `id_album` porte ce verdict mesuré.
fn une_piste_de_l_album(id_album: &str, verdict: &str) -> String {
    format!(
        "EXISTS (SELECT 1 FROM tracks ht JOIN hires_verifications hv ON hv.track_id = ht.id \
         WHERE ht.album_id = {id_album} AND hv.verdict = '{verdict}')"
    )
}

/// Le prédicat SQL « l'album `id_album` (une colonne : `a.id`, `al.id`) se
/// PUBLIE avec ce verdict ». Il suit [`verdict_d_album`] règle pour règle :
///
/// - douteux : une piste douteuse au moins ;
/// - vérifié : aucune douteuse, une vérifiée au moins, aucune conforme ;
/// - conforme : aucune douteuse, une conforme au moins ;
/// - non analysé : aucune douteuse, vérifiée ni conforme (pas de mesure, ou
///   rien que du non concluant).
///
/// « Douteux » masqué : le prédicat douteux est faux, et un album douteux
/// répond à « non analysé ». Les deux bases lisent ce SQL tel quel.
pub fn predicat_album(id_album: &str, verdict: VerdictPublie, douteux_visible: bool) -> String {
    let d = une_piste_de_l_album(id_album, "douteux");
    let v = une_piste_de_l_album(id_album, "verifie");
    let c = une_piste_de_l_album(id_album, "conforme");
    match verdict {
        VerdictPublie::Douteux if douteux_visible => d,
        VerdictPublie::Douteux => DOUTEUX_MASQUE.to_string(),
        VerdictPublie::Verifie => format!("(NOT {d} AND {v} AND NOT {c})"),
        VerdictPublie::Conforme => format!("(NOT {d} AND {c})"),
        VerdictPublie::NonAnalyse if douteux_visible => {
            format!("(NOT {d} AND NOT {v} AND NOT {c})")
        }
        VerdictPublie::NonAnalyse => format!("({d} OR (NOT {v} AND NOT {c}))"),
    }
}

/// Le filtre `?hires_verdict=` de la liste des albums.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FiltreHires {
    /// `None` : un mot inconnu, qui ne rend RIEN plutôt que tout.
    pub verdict: Option<VerdictPublie>,
    pub douteux_visible: bool,
}

impl FiltreHires {
    pub fn depuis_le_mot(mot: &str, douteux_visible: bool) -> Self {
        Self {
            verdict: VerdictPublie::lire(mot),
            douteux_visible,
        }
    }

    /// Le prédicat sur la colonne d'identifiant d'album `id_album`.
    pub fn predicat(&self, id_album: &str) -> String {
        match self.verdict {
            Some(v) => predicat_album(id_album, v, self.douteux_visible),
            None => "1 = 0".to_string(),
        }
    }
}

/// Le prédicat SQL « la piste `id_piste` (`t.id`) se PUBLIE avec ce
/// verdict » : celui de sa ligne de `hires_verifications`. Sans ligne, ou
/// `non_concluant`, elle est non analysée ; douteuse masquée aussi.
pub fn predicat_piste(id_piste: &str, verdict: VerdictPublie, douteux_visible: bool) -> String {
    let parmi = |verdicts: &str| {
        format!(
            "EXISTS (SELECT 1 FROM hires_verifications hv \
             WHERE hv.track_id = {id_piste} AND hv.verdict IN ({verdicts}))"
        )
    };
    match verdict {
        VerdictPublie::Verifie => parmi("'verifie'"),
        VerdictPublie::Conforme => parmi("'conforme'"),
        VerdictPublie::Douteux if douteux_visible => parmi("'douteux'"),
        VerdictPublie::Douteux => DOUTEUX_MASQUE.to_string(),
        VerdictPublie::NonAnalyse if douteux_visible => {
            format!("NOT {}", parmi("'verifie', 'conforme', 'douteux'"))
        }
        VerdictPublie::NonAnalyse => format!("NOT {}", parmi("'verifie', 'conforme'")),
    }
}

#[cfg(test)]
#[path = "faux_hires_publie_tests_6285.rs"]
mod tests;
