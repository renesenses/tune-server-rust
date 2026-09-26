//! L'ordre des passes qui décodent : où se place la plage dynamique (#5169).
//!
//! ## Le constat
//!
//! La plage dynamique est le rang 3 de la cascade de fond
//! ([`crate::audio::replaygain::un_tour_de_cascade`]) : elle ne décode que
//! lorsque le ReplayGain ET les empreintes n'ont plus rien à faire, et elle
//! partage en plus le créneau d'analyse avec le CLAP
//! ([`crate::audio::replaygain::ANALYSIS_SLOT`]), qui passe un lot sur deux.
//! Chez Thierry (Tades, 537 910 pistes) elle ne démarrerait pas avant des
//! jours — « dommage, c'est plus urgent que le CLAP ».
//!
//! ## Le réglage
//!
//! Trois positions, et la première est l'ordre d'avant, qui reste le défaut :
//!
//! | Position | Cascade | CLAP |
//! |---|---|---|
//! | [`PrioriteDr::Derniere`] (défaut) | ReplayGain → empreintes → plage dynamique | alterne avec la cascade |
//! | [`PrioriteDr::AvantEmpreintes`] | ReplayGain → **plage dynamique** → empreintes | cède tant que la plage dynamique a du travail |
//! | [`PrioriteDr::Premiere`] | **plage dynamique** → ReplayGain → empreintes | cède tant que la plage dynamique a du travail |
//!
//! ⚠️ Ce que « avant le ReplayGain » veut dire, exactement : la passe
//! ReplayGain mesure DÉJÀ la plage dynamique de chaque piste qu'elle décode,
//! sur le même décodage. Le rang « plage dynamique » ne vise que les pistes
//! que le ReplayGain a laissées derrière lui sans DR (analysées avant que le
//! DR n'existe, ou gain lu dans les tags — `CANDIDATS_DR_WHERE`). La
//! position `Premiere` fait passer ce stock-là avant les pistes que le
//! ReplayGain n'a pas encore vues ; elle ne fait pas décoder deux fois une
//! même piste.
//!
//! ## Ce qui ne change pas
//!
//! La pause de l'utilisateur et la priorité à la lecture s'appliquent à
//! chaque rang comme avant : la descente s'arrête au premier rang SUSPENDU,
//! quel que soit l'ordre, et une zone qui joue arrête tout
//! (`replaygain::spawn`, `taches_de_fond::priorite`). Le CLAP ne cède
//! jamais à une plage dynamique suspendue : il reprendrait sinon la place
//! d'une passe qui ne tourne pas.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use crate::db::backend::DbBackend;
use crate::db::settings_repo::SettingsRepo;

/// La clé de la table `settings`. Préfixe des pauses
/// (`tache_de_fond_pause_…`) évité à dessein : un `LIKE` sur celles-ci ne
/// doit pas ramasser ce réglage.
pub const CLE_REGLAGE: &str = "tache_de_fond_priorite_dynamic_range";

/// Où se place la plage dynamique parmi les passes qui décodent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PrioriteDr {
    /// L'ordre historique : après le ReplayGain et les empreintes, en
    /// alternance avec le CLAP.
    #[default]
    Derniere,
    /// Après le ReplayGain, avant les empreintes et le CLAP.
    AvantEmpreintes,
    /// Avant tout : le ReplayGain, les empreintes et le CLAP.
    Premiere,
}

impl PrioriteDr {
    /// Toutes les positions, dans l'ordre où l'écran les propose.
    pub const TOUTES: [PrioriteDr; 3] = [
        PrioriteDr::Derniere,
        PrioriteDr::AvantEmpreintes,
        PrioriteDr::Premiere,
    ];

