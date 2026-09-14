//! **D4, en un seul endroit** : ce que vaut la lecture d'une piste de serveur
//! multimédia UPnP, sortie par sortie.
//!
//! Arbitrage de Bertrand, 14/09/2026 : *jouable partout, défauts assumés et
//! DITS*. Le tableau relevé par la reconnaissance du chantier
//! `unifier-serveurs-upnp-et-bibliotheque` :
//!
//! | sortie | ce qu'il se passe |
//! |---|---|
//! | réseau (DLNA / OpenHome) | joue — **sans DSP** : pas un octet ne traverse Tune |
//! | navigateur | joue — **sans DSP** : relais octet pour octet |
//! | locale | joue **avec** DSP — mais **sans ReplayGain**, et le **seek est cassé** |
//! | OAAT | **silence** — donc **refus explicite** |
//!
//! # Pourquoi une table, et pas deux listes qui se ressemblent
//!
//! Ce tableau a **deux lecteurs** :
//!
//! - `orchestrator::resolve_direct`, qui **refuse** la sortie OAAT avant de
//!   lancer quoi que ce soit ;
//! - `routes/playback.rs`, qui **dit** les dégradations dans la réponse de
//!   toutes les routes de lecture.
//!
//! Écrits séparément, ils auraient divergé au premier correctif : un refus levé
//! d'un côté, un avertissement resté de l'autre, et le produit mentirait sur ce
//! qu'il fait de l'audio. Ils lisent donc la même table, et
//! [`tests::le_refus_et_l_avertissement_disent_la_meme_chose`] refuse qu'ils se
//! séparent.
//!
//! # Ce que ce module ne fait pas
//!
//! Il ne **corrige** aucune de ces dégradations. Brancher le DSP sur les
//! sorties réseau et navigateur (le portage de #2863 qui n'a jamais eu lieu sur
//! ce chemin), rendre le ReplayGain disponible sans fichier local, et propager
//! `seek_ms` dans `resolve_direct.rs` — qui n'en contient toujours aucune
//! occurrence — restent à faire. Ce module fait la seule chose qui ne pouvait
//! pas attendre : **ne pas faire semblant**.

/// Les quatre familles de sortie que D4 distingue.
///
/// Ce n'est pas la liste des types de sortie du produit (il y en a davantage :
/// BluOS, Squeezebox, HQPlayer, Diretta…) mais celle des quatre **comportements
/// audio** distincts face à une URL distante. Tout ce qui reçoit une URI à
/// tirer lui-même rentre dans `Reseau`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortieD4 {
    /// Un renderer qui va chercher les octets lui-même (DLNA, OpenHome…).
    Reseau,
    /// Un onglet, qui tire `stream_url` de Tune.
    Navigateur,
    /// Une carte son de cette machine.
    Locale,
    /// Un point de sortie OAAT.
    Oaat,
}

impl SortieD4 {
    /// La famille d'une zone, d'après son type de sortie et son périphérique.
    ///
    /// Les préfixes sont ceux que l'orchestrateur emploie déjà
    /// (`resolve_direct.rs`) : `oaat:` / `oaat-group:` et `local:`. Une zone
    /// navigateur n'a **volontairement** aucun périphérique — l'onglet est la
    /// sortie —, d'où la lecture du type en base (#2076, #2158). Tout le reste
    /// est du réseau : c'est le repli le plus sûr, puisque c'est la famille qui
    /// n'a aucune exigence de format.
    pub fn depuis(output_type: Option<&str>, output_device_id: Option<&str>) -> Self {
        if let Some(id) = output_device_id {
            if id.starts_with("oaat:") || id.starts_with("oaat-group:") {
                return Self::Oaat;
            }
            if id.starts_with("local:") {
                return Self::Locale;
            }
            return Self::Reseau;
        }
        if output_type == Some("browser") {
            return Self::Navigateur;
        }
        Self::Reseau
    }

    /// Le nom de la sortie tel qu'il se lit dans un message destiné à un
    /// auditeur.
    pub fn nom(self) -> &'static str {
        match self {
            Self::Reseau => "réseau",
            Self::Navigateur => "navigateur",
            Self::Locale => "locale",
            Self::Oaat => "OAAT",
        }
    }

    /// Cette sortie peut-elle jouer une piste UPnP telle quelle ?
    ///
    /// `false` pour OAAT **seulement** : les trois autres jouent, avec les
    /// dégradations que [`Self::degradations`] énumère. Un flux déjà en WAV
    /// échappe au refus — c'est l'appelant qui le vérifie, parce que lui seul
    /// connaît le type MIME.
    pub fn joue_un_flux_compresse(self) -> bool {
        !matches!(self, Self::Oaat)
    }

    /// Ce que l'auditeur doit savoir **avant** d'entendre le résultat.
    ///
    /// Vide n'est pas une option pour les trois sorties jouantes : chacune a au
    /// moins un défaut, et le taire serait le contraire de ce que D4 demande.
    /// C'est ce que garde [`tests::aucune_sortie_jouante_ne_se_tait`].
    pub fn degradations(self) -> &'static [&'static str] {
        match self {
            Self::Reseau => &["Le DSP de la zone (égaliseur, convolveur, crossfeed) ne \
                 s'applique pas : le lecteur réseau va chercher les octets \
                 directement sur le serveur distant, ils ne traversent pas Tune."],
            Self::Navigateur => &["Le DSP de la zone (égaliseur, convolveur, crossfeed) ne \
                 s'applique pas : Tune relaie les octets du serveur distant \
                 tels quels."],
            Self::Locale => &[
                "Le ReplayGain ne s'applique pas : il est mesuré sur un fichier \
                 local, et cette piste n'en a pas.",
                "Un saut dans la piste la relance au début : la position n'est \
                 pas encore propagée sur ce chemin.",
            ],
            Self::Oaat => &[
                "Lecture refusée : un point de sortie OAAT ne lit que du PCM en \
                 conteneur WAV, et Tune ne sait pas encore convertir ce flux au \
                 fil de l'eau.",
            ],
        }
    }
}

