//! Les raccourcis clavier DANS l'app (#9255, fil du forum 2968).
//!
//! Jusqu'à la rc12, l'app enregistrait des raccourcis GLOBAUX
//! (`tauri-plugin-global-shortcut`) : Cmd+Maj+Espace/←/→/↑/↓/M et les touches
//! média du clavier. Sur macOS, la capture des touches média passe par un
//! `CGEventTap`, que le système soumet à l'autorisation d'Accessibilité :
//! au premier lancement, macOS demandait donc à l'utilisateur de laisser l'app
//! « contrôler cet ordinateur ». Rémi : « des autorisations qui paraissent TRÈS
//! étendues ». Décision de Bertrand du 20/11 : plus aucun raccourci global.
//!
//! À la place, les raccourcis d'une app macOS ordinaire : les entrées de la
//! barre des menus et leurs équivalents clavier, qui ne marchent que quand
//! l'app est au premier plan et ne demandent AUCUNE autorisation. Ils suivent
//! l'usage de Roon (capture du fil 2968) là où il ne heurte pas celui de macOS :
//!
//! | Raccourci | Action                       | Roon      |
//! |-----------|------------------------------|-----------|
//! | Espace    | lecture / pause (la page)    | Espace    |
//! | ⌘J / ⌘K   | piste précédente / suivante  | ⌘J / ⌘K   |
//! | ⌘↑ / ⌘↓   | volume + / −                 | ⌘↑ / ⌘↓   |
//! | ⌘[ / ⌘]   | écran précédent / suivant    | ⌘← / ⌘→   |
//! | ⌘F        | recherche                    | ⌘F        |
//! | ⌘⇧M       | mini-lecteur / fenêtre complète | —      |
//! | ⌃⌘F       | plein écran (macOS)          | —         |
//!
//! ⌘[ / ⌘] plutôt que ⌘← / ⌘→ : dans un champ de saisie, ⌘← / ⌘→ vont en début
//! et fin de ligne, et un équivalent de menu les volerait ; ⌘[ / ⌘] sont ceux
//! de Safari et du Finder, et ceux que le client web connaît déjà (web#3275).
//! Quand la page traite elle-même la touche (elle l'annule), WebKit ne la passe
//! pas au menu : aucun geste n'est joué deux fois.
//!
//! L'espace n'est pas un équivalent de menu : il taperait une espace dans le
//! champ de recherche. La page la traite (client web : `keyboard.ts` ;
//! mini-lecteur embarqué : `app.js`), hors des champs de saisie.
//!
//! Le module est pur : `main.rs` construit le menu et exécute les actions.

/// Ce qu'une entrée de menu (ou son raccourci) fait.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    LecturePause,
    PistePrecedente,
    PisteSuivante,
    VolumePlus,
    VolumeMoins,
    /// L'écran précédent de la page (`history.back()`).
    Reculer,
    /// L'écran suivant de la page (`history.forward()`).
    Avancer,
    Rechercher,
    /// Bascule mini-lecteur ↔ fenêtre complète.
    BasculerMini,
    /// Ouvre la liste des serveurs du réseau (et la saisie manuelle).
    ChoisirServeur,
}

/// Le menu de la barre des menus où l'entrée se range.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Menu {
    App,
    Lecture,
    Aller,
    Presentation,
}

pub struct Entree {
    /// Identifiant de l'entrée de menu (`MenuEvent::id`).
    pub id: &'static str,
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub libelle: &'static str,
    /// Équivalent clavier, syntaxe des accélérateurs de Tauri (`muda`).
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub accelerateur: Option<&'static str>,
    pub action: Action,
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub menu: Menu,
}