    /// Le mot que lisent l'API et le client web.
    pub fn id(self) -> &'static str {
        match self {
            PrioriteDr::Derniere => "last",
            PrioriteDr::AvantEmpreintes => "before_fingerprints",
            PrioriteDr::Premiere => "first",
        }
    }

    /// L'inverse de [`Self::id`]. `None` sur un mot inconnu : la route rend
    /// alors 400 au lieu de retomber en silence sur le défaut.
    pub fn depuis_id(id: &str) -> Option<Self> {
        Self::TOUTES.into_iter().find(|p| p.id() == id)
    }

    fn code(self) -> u8 {
        match self {
            PrioriteDr::Derniere => 0,
            PrioriteDr::AvantEmpreintes => 1,
            PrioriteDr::Premiere => 2,
        }
    }

    fn depuis_code(code: u8) -> Self {
        match code {
            1 => PrioriteDr::AvantEmpreintes,
            2 => PrioriteDr::Premiere,
            _ => PrioriteDr::Derniere,
        }
    }
}

/// Un rang de la cascade de fond.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rang {
    ReplayGain,
    Empreintes,
    PlageDynamique,
}

/// L'ordre dans lequel la cascade tente ses trois rangs.
///
/// Pure, et la SEULE source de l'ordre : la cascade itère sur ce tableau, elle
/// ne recopie pas l'ordre à la main.
pub fn ordre_de_la_cascade(priorite: PrioriteDr) -> [Rang; 3] {
    match priorite {
        PrioriteDr::Derniere => [Rang::ReplayGain, Rang::Empreintes, Rang::PlageDynamique],
        PrioriteDr::AvantEmpreintes => [Rang::ReplayGain, Rang::PlageDynamique, Rang::Empreintes],
        PrioriteDr::Premiere => [Rang::PlageDynamique, Rang::ReplayGain, Rang::Empreintes],
    }
}

/// Miroir en mémoire du réglage — la cascade le relit à chaque tour et le
/// CLAP à chaque lot : une lecture atomique, pas une requête.
static PRIORITE: AtomicU8 = AtomicU8::new(0);

/// Le dernier rang « plage dynamique » tenté a-t-il trouvé du travail ?
///
/// C'est le signal que lit le CLAP pour céder son tour. Posé par la cascade
/// après chaque tentative du rang, et remis à faux quand la cascade s'arrête
/// (analyse coupée) : un CLAP qui céderait à une passe éteinte ne
/// travaillerait plus jamais.
static DR_EN_ATTENTE: AtomicBool = AtomicBool::new(false);

/// Le réglage courant.
pub fn priorite_dr() -> PrioriteDr {
    PrioriteDr::depuis_code(PRIORITE.load(Ordering::Relaxed))
}

/// Relire le réglage depuis la base. Appelé par [`super::hydrater`] au
/// démarrage. Une valeur absente ou inconnue vaut le défaut.
pub fn hydrater(backend: &Arc<dyn DbBackend>) {
    let lu = SettingsRepo::with_backend(backend.clone())
        .get(CLE_REGLAGE)
        .ok()
        .flatten()
        .and_then(|v| PrioriteDr::depuis_id(v.trim()))
        .unwrap_or_default();
    PRIORITE.store(lu.code(), Ordering::Relaxed);
    if lu != PrioriteDr::Derniere {
        tracing::info!(priorite = lu.id(), "plage_dynamique_priorite_restauree");
    }
}

/// Changer le réglage : la base d'abord, le miroir ensuite — même ordre que
/// les pauses, pour la même raison.
pub fn fixer_priorite_dr(backend: &Arc<dyn DbBackend>, priorite: PrioriteDr) -> Result<(), String> {
    SettingsRepo::with_backend(backend.clone()).set(CLE_REGLAGE, priorite.id())?;
    PRIORITE.store(priorite.code(), Ordering::Relaxed);
    tracing::info!(
        priorite = priorite.id(),
        "plage_dynamique_priorite_modifiee"
    );
    Ok(())
}