/// Le motif du refus opposé à une sortie OAAT, **avant** que la piste ne soit
/// lancée.
///
/// Il nomme les quatre choses qu'un auditeur doit savoir pour agir : quelle
/// sortie, ce qu'elle sait lire, ce qui serait arrivé s'il n'y avait pas eu de
/// refus (le silence — c'est ce qui distingue un refus d'une panne), et où la
/// piste joue.
pub fn motif_du_refus_oaat(titre: &str, mime: &str) -> String {
    format!(
        "Lecture refusée : « {titre} » vient d'un serveur multimédia UPnP et \
         n'est publiée qu'en {mime}, alors qu'un point de sortie OAAT ne lit que \
         du PCM en conteneur WAV. Tune ne sait pas encore convertir ce flux au \
         fil de l'eau pour OAAT — la piste n'a pas été lancée, elle n'aurait \
         produit qu'un silence. Elle joue en revanche sur une zone réseau, \
         navigateur ou locale."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn les_quatre_familles_se_reconnaissent() {
        assert_eq!(
            SortieD4::depuis(Some("oaat"), Some("oaat:endpoint-1")),
            SortieD4::Oaat
        );
        assert_eq!(
            SortieD4::depuis(Some("oaat"), Some("oaat-group:salon")),
            SortieD4::Oaat
        );
        assert_eq!(
            SortieD4::depuis(Some("local"), Some("local:hw:0,0")),
            SortieD4::Locale
        );
        assert_eq!(
            SortieD4::depuis(Some("browser"), None),
            SortieD4::Navigateur
        );
        assert_eq!(
            SortieD4::depuis(Some("dlna"), Some("uuid:abc-renderer")),
            SortieD4::Reseau
        );
    }

    /// **La contre-épreuve de la reconnaissance** : une zone navigateur qui
    /// porterait un périphérique n'est plus un onglet, et une zone sans type ni
    /// périphérique retombe sur le réseau — la famille sans exigence de format,
    /// donc le repli qui ne promet rien.
    #[test]
    fn le_repli_ne_promet_rien() {
        assert_eq!(
            SortieD4::depuis(Some("browser"), Some("uuid:abc")),
            SortieD4::Reseau,
            "un périphérique nommé l'emporte sur le type de zone"
        );
        assert_eq!(SortieD4::depuis(None, None), SortieD4::Reseau);
    }

    /// Les trois sorties jouantes ont chacune au moins un défaut à dire. Si
    /// l'une venait à se taire, ce serait ou bien que quelqu'un a corrigé la
    /// dégradation — et il doit alors le prouver ailleurs — ou bien qu'on a
    /// recommencé à faire semblant.
    #[test]
    fn aucune_sortie_jouante_ne_se_tait() {
        for sortie in [SortieD4::Reseau, SortieD4::Navigateur, SortieD4::Locale] {
            assert!(
                !sortie.degradations().is_empty(),
                "la sortie {} ne dit plus aucune dégradation",
                sortie.nom()
            );
            assert!(sortie.joue_un_flux_compresse());
        }
    }

    /// La sortie locale est la seule à porter DEUX manques distincts, et les
    /// deux comptent : l'un est audible (le niveau), l'autre est un geste qui
    /// ne fait pas ce qu'on lui demande (le saut).
    #[test]
    fn la_sortie_locale_dit_ses_deux_manques() {
        let dites = SortieD4::Locale.degradations();
        assert!(
            dites.iter().any(|d| d.contains("ReplayGain")),
            "le manque de ReplayGain n'est plus dit"
        );
        assert!(
            dites.iter().any(|d| d.contains("saut")),
            "le seek cassé n'est plus dit"
        );
    }

    /// **Le verrou entre les deux lecteurs de la table.** Le refus opposé par
    /// l'orchestrateur et l'avertissement rendu par les routes de lecture
    /// doivent dire la même chose ; s'ils se séparaient, le produit refuserait
    /// pour une raison et en annoncerait une autre.
    #[test]
    fn le_refus_et_l_avertissement_disent_la_meme_chose() {
        assert!(
            !SortieD4::Oaat.joue_un_flux_compresse(),
            "OAAT doit rester la seule sortie qui refuse"
        );
        let avertissement = SortieD4::Oaat.degradations()[0];
        let motif = motif_du_refus_oaat("Wonderwall", "audio/x-flac");
        for mot in ["OAAT", "WAV"] {
            assert!(
                avertissement.contains(mot) && motif.contains(mot),
                "« {mot} » doit figurer des DEUX côtés : \
                 avertissement = {avertissement} / motif = {motif}"
            );
        }
        assert!(
            avertissement.contains("refusée") && motif.contains("refusée"),
            "les deux doivent dire que c'est un REFUS, pas une panne"
        );
        assert!(
            motif.contains("silence"),
            "le motif doit dire ce qui serait arrivé"
        );
        assert!(
            motif.contains("réseau") && motif.contains("navigateur"),
            "le motif doit dire où la piste joue"
        );
    }
}
