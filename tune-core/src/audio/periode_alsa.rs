//! #3208 — la période demandée au pilote local, et la garde de préchargement
//! qui s'en déduit.
//!
//! ## Le fait
//!
//! Tous les flux de la sortie locale s'ouvraient avec `cpal::BufferSize::Default` :
//! `cpal::BufferSize::Fixed` n'apparaissait NULLE PART dans le dépôt. Sous ALSA,
//! `Default` laisse le pilote choisir seul la taille de période — il n'existait
//! donc aucune maîtrise de la latence matérielle sous Linux, et aucun moyen d'en
//! mesurer une.
//!
//! ## Ce que ce module choisit — et ce qu'il refuse de choisir
//!
//! Il ne fixe AUCUNE valeur par défaut. L'issue le demande explicitement : « une
//! période trop courte produit des xruns sur du matériel modeste ; trop longue,
//! elle annule le bénéfice », et personne n'a encore ouvert un flux avec une
//! période imposée sur du vrai matériel. Tant que ce relevé n'existe pas, une
//! valeur par défaut serait devinée.
//!
//! Le module rend donc la période *choisissable et mesurable*, désarmée par
//! défaut, Linux seulement, par la variable d'environnement
//! [`VARIABLE_PERIODE`]. Sans elle — et sur toute autre plateforme — les
//! fonctions rendent exactement le comportement d'aujourd'hui. Le témoin
//! obligatoire de l'issue (« les autres plateformes ne doivent RIEN voir
//! changer ») est tenu par construction : hors Linux, [`trames_de_periode`] ne
//! lit même pas l'environnement. Même précédent que `TUNE_DASH_WARM_CACHE`.
//!
//! ## La garde de préchargement, et d'où elle vient VRAIMENT
//!
//! L'issue affirmait que la garde de ~500 ms « vient de CoreAudio ». L'historique
//! dit autre chose, et il a été établi avant d'y toucher :
//!
//! - `84c08264` (13/06/2026, 15h21) la fait NAÎTRE pour du bruit blanc entre
//!   pistes signalé sur macOS **et sur Windows** — donc multi-plateforme ;
//! - `3c9696eb` (le même jour, 17h28) la GROSSIT de 200 à 500 ms sur le seul
//!   relevé de FRIDER, macOS/CoreAudio.
//!
//! Seule sa TAILLE vient de CoreAudio ; son existence, non. La baisser sous Linux
//! « parce qu'elle est CoreAudio » aurait remplacé une prudence non mesurée par
//! une audace non mesurée. Elle n'est donc pas touchée tant qu'aucune période
//! n'est imposée : sans opt-in, [`garde_de_prechargement`] rend le compte
//! d'origine, au sample près.
//!
//! En revanche, dès qu'une période EST imposée, la garde cesse d'être un nombre
//! de millisecondes hérité d'un défaut CoreAudio et devient ce qu'elle aurait
//! toujours dû être quand la période est connue : un nombre de PÉRIODES
//! ([`PERIODES_DE_GARDE`]). C'est la troisième question de l'issue — « une garde
//! calculée en millisecondes et une période réglée en trames peuvent se
//! contredire » — et elle se tranche ici dans le seul sens sûr : la garde dérivée
//! ne DÉPASSE jamais la garde d'origine (`min`), elle ne peut donc qu'abaisser la
//! latence de démarrage, jamais raccourcir une protection en dessous de ce que
//! demande le pilote.

/// La variable qui arme la période. Absente ⇒ comportement d'aujourd'hui.
pub const VARIABLE_PERIODE: &str = "TUNE_ALSA_PERIOD_FRAMES";

/// Bornes de plausibilité. Hors bornes ⇒ la valeur est ignorée, et la sortie
/// locale garde le comportement d'aujourd'hui.
///
/// Une valeur absurde n'est pas anodine : toutes les tentatives de la cascade
/// d'ouverture portent la MÊME période, donc une période qu'ALSA refuse les fait
/// toutes échouer et la zone reste muette. Une faute de frappe dans une variable
/// d'environnement ne doit pas pouvoir éteindre la sortie locale.
pub const TRAMES_MIN: u32 = 16;
/// Voir [`TRAMES_MIN`].
pub const TRAMES_MAX: u32 = 65_536;

/// Combien de périodes de réserve avant que le rappel cesse de sortir du
/// silence, quand la période est connue.
pub const PERIODES_DE_GARDE: usize = 4;

