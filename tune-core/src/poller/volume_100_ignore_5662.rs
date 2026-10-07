//! #5662 — le sondeur ignore un renderer à 100 %, et le journal le dit.
//!
//! Les trois sites d'adoption du volume (`tick.rs`) écartent délibérément un
//! volume rapporté ≥ 0,999 : beaucoup de renderers annoncent 100 % par défaut
//! (ou au réveil), et l'adopter écraserait la consigne de l'auditeur. Cette
//! règle reste. Mais une enceinte qui se remet D'ELLE-MÊME à 100 % n'était ni
//! adoptée, ni affichée, ni signalée : Tune continuait d'afficher sa propre
//! valeur, et rien au journal ne permettait de le constater.
//!
//! Un épisode commence quand le renderer annonce ≥ 0,999 alors que Tune croit
//! la zone plus bas : une ligne WARN `renderer_volume_100_ignore`, une seule.
//! Il se termine quand les deux valeurs ne divergent plus de cette façon (le
//! renderer redescend, ou la consigne de Tune monte à 100 %) : une ligne INFO
//! `renderer_volume_100_fin`.
//!
//! Le constat ne décide de rien : réimposer le volume de Tune au renderer est
//! une autre décision.

use super::*;

/// Ce que vaut un tour de sondage pour l'épisode d'une zone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum EpisodeVolume100 {
    /// Le renderer vient de passer à 100 % sans Tune : la ligne WARN part.
    Ouvert,
    /// L'épisode en cours prend fin : la ligne INFO part.
    Ferme,
    /// Rien de neuf : rien n'est écrit.
    Inchange,
}

/// Seuil commun aux trois sites d'adoption de `tick.rs`.
const PLEIN_VOLUME: f64 = 0.999;

/// La transition, sans état ni journal.
pub(super) fn transition_volume_100(
    en_cours: bool,
    tune_croit: f64,
    renderer: f64,
) -> EpisodeVolume100 {
    let ignore = renderer >= PLEIN_VOLUME && tune_croit < PLEIN_VOLUME;
    match (en_cours, ignore) {
        (false, true) => EpisodeVolume100::Ouvert,
        (true, false) => EpisodeVolume100::Ferme,
        _ => EpisodeVolume100::Inchange,
    }
}

/// Tient l'épisode de `zone_id` dans `episodes` et écrit ses deux bornes.
pub(super) fn constater_volume_100(
    episodes: &std::sync::Mutex<std::collections::HashSet<i64>>,
    zone_id: i64,
    tune_croit: f64,
    renderer: f64,
) -> EpisodeVolume100 {
    let Ok(mut en_cours) = episodes.lock() else {
        return EpisodeVolume100::Inchange;
    };
    let transition = transition_volume_100(en_cours.contains(&zone_id), tune_croit, renderer);
    match transition {
        EpisodeVolume100::Ouvert => {
            en_cours.insert(zone_id);
            warn!(zone_id, tune_croit, renderer, "renderer_volume_100_ignore");
        }
        EpisodeVolume100::Ferme => {
            en_cours.remove(&zone_id);
            info!(zone_id, tune_croit, renderer, "renderer_volume_100_fin");
        }
        EpisodeVolume100::Inchange => {}
    }
    transition
}

impl PositionPoller {
    /// À appeler sur chaque site d'adoption, quand l'adoption serait
    /// possible hors du seuil de 100 % (zone à volume variable, hors des
    /// délais de grâce).
    pub(super) fn volume_100_ignore_constate(&self, zone_id: i64, tune_croit: f64, renderer: f64) {
        constater_volume_100(&self.volumes_100_ignores, zone_id, tune_croit, renderer);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct JournalCapture(Arc<Mutex<Vec<u8>>>);
    impl JournalCapture {
        fn texte(&self) -> String {
            String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
        }
    }
    impl std::io::Write for JournalCapture {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for JournalCapture {
        type Writer = JournalCapture;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    #[test]
    fn la_transition_ne_s_ouvre_que_sur_un_vrai_ecart() {
        use EpisodeVolume100::*;
        assert_eq!(transition_volume_100(false, 0.42, 1.0), Ouvert);
        assert_eq!(transition_volume_100(true, 0.42, 1.0), Inchange);
        assert_eq!(transition_volume_100(true, 0.42, 0.42), Ferme);
        // Tune croit 100 % lui aussi : pas d'écart, pas d'épisode.
        assert_eq!(transition_volume_100(false, 1.0, 1.0), Inchange);
        assert_eq!(transition_volume_100(true, 1.0, 1.0), Ferme);
        // Un volume ordinaire n'ouvre rien.
        assert_eq!(transition_volume_100(false, 0.42, 0.6), Inchange);
    }

    /// Dix tours à 100 % donnent UNE ligne WARN ; le retour à 42 % donne UNE
    /// fin d'épisode ; un second passage à 100 % rouvre un épisode.
    #[test]
    fn un_episode_donne_une_ligne_puis_une_fin() {
        crate::journal_de_test::fiabiliser_la_capture();
        let journal = JournalCapture::default();
        let abonne = tracing_subscriber::fmt()
            .with_writer(journal.clone())
            .with_ansi(false)
            .with_max_level(tracing::Level::INFO)
            .finish();
        let episodes = std::sync::Mutex::new(HashSet::new());
        tracing::subscriber::with_default(abonne, || {
            for _ in 0..10 {
                constater_volume_100(&episodes, 7, 0.42, 1.0);
            }
            for _ in 0..3 {
                constater_volume_100(&episodes, 7, 0.42, 0.42);
            }
            constater_volume_100(&episodes, 7, 0.42, 1.0);
        });
        let texte = journal.texte();
        let ignores: Vec<&str> = texte
            .lines()
            .filter(|l| l.contains("renderer_volume_100_ignore"))
            .collect();
        assert_eq!(ignores.len(), 2, "deux épisodes, deux lignes :\n{texte}");
        assert!(ignores[0].contains("WARN"), "{}", ignores[0]);
        assert!(
            ignores[0].contains("zone_id=7")
                && ignores[0].contains("tune_croit=0.42")
                && ignores[0].contains("renderer=1"),
            "la ligne doit dire la zone et les deux volumes : {}",
            ignores[0]
        );
        let fins: Vec<&str> = texte
            .lines()
            .filter(|l| l.contains("renderer_volume_100_fin"))
            .collect();
        assert_eq!(fins.len(), 1, "une seule fin d'épisode :\n{texte}");
        assert!(fins[0].contains("INFO"), "{}", fins[0]);
    }
}