/// Toutes les entrées propres à l'app, dans l'ordre d'affichage.
pub const ENTREES: &[Entree] = &[
    Entree {
        id: "choisir_serveur",
        libelle: "Choisir un serveur…",
        accelerateur: None,
        action: Action::ChoisirServeur,
        menu: Menu::App,
    },
    Entree {
        id: "lecture_pause",
        libelle: "Lecture / Pause (Espace)",
        accelerateur: None,
        action: Action::LecturePause,
        menu: Menu::Lecture,
    },
    Entree {
        id: "piste_precedente",
        libelle: "Piste précédente",
        accelerateur: Some("CmdOrCtrl+J"),
        action: Action::PistePrecedente,
        menu: Menu::Lecture,
    },
    Entree {
        id: "piste_suivante",
        libelle: "Piste suivante",
        accelerateur: Some("CmdOrCtrl+K"),
        action: Action::PisteSuivante,
        menu: Menu::Lecture,
    },
    Entree {
        id: "volume_plus",
        libelle: "Monter le volume",
        accelerateur: Some("CmdOrCtrl+Up"),
        action: Action::VolumePlus,
        menu: Menu::Lecture,
    },
    Entree {
        id: "volume_moins",
        libelle: "Baisser le volume",
        accelerateur: Some("CmdOrCtrl+Down"),
        action: Action::VolumeMoins,
        menu: Menu::Lecture,
    },
    Entree {
        id: "reculer",
        libelle: "Précédent",
        accelerateur: Some("CmdOrCtrl+["),
        action: Action::Reculer,
        menu: Menu::Aller,
    },
    Entree {
        id: "avancer",
        libelle: "Suivant",
        accelerateur: Some("CmdOrCtrl+]"),
        action: Action::Avancer,
        menu: Menu::Aller,
    },
    Entree {
        id: "rechercher",
        libelle: "Rechercher",
        accelerateur: Some("CmdOrCtrl+F"),
        action: Action::Rechercher,
        menu: Menu::Aller,
    },
    Entree {
        id: "basculer_mini",
        libelle: "Mini-lecteur / fenêtre complète",
        accelerateur: Some("CmdOrCtrl+Shift+M"),
        action: Action::BasculerMini,
        menu: Menu::Presentation,
    },
];

/// L'action d'un identifiant d'entrée de menu.
pub fn action_de(id: &str) -> Option<Action> {
    ENTREES.iter().find(|e| e.id == id).map(|e| e.action)
}

/// Les entrées d'un menu, dans l'ordre.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn entrees_de(menu: Menu) -> impl Iterator<Item = &'static Entree> {
    ENTREES.iter().filter(move |e| e.menu == menu)
}