/// La décision pure : ce que vaut le texte de la variable.
///
/// Séparée de la lecture de l'environnement pour être éprouvable sans
/// `set_var` — qui casserait la suite `--workspace` en la faisant courir dans
/// un processus dont d'autres épreuves lisent l'environnement.
pub fn trames_depuis(valeur: Option<&str>) -> Option<u32> {
    let n: u32 = valeur?.trim().parse().ok()?;
    (TRAMES_MIN..=TRAMES_MAX).contains(&n).then_some(n)
}

/// La période demandée au pilote dans CE processus, ou `None` si le pilote doit
/// choisir seul (le comportement de toujours).
///
/// Linux seulement : ailleurs, la fonction se réduit à `None` et le compilateur
/// retire jusqu'à la lecture de la variable.
#[cfg(target_os = "linux")]
pub fn trames_de_periode() -> Option<u32> {
    static TRAMES: std::sync::LazyLock<Option<u32>> = std::sync::LazyLock::new(|| {
        let choisi = trames_depuis(std::env::var(VARIABLE_PERIODE).ok().as_deref());
        if let Some(n) = choisi {
            tracing::info!(period_frames = n, "local_audio_alsa_period_armed");
        }
        choisi
    });
    *TRAMES
}

/// Voir la version Linux : hors Linux, le pilote choisit seul, sans condition.
#[cfg(not(target_os = "linux"))]
pub fn trames_de_periode() -> Option<u32> {
    None
}

