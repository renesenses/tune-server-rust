//! #3208 — le seul endroit du dépôt qui traduit la décision de période en
//! `cpal::BufferSize`.
//!
//! La décision elle-même vit dans [`crate::audio::periode_alsa`], hors de
//! `local-audio` : elle ne dépend ni de cpal ni d'une carte son, et la porte
//! `test` de la CI ne compile pas `local-audio`. Ici, il ne reste que
//! l'adaptation aux types de cpal — et c'est ici que
//! `cpal::BufferSize::Default` est écrit, une fois, pour tout le fichier
//! `local.rs` et ses modules.

use crate::audio::periode_alsa;

/// La taille de période à demander au pilote.
pub(super) fn taille_de_periode() -> cpal::BufferSize {
    match periode_alsa::trames_de_periode() {
        Some(trames) => cpal::BufferSize::Fixed(trames),
        None => cpal::BufferSize::Default,
    }
}

/// Le `StreamConfig` que la sortie locale fabrique quand elle choisit
/// elle-même ses paramètres.
pub(super) fn config_de_flux(channels: u16, sample_rate: u32) -> cpal::StreamConfig {
    cpal::StreamConfig {
        channels,
        sample_rate,
        buffer_size: taille_de_periode(),
    }
}

/// Impose la période à un `StreamConfig` VENU D'AILLEURS.
///
/// Indispensable, et c'est le piège de ce ticket : aux sites nominaux, la
/// configuration ne sort pas d'un littéral mais de
/// `device.default_output_config()`, dont le `buffer_size` est
/// `BufferSize::Default`. Ne corriger que les littéraux aurait laissé le chemin
/// le plus fréquent — celui d'un périphérique qui annonce une cadence par
/// défaut — ouvrir sans période, et la garde aurait gardé un branchement que la
/// production n'emprunte pas.
///
/// Sans période armée (et sur toute plateforme non-Linux), la configuration est
/// rendue INCHANGÉE : on n'écrase jamais un `buffer_size` par `Default`.
pub(super) fn avec_periode(mut cfg: cpal::StreamConfig) -> cpal::StreamConfig {
    if let Some(trames) = periode_alsa::trames_de_periode() {
        cfg.buffer_size = cpal::BufferSize::Fixed(trames);
    }
    cfg
}

/// Le seuil de préchargement du rappel, déduit de la période RÉELLEMENT
/// demandée dans `cfg` — pas d'une seconde lecture de l'environnement.
///
/// `garde_par_defaut` est le compte d'aujourd'hui, en échantillons entrelacés.
/// Sans période imposée, il est rendu tel quel.
pub(super) fn garde_de_prechargement(cfg: &cpal::StreamConfig, garde_par_defaut: usize) -> usize {
    let trames = match cfg.buffer_size {
        cpal::BufferSize::Fixed(trames) => Some(trames),
        cpal::BufferSize::Default => None,
    };
    periode_alsa::garde_de_prechargement(trames, garde_par_defaut, cfg.channels)
}