/// Le script qu'une action de NAVIGATION exécute dans la page de la fenêtre au
/// premier plan ; `None` pour les actions que le côté natif exécute lui-même.
///
/// La recherche n'a pas d'adresse dans le client web (la coquille navigue par
/// son état, pas par l'URL) : on presse son bouton — l'onglet Recherche de la
/// vue mini, la loupe de l'en-tête de la vue complète — puis on donne le focus
/// au champ ; dans le mini-lecteur embarqué, c'est `#search-input`.
pub fn script_de(action: Action) -> Option<&'static str> {
    match action {
        Action::Reculer => Some("history.back()"),
        Action::Avancer => Some("history.forward()"),
        Action::Rechercher => Some(
            "(function(){\
var champ=function(){return document.querySelector('.search-overlay input,input[type=\"search\"],#search-input');};\
var c=champ();if(c){c.focus();if(c.select)c.select();return;}\
var b=document.querySelector('[data-onglet=\"recherche\"],.search-icon-btn');if(!b)return;b.click();\
setTimeout(function(){var c=champ();if(c)c.focus();},150);})()",
        ),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn chaque_raccourci_se_lit_et_n_est_declare_qu_une_fois() {
        let mut vus = Vec::new();
        for e in ENTREES {
            if let Some(a) = e.accelerateur {
                let lu = muda::accelerator::Accelerator::from_str(a)
                    .unwrap_or_else(|err| panic!("{a} illisible : {err}"));
                assert!(!vus.contains(&lu), "{a} déclaré deux fois");
                vus.push(lu);
            }
        }
        let mut ids: Vec<_> = ENTREES.iter().map(|e| e.id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), ENTREES.len(), "identifiant d'entrée en double");
    }

    #[test]
    fn les_raccourcis_de_roon_et_de_macos() {
        let raccourci = |action| {
            ENTREES
                .iter()
                .find(|e| e.action == action)
                .and_then(|e| e.accelerateur)
        };
        assert_eq!(raccourci(Action::PistePrecedente), Some("CmdOrCtrl+J"));
        assert_eq!(raccourci(Action::PisteSuivante), Some("CmdOrCtrl+K"));
        assert_eq!(raccourci(Action::VolumePlus), Some("CmdOrCtrl+Up"));
        assert_eq!(raccourci(Action::VolumeMoins), Some("CmdOrCtrl+Down"));
        assert_eq!(raccourci(Action::Reculer), Some("CmdOrCtrl+["));
        assert_eq!(raccourci(Action::Avancer), Some("CmdOrCtrl+]"));
        assert_eq!(raccourci(Action::Rechercher), Some("CmdOrCtrl+F"));
        // L'espace n'est PAS un équivalent de menu : il taperait dans les
        // champs. C'est la page qui le traite.
        assert_eq!(raccourci(Action::LecturePause), None);
    }

    /// Aucun raccourci sans modificateur : un équivalent de menu sans ⌘
    /// volerait des touches tapées dans un champ.
    #[test]
    fn aucun_raccourci_ne_vole_une_touche_de_saisie() {
        use muda::accelerator::{Accelerator, Modifiers};
        for e in ENTREES {
            if let Some(a) = e.accelerateur {
                let lu = Accelerator::from_str(a).unwrap();
                assert!(
                    lu.modifiers()
                        .intersects(Modifiers::SUPER | Modifiers::CONTROL),
                    "{a} sans ⌘ ni Ctrl"
                );
            }
        }
    }

    #[test]
    fn les_identifiants_menent_a_leur_action() {
        assert_eq!(action_de("piste_suivante"), Some(Action::PisteSuivante));
        assert_eq!(action_de("choisir_serveur"), Some(Action::ChoisirServeur));
        assert_eq!(action_de("quit"), None);
        assert_eq!(entrees_de(Menu::Aller).count(), 3);
    }

    #[test]
    fn seules_les_actions_de_navigation_ont_un_script() {
        assert_eq!(script_de(Action::Reculer), Some("history.back()"));
        assert_eq!(script_de(Action::Avancer), Some("history.forward()"));
        assert!(script_de(Action::Rechercher)
            .unwrap()
            .contains("data-onglet=\"recherche\""));
        assert_eq!(script_de(Action::PisteSuivante), None);
        assert_eq!(script_de(Action::LecturePause), None);
    }

    /// La décision du 20/11 : AUCUN raccourci global sous macOS, donc aucune
    /// demande d'Accessibilité. Sous Windows, ils restent (#8531, relecture de
    /// #9257) : le greffon ne doit être une dépendance QUE pour Windows, sans
    /// quoi il serait compilé — et la capture des touches média active — dans
    /// l'app macOS. Aucune capacité ne doit l'ouvrir (enregistrement côté Rust).
    #[test]
    fn aucun_raccourci_global_sous_macos() {
        let manifeste = include_str!("../Cargo.toml");
        let mut section = "";
        for ligne in manifeste.lines() {
            let l = ligne.trim();
            if l.starts_with('[') {
                section = l;
            } else if l.starts_with("tauri-plugin-global-shortcut") {
                assert_eq!(
                    section, "[target.'cfg(windows)'.dependencies]",
                    "tauri-plugin-global-shortcut hors de la section Windows : \
                     macOS redemanderait l'Accessibilité"
                );
            }
        }
        for capacite in [
            include_str!("../capabilities/default.json"),
            include_str!("../capabilities/full-window.json"),
        ] {
            assert!(!capacite.contains("global-shortcut"));
        }
        // Le code qui l'utilise n'est compilé que pour Windows.
        let main = include_str!("main.rs");
        assert!(main.contains("#[cfg(windows)]\nmod raccourcis_globaux;"));
    }
}
