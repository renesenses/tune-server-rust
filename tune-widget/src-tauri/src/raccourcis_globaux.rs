//! Raccourcis GLOBAUX, sous Windows SEULEMENT (décision de Bertrand, relecture
//! de #9257 ; #8531, Sandro).
//!
//! Sous macOS, ils sont retirés (#9255) : la capture des touches média y passe
//! par un `CGEventTap`, soumis à l'autorisation d'Accessibilité (« contrôler
//! cet ordinateur »). Sous Windows, `RegisterHotKey` et le crochet clavier ne
//! demandent RIEN à l'utilisateur, et il n'y a pas de barre des menus pour
//! porter les raccourcis : ils restent donc, comme avant #9255.
//!
//! Le greffon n'est une dépendance que sous Windows
//! (`[target.'cfg(windows)'.dependencies]`) : il n'est même pas compilé dans
//! l'app macOS. Aucune capacité ne lui est ouverte : l'enregistrement se fait
//! ici, côté Rust ; la page n'en a pas besoin.

use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut, ShortcutState};

use crate::raccourcis::Action;

/// Les raccourcis d'avant #9255, sur la zone visée (`zone_visee`).
pub const RACCOURCIS: &[(&str, Action)] = &[
    ("CmdOrCtrl+Shift+Space", Action::LecturePause),
    ("CmdOrCtrl+Shift+Right", Action::PisteSuivante),
    ("CmdOrCtrl+Shift+Left", Action::PistePrecedente),
    ("CmdOrCtrl+Shift+Up", Action::VolumePlus),
    ("CmdOrCtrl+Shift+Down", Action::VolumeMoins),
    ("CmdOrCtrl+Shift+M", Action::BasculerMini),
    // Les touches média du clavier (#8531). Les touches de VOLUME du clavier
    // restent au système : elles règlent la sortie de l'ordinateur.
    ("MediaPlayPause", Action::LecturePause),
    ("MediaTrackNext", Action::PisteSuivante),
    ("MediaTrackPrevious", Action::PistePrecedente),
];

/// L'action d'un raccourci pressé.
pub fn action_de(raccourci: &Shortcut) -> Option<Action> {
    RACCOURCIS.iter().find_map(|(spec, action)| {
        spec.parse::<Shortcut>()
            .ok()
            .filter(|lu| lu == raccourci)
            .map(|_| *action)
    })
}

/// Le greffon, avec UN gestionnaire pour tous les raccourcis (des fermetures
/// par raccourci faisaient planter macOS à l'époque, 2a37432) ; le travail
/// part sur le fil asynchrone.
pub fn greffon() -> tauri::plugin::TauriPlugin<tauri::Wry> {
    tauri_plugin_global_shortcut::Builder::new()
        .with_handler(|app, raccourci, evenement| {
            if evenement.state() != ShortcutState::Pressed {
                return;
            }
            if let Some(action) = action_de(raccourci) {
                tauri::async_runtime::spawn(crate::executer_action_menu(app.clone(), action));
            }
        })
        .build()
}

/// Enregistre les raccourcis ; un échec (combinaison prise par une autre app)
/// est journalisé, jamais fatal.
pub fn enregistrer(app: &tauri::AppHandle) {
    for (spec, _) in RACCOURCIS {
        match spec.parse::<Shortcut>() {
            Ok(sc) => {
                if let Err(e) = app.global_shortcut().register(sc) {
                    tracing::warn!(shortcut = spec, error = %e, "global_shortcut_register_failed");
                }
            }
            Err(e) => tracing::warn!(shortcut = spec, error = %e, "global_shortcut_parse_failed"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tauri_plugin_global_shortcut::Code;

    #[test]
    fn les_touches_media_du_clavier_pilotent_la_lecture() {
        let action = |code| action_de(&Shortcut::new(None, code));
        assert_eq!(action(Code::MediaPlayPause), Some(Action::LecturePause));
        assert_eq!(action(Code::MediaTrackNext), Some(Action::PisteSuivante));
        assert_eq!(
            action(Code::MediaTrackPrevious),
            Some(Action::PistePrecedente)
        );
        assert_eq!(action(Code::AudioVolumeUp), None);
        let lus: Vec<Shortcut> = RACCOURCIS
            .iter()
            .map(|(spec, _)| spec.parse::<Shortcut>().expect(spec))
            .collect();
        for (i, a) in lus.iter().enumerate() {
            assert!(!lus[i + 1..].contains(a), "{}", RACCOURCIS[i].0);
        }
    }
}