/// La cascade note ce que le rang « plage dynamique » vient de rendre.
pub fn noter_travail_dr(a_travaille: bool) {
    DR_EN_ATTENTE.store(a_travaille, Ordering::Relaxed);
}

/// La décision du CLAP, pure : céder son tour à la plage dynamique ?
///
/// Oui seulement si les trois tiennent : l'utilisateur a placé la plage
/// dynamique avant le CLAP, elle n'est pas suspendue, et son dernier tour a
/// trouvé du travail.
pub fn le_clap_doit_ceder(priorite: PrioriteDr, dr_en_pause: bool, dr_en_attente: bool) -> bool {
    priorite != PrioriteDr::Derniere && !dr_en_pause && dr_en_attente
}

/// La même décision, sur l'état du processus. Appelée par la boucle CLAP
/// (`audio::embedding`) avant de prendre le créneau d'analyse.
pub fn le_clap_cede_a_la_plage_dynamique() -> bool {
    le_clap_doit_ceder(
        priorite_dr(),
        super::est_en_pause(super::Tache::PlageDynamique),
        DR_EN_ATTENTE.load(Ordering::Relaxed),
    )
}

/// Remettre le miroir à neuf, pour les témoins (caisses externes).
pub fn oublier_pour_les_essais() {
    PRIORITE.store(0, Ordering::Relaxed);
    DR_EN_ATTENTE.store(false, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn par_defaut_l_ordre_historique_ne_change_pas() {
        assert_eq!(PrioriteDr::default(), PrioriteDr::Derniere);
        assert_eq!(
            ordre_de_la_cascade(PrioriteDr::default()),
            [Rang::ReplayGain, Rang::Empreintes, Rang::PlageDynamique],
            "#5169 — le défaut DOIT rester l'ordre d'avant le réglage"
        );
    }

    #[test]
    fn avant_les_empreintes_la_plage_dynamique_passe_au_rang_2() {
        assert_eq!(
            ordre_de_la_cascade(PrioriteDr::AvantEmpreintes),
            [Rang::ReplayGain, Rang::PlageDynamique, Rang::Empreintes]
        );
    }

    #[test]
    fn en_premier_la_plage_dynamique_passe_avant_le_replaygain() {
        assert_eq!(
            ordre_de_la_cascade(PrioriteDr::Premiere),
            [Rang::PlageDynamique, Rang::ReplayGain, Rang::Empreintes]
        );
    }

    #[test]
    fn chaque_ordre_tente_les_trois_rangs_une_fois() {
        for p in PrioriteDr::TOUTES {
            let o = ordre_de_la_cascade(p);
            for r in [Rang::ReplayGain, Rang::Empreintes, Rang::PlageDynamique] {
                assert_eq!(o.iter().filter(|x| **x == r).count(), 1, "{p:?} : {o:?}");
            }
        }
    }

    #[test]
    fn les_identifiants_font_l_aller_retour() {
        for p in PrioriteDr::TOUTES {
            assert_eq!(PrioriteDr::depuis_id(p.id()), Some(p));
            assert_eq!(PrioriteDr::depuis_code(p.code()), p);
        }
        assert_eq!(PrioriteDr::depuis_id("avant"), None);
    }

    #[test]
    fn le_clap_ne_cede_que_sur_reglage_hors_pause_et_avec_du_travail() {
        use PrioriteDr::*;
        assert!(
            !le_clap_doit_ceder(Derniere, false, true),
            "défaut : jamais"
        );
        assert!(le_clap_doit_ceder(AvantEmpreintes, false, true));
        assert!(le_clap_doit_ceder(Premiere, false, true));
        assert!(
            !le_clap_doit_ceder(Premiere, true, true),
            "une plage dynamique SUSPENDUE ne doit pas geler le CLAP"
        );
        assert!(
            !le_clap_doit_ceder(Premiere, false, false),
            "une plage dynamique au repos ne doit pas geler le CLAP"
        );
    }
}