/// Le seuil de préchargement, en échantillons ENTRELACÉS, que le rappel attend
/// avant de cesser de sortir du silence.
///
/// `garde_par_defaut` est le compte d'aujourd'hui, calculé en millisecondes par
/// l'appelant (`sr * ch / 2` pour ~500 ms, `sr * ch / 5` pour ~200 ms). Sans
/// période imposée, il est rendu tel quel — aucun chemin ne change.
pub fn garde_de_prechargement(
    trames_de_periode: Option<u32>,
    garde_par_defaut: usize,
    canaux: u16,
) -> usize {
    let Some(trames) = trames_de_periode else {
        return garde_par_defaut;
    };
    let canaux = (canaux.max(1)) as usize;
    (trames as usize)
        .saturating_mul(canaux)
        .saturating_mul(PERIODES_DE_GARDE)
        .min(garde_par_defaut)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Le marqueur qui dit à l'enfant qu'il est l'enfant. Sans lui, l'épreuve
    /// enfant ne fait rien : elle ne doit pas prétendre mesurer quoi que ce soit
    /// dans un processus dont l'environnement n'est pas armé.
    const MARQUEUR_ENFANT: &str = "TUNE_TEST_PERIODE_ALSA_ENFANT";
    const CHEMIN_ENFANT: &str = "audio::periode_alsa::tests::le_processus_lit_sa_propre_variable";

    /// Les trois fichiers de PRODUCTION de la sortie locale, tels qu'ils sont
    /// sur le disque. `include_str!` ne dépend d'aucune fonctionnalité : cette
    /// garde tourne donc dans la porte `test` de la CI, qui ne compile PAS
    /// `local-audio` — sans quoi elle ne garderait rien là où ça compte.
    const FICHIERS_DE_PRODUCTION: [(&str, &str); 3] = [
        ("outputs/local.rs", include_str!("../outputs/local.rs")),
        (
            "outputs/local/backend.rs",
            include_str!("../outputs/local/backend.rs"),
        ),
        (
            "outputs/local/resolution.rs",
            include_str!("../outputs/local/resolution.rs"),
        ),
    ];

    /// LE fait du ticket : plus aucun site de création de flux n'écrit
    /// `BufferSize::Default` à la main. Le seul endroit du dépôt qui a le droit
    /// de l'écrire est `outputs/local/periode.rs`, qui EST la décision.
    ///
    /// Négative par nature : aucune définition ajoutée ailleurs ne peut la
    /// satisfaire à faux, et un site ajouté demain avec un `BufferSize` écrit à
    /// la main la fait rougir.
    #[test]
    fn aucun_site_de_production_ne_choisit_la_periode_a_la_main() {
        for (nom, texte) in FICHIERS_DE_PRODUCTION {
            assert!(
                !texte.contains("BufferSize::Default"),
                "{nom} choisit encore la période à la main : la sortie locale \
                 ouvre sans période imposée, quoi que dise la variable (#3208)"
            );
        }
    }

    /// Le pendant positif : les trois fichiers APPELLENT la décision. Sans
    /// cette moitié, supprimer purement et simplement les sites rendrait la
    /// garde précédente verte.
    ///
    /// Les deux noms cherchés sont DÉFINIS dans `outputs/local/periode.rs` :
    /// les trouver ici est donc un appel, jamais une définition.
    #[test]
    fn les_sites_de_production_appellent_la_decision() {
        for (nom, texte) in FICHIERS_DE_PRODUCTION {
            assert!(
                texte.contains("config_de_flux") || texte.contains("avec_periode"),
                "{nom} n'appelle plus la décision de période (#3208)"
            );
        }
    }

    #[test]
    fn une_variable_absente_laisse_le_pilote_choisir() {
        assert_eq!(trames_depuis(None), None);
    }

    #[test]
    fn une_valeur_illisible_ou_hors_bornes_est_ignoree() {
        for texte in ["", "  ", "0", "abc", "256ms", "-256", "8", "65537", "4.5"] {
            assert_eq!(
                trames_depuis(Some(texte)),
                None,
                "« {texte} » ne doit pas armer de période"
            );
        }
    }

    #[test]
    fn une_valeur_plausible_arme_la_periode() {
        assert_eq!(trames_depuis(Some("256")), Some(256));
        assert_eq!(trames_depuis(Some(" 1024 ")), Some(1024));
        assert_eq!(trames_depuis(Some("16")), Some(TRAMES_MIN));
        assert_eq!(trames_depuis(Some("65536")), Some(TRAMES_MAX));
    }

    #[test]
    fn sans_periode_la_garde_est_celle_d_aujourd_hui_au_sample_pres() {
        // 44 100 × 2 / 2 = ~500 ms, le compte exact du chemin compressé.
        assert_eq!(garde_de_prechargement(None, 44_100, 2), 44_100);
        // 48 000 × 2 / 5 = ~200 ms, le compte exact du chemin PCM.
        assert_eq!(garde_de_prechargement(None, 19_200, 2), 19_200);
    }

    #[test]
    fn une_periode_imposee_compte_la_garde_en_periodes() {
        // 256 trames × 2 voies × 4 périodes = 2 048 échantillons entrelacés,
        // soit ~23 ms à 44,1 kHz au lieu de 500.
        assert_eq!(
            garde_de_prechargement(Some(256), 44_100, 2),
            256 * 2 * PERIODES_DE_GARDE
        );
    }

    #[test]
    fn la_garde_derivee_ne_depasse_jamais_celle_d_origine() {
        // Une période énorme ne doit pas GROSSIR la garde : la borne haute reste
        // le compte en millisecondes d'aujourd'hui.
        assert_eq!(garde_de_prechargement(Some(65_536), 44_100, 2), 44_100);
    }

    /// Exécutée SEULE par l'épreuve parente, dans un processus dont
    /// l'environnement est armé. Hors de ce cadre, elle ne mesure rien et le dit
    /// en ne faisant rien.
    #[test]
    fn le_processus_lit_sa_propre_variable() {
        if std::env::var(MARQUEUR_ENFANT).is_err() {
            return;
        }
        let attendu: u32 = std::env::var(VARIABLE_PERIODE)
            .expect("le parent arme la variable")
            .parse()
            .expect("le parent arme une valeur numérique");
        if cfg!(target_os = "linux") {
            assert_eq!(trames_de_periode(), Some(attendu));
        } else {
            assert_eq!(
                trames_de_periode(),
                None,
                "hors Linux, la variable ne doit RIEN changer"
            );
        }
    }

    /// La lecture de l'environnement n'est pas éprouvable en place : elle est
    /// mémoïsée par processus, et `set_var` dans une épreuve casse la suite.
    /// On relance donc le VRAI binaire d'épreuve avec la variable armée.
    #[test]
    fn la_variable_arme_la_periode_dans_un_vrai_processus() {
        let exe = std::env::current_exe().expect("binaire d'épreuve");
        let sortie = std::process::Command::new(exe)
            .args(["--exact", CHEMIN_ENFANT, "--nocapture"])
            .env(MARQUEUR_ENFANT, "1")
            .env(VARIABLE_PERIODE, "256")
            .output()
            .expect("relancer le binaire d'épreuve");
        let texte = format!(
            "{}{}",
            String::from_utf8_lossy(&sortie.stdout),
            String::from_utf8_lossy(&sortie.stderr)
        );
        assert!(
            sortie.status.success(),
            "l'enfant a échoué avec la période armée :\n{texte}"
        );
        // Sans ce contrôle, un filtre périmé rendrait « 0 passed » — un vert qui
        // ne garde rien.
        assert!(
            texte.contains("1 passed"),
            "le filtre « {CHEMIN_ENFANT} » ne désigne plus l'épreuve enfant :\n{texte}"
        );
    }
}
