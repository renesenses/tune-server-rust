//! Le repli mono dit quand il n'agit pas, au lieu de se taire (#3254).
//!
//! ## Le défaut
//!
//! `zone_{id}_mono_downmix` (#2362) est **accepté** par `PATCH /zones/{id}` et
//! **relu** par `GET /zones/{id}` pour n'importe quelle zone — mais il n'est
//! appliqué que par [`LocalOutput`](crate::outputs::local::LocalOutput),
//! derrière la double garde `device_id.starts_with("local:")` +
//! `downcast_ref::<LocalOutput>()`, aux trois seuls sites qui le poussent.
//! Sur une zone réseau : accepté, persisté, relu… et sans effet.
//!
//! C'est le frère de #2742 (crossfeed) et le motif de #3192 (« mode exclusif »
//! décoché sans effet sous ASIO) : le défaut n'est pas la règle — le repli mono
//! est une correction de câblage d'une chaîne DSP locale, un renderer réseau
//! n'en a pas — le défaut est que **le réglage ment**.
//!
//! ## Ce qui distingue ce cas de celui du crossfeed
//!
//! Le **chemin du signal** dit déjà la vérité : `zone_mono_downmix_step`
//! (`tune-server/src/routes/zones.rs`) rend `None` hors `output_type ==
//! "local"`, et rend `None` en mode PURE. C'est le **réglage** qui se tait, pas
//! l'affichage de la chaîne. Le mensonge est donc plus discret — mais c'est le
//! même : l'interrupteur reste franchement armé dans la fiche de zone.
//!
//! ## Pourquoi la règle vit ici et pas dans la route
//!
//! `output_is_local` et `audiophile` sont des **paramètres**, pas des lectures :
//! la règle est éprouvable sans monter une zone ni une sortie, et sur une cible
//! compilée **sans** `local-audio` — où les trois sites d'installation
//! n'existent même pas, ce qui ne rend le réglage que plus muet. Même intention
//! que le `on_windows` d'[`exclusive_mode_status`](crate::config::exclusive_mode_status).
//!
//! ## Vocabulaire
//!
//! `reason` (code stable, pour la machine) / `detail` (phrase en clair, pour un
//! écran sans table de traduction) / le triplet `requested` / `effective` /
//! `unavailable` : c'est celui d'`ExclusiveModeStatus` (#3243, `/system/config`)
//! et de `CrossfeedStatus` (#2742, `GET`/`PUT /zones/{id}/dsp`). Un client qui
//! sait lire l'un lit celui-ci avec la même forme de code.
//!
//! ⚠️ Trois canaux, trois domaines, et celui-ci est le troisième :
//! `LocalBackendStatus.device` dit ce qui est **constaté à la lecture** ;
//! `/system/config` dit une **contrainte de serveur** ; le document du réglage
//! lui-même — ici `GET`/`PATCH /zones/{id}`, celui qui porte déjà
//! `mono_downmix` — dit une **disponibilité par zone**. La disponibilité du
//! repli mono EST une propriété de la zone : le même serveur porte au même
//! instant une zone `local:` où il s'applique et une zone `dlna:` où il n'a
//! aucun chemin.

use serde::Serialize;

/// Pourquoi le repli mono demandé n'agira pas.
///
/// Les codes sont **stables** et destinés à la machine (le client les traduit),
/// sur le modèle d'`ExclusiveModeConstraint` (#3192) et de `LocalBackendFallback`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MonoDownmixConstraint {
    /// La zone ne sort pas par une carte son locale. Les trois sites qui
    /// poussent le repli exigent `device_id.starts_with("local:")` **et** un
    /// `LocalOutput` ; un renderer DLNA / AirPlay / Chromecast / BluOS /
    /// OpenHome / Squeezebox / HQPlayer décode chez lui, et le seul chemin
    /// serveur qui pourrait le toucher — `transcode_source_to_file` — ne
    /// transporte ni ne connaît le repli.
    NonLocalOutput,
    /// Mode PURE (audiophile) : le PCM atteint la sortie intact. Sommer les
    /// deux voies réécrirait chaque échantillon, ce que PURE promet justement
    /// de ne pas faire — `zone_mono_downmix_with` rend donc `false` sans même
    /// lire la clé.
    PureMode,
}

impl MonoDownmixConstraint {
    /// Code stable, celui que porte la charge utile JSON.
    pub fn code(self) -> &'static str {
        match self {
            Self::NonLocalOutput => "non_local_output",
            Self::PureMode => "pure_mode",
        }
    }

    /// Phrase courte, dans la langue du chemin du signal — le serveur y écrit
    /// déjà ses `detail` en français.
    pub fn detail(self) -> &'static str {
        match self {
            Self::NonLocalOutput => {
                "La sortie mono est appliquée par la chaîne audio LOCALE du \
                 serveur (carte son ou DAC branché dessus). Cette zone n'en a \
                 pas : un lecteur réseau décode et rend chez lui, le serveur \
                 n'a pas la main sur ses échantillons. Le réglage reste \
                 enregistré et redeviendra actif si la zone repasse sur une \
                 sortie locale."
            }
            Self::PureMode => {
                "Le mode PURE laisse le signal intact jusqu'au DAC. Sommer les \
                 deux voies réécrirait chaque échantillon, donc la sortie mono \
                 est désarmée tant que PURE est actif. Le réglage reste \
                 enregistré et redeviendra actif dès la sortie du mode PURE."
            }
        }
    }

    /// Toutes les variantes. Sert la contre-épreuve permanente : une contrainte
    /// ajoutée sans code ni libellé fait tomber le test qui parcourt cette
    /// liste.
    pub const ALL: [Self; 2] = [Self::NonLocalOutput, Self::PureMode];
}

/// Ce que le repli mono VAUT réellement pour une zone, à côté de ce que le
/// réglage demande — et pourquoi, quand les deux diffèrent.
///
/// **Additif** : aucun champ ne remplace `mono_downmix`, qui reste publié tel
/// quel, à sa valeur PERSISTÉE. Un client qui ne lit pas cette structure voit
/// le même écran qu'avant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MonoDownmixStatus {
    /// Ce que l'utilisateur a demandé — la valeur persistée, celle que la
    /// fiche de zone publie déjà dans `mono_downmix`.
    pub requested: bool,
    /// Ce qui sera réellement appliqué au signal de cette zone.
    pub effective: bool,
    /// `true` dès que la contrainte s'applique — **y compris quand la case
    /// était déjà décochée**. C'est ce champ qui doit VERROUILLER le contrôle :
    /// la question n'est pas « le réglage a-t-il été changé ? » mais « ce
    /// réglage a-t-il encore un sens ici ? ».
    pub unavailable: bool,
    /// Pourquoi. `None` = le réglage est honoré tel quel.
    pub reason: Option<MonoDownmixConstraint>,
    /// La même chose en clair, pour un écran qui n'a pas de table de
    /// traduction.
    pub detail: Option<&'static str>,
}

/// Le repli mono a-t-il un chemin sur CETTE sortie ?
///
/// Interroge **exactement** le prédicat des trois sites d'installation
/// (`orchestrator.rs`) : `device_id.starts_with("local:")`. C'est ce préfixe,
/// et lui seul, qui dit à l'orchestrateur « carte son » plutôt que « renderer
/// réseau ». Une seule règle, donc pas de dérive possible entre l'écran et le
/// son.
///
/// `None` — une zone orpheline, sans périphérique assigné — rend `false` : elle
/// n'a aucune sortie locale, donc rien n'appliquera le repli. C'est pourquoi le
/// libellé de [`MonoDownmixConstraint::NonLocalOutput`] nomme ce qui est REQUIS
/// (« cette zone n'a pas de chaîne locale ») plutôt que ce que la zone serait.
pub fn mono_downmix_runs_on_output(output_device_id: Option<&str>) -> bool {
    output_device_id.is_some_and(|id| id.starts_with("local:"))
}

/// La règle, isolée de toute base de données et de tout `cfg` pour être
/// vérifiable partout.
///
/// L'ordre des contraintes n'est pas neutre : une sortie réseau PRIME sur le
/// mode PURE. Sortir du mode PURE ne rendrait rien à cette zone-là, alors que
/// l'inverse est vrai sur une zone locale — la raison publiée doit donc être
/// celle qui reste vraie quand l'autre disparaît.
pub fn mono_downmix_status(
    requested: bool,
    output_is_local: bool,
    audiophile: bool,
) -> MonoDownmixStatus {
    let reason = if !output_is_local {
        Some(MonoDownmixConstraint::NonLocalOutput)
    } else if audiophile {
        Some(MonoDownmixConstraint::PureMode)
    } else {
        None
    };
    let unavailable = reason.is_some();
    MonoDownmixStatus {
        requested,
        effective: requested && !unavailable,
        unavailable,
        reason,
        detail: reason.map(MonoDownmixConstraint::detail),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn une_sortie_locale_honore_le_repli_sans_rien_annoncer() {
        let s = mono_downmix_status(true, true, false);
        assert!(s.requested);
        assert!(
            s.effective,
            "le témoin : sur une sortie locale, le repli agit"
        );
        assert!(!s.unavailable);
        assert_eq!(s.reason, None);
        assert_eq!(s.detail, None);
    }

    /// Et le cas nominal DÉSARMÉ ne doit rien annoncer non plus : sans ce
    /// second témoin, une règle qui dirait « indisponible » dès que la case est
    /// décochée passerait le test ci-dessus.
    #[test]
    fn une_sortie_locale_desarmee_nannonce_rien_non_plus() {
        let s = mono_downmix_status(false, true, false);
        assert!(!s.effective);
        assert!(!s.unavailable);
        assert_eq!(s.reason, None);
    }

    #[test]
    fn une_sortie_reseau_dit_que_le_repli_nagit_pas() {
        let s = mono_downmix_status(true, false, false);
        assert!(s.requested, "la demande n'est pas effacée");
        assert!(!s.effective);
        assert!(s.unavailable);
        assert_eq!(s.reason, Some(MonoDownmixConstraint::NonLocalOutput));
        assert!(s.detail.is_some());
    }

    /// `unavailable` ne dépend PAS de la case : c'est lui qui verrouille le
    /// contrôle, pas la valeur demandée. Une zone réseau case décochée doit
    /// dire la même indisponibilité — sinon l'utilisateur coche, puis découvre.
    #[test]
    fn une_sortie_reseau_verrouille_meme_case_decochee() {
        let s = mono_downmix_status(false, false, false);
        assert!(!s.effective);
        assert!(s.unavailable);
        assert_eq!(s.reason, Some(MonoDownmixConstraint::NonLocalOutput));
    }

    #[test]
    fn le_mode_pure_desarme_le_repli_et_le_dit() {
        let s = mono_downmix_status(true, true, true);
        assert!(!s.effective);
        assert!(s.unavailable);
        assert_eq!(s.reason, Some(MonoDownmixConstraint::PureMode));
    }

    /// La sortie réseau prime : sortir de PURE ne rendrait toujours rien.
    #[test]
    fn la_sortie_reseau_prime_sur_le_mode_pure() {
        let s = mono_downmix_status(true, false, true);
        assert_eq!(s.reason, Some(MonoDownmixConstraint::NonLocalOutput));
    }

    /// Le prédicat est celui des sites, à la lettre.
    #[test]
    fn seul_le_prefixe_local_porte_le_repli() {
        assert!(mono_downmix_runs_on_output(Some("local:hw:CARD=DAC")));
        assert!(!mono_downmix_runs_on_output(Some("uuid-renderer-dlna")));
        assert!(!mono_downmix_runs_on_output(Some("chromecast:salon")));
        // Une zone orpheline n'a aucune chaîne locale : rien ne l'appliquera.
        assert!(!mono_downmix_runs_on_output(None));
        // Le préfixe se lit au DÉBUT, pas n'importe où.
        assert!(!mono_downmix_runs_on_output(Some("dlna:local:piege")));
    }

    /// Contre-épreuve permanente : toute contrainte ajoutée doit porter un code
    /// stable, un libellé, et se sérialiser en son code — pas en son nom Rust.
    #[test]
    fn le_code_serialise_est_le_code_stable() {
        for motif in MonoDownmixConstraint::ALL {
            assert!(!motif.code().is_empty());
            assert!(
                motif.detail().len() > 40,
                "{} : le libellé doit EXPLIQUER, pas nommer",
                motif.code()
            );
            assert_eq!(
                serde_json::to_value(motif).expect("sérialisable"),
                serde_json::Value::String(motif.code().to_string()),
            );
        }
    }
}

/// GARDE DE SITE — la production doit continuer à ne pousser le repli mono que
/// derrière la double garde locale (#3254).
///
/// La règle publiée par `mono_downmix_status` affirme une PRÉMISSE sur le code
/// de production : le repli n'atteint que `LocalOutput`. Aucune épreuve de
/// comportement ne peut voir cette prémisse s'inverser — retirer une garde ne
/// fait rien tomber, ça élargit silencieusement la portée, et c'est alors le
/// `reason: "non_local_output"` qui se met à mentir **dans l'autre sens**.
///
/// On relit donc la production. Idiome du dépôt : `terminologie_eq.rs`,
/// `position_publiee_guard` (`poller.rs`), et
/// `find_device_with_fallback_passe_bien_l_hote_d_origine` (`local.rs`).
#[cfg(test)]
mod garde_de_site {
    /// Chaque poussée du repli, avec sa fonction englobante.
    ///
    /// La remontée s'arrête au début de la fonction : une garde posée chez le
    /// voisin ne garde rien, et une simple fenêtre de N lignes resterait verte
    /// contre une garde supprimée juste au-dessus du site.
    fn fonction_englobante(source: &str, site: usize) -> &str {
        let debut = [
            "\n    pub async fn ",
            "\n    pub fn ",
            "\n    async fn ",
            "\n    fn ",
            "\n    pub(super) fn ",
            "\n    pub(super) async fn ",
        ]
        .iter()
        .filter_map(|motif| source[..site].rfind(motif))
        .max()
        .expect("tout site vit dans une méthode d'impl");
        &source[debut..site]
    }

    fn numero_de_ligne(source: &str, index: usize) -> usize {
        source[..index].matches('\n').count() + 1
    }

    #[test]
    fn aucun_site_de_repli_mono_hors_de_la_garde_locale() {
        // Le bloc `impl PlaybackOrchestrator` est réparti par familles (REF-2,
        // #2219) : on lit chaque fichier qui porte un site, mis bout à bout.
        const SOURCE: &str = concat!(
            include_str!("../orchestrator.rs"),
            include_str!("../orchestrator/dsp.rs"),
            include_str!("../orchestrator/transport.rs"),
        );
        const APPEL: &str = "local_output.set_mono_downmix(";

        let mut sites = 0usize;
        let mut reste = SOURCE;
        let mut offset = 0usize;
        while let Some(pos) = reste.find(APPEL) {
            let site = offset + pos;
            sites += 1;
            let corps = fonction_englobante(SOURCE, site);
            let ligne = numero_de_ligne(SOURCE, site);
            assert!(
                corps.contains("starts_with(\"local:\")"),
                "site de repli mono sans garde `local:` dans sa propre fonction \
                 (orchestrator.rs + dsp.rs + transport.rs, ligne {ligne}). Si le repli atteint désormais \
                 une sortie NON locale, `mono_downmix_status` ment et doit être \
                 corrigé AVEC ce site (#3254)."
            );
            assert!(
                corps.contains("LocalOutput"),
                "site de repli mono sans `downcast_ref::<LocalOutput>()` dans sa \
                 propre fonction (orchestrator.rs + dsp.rs + transport.rs, ligne {ligne}) — la seconde \
                 moitié de la garde qu'annonce `non_local_output` (#3254)."
            );
            offset = site + APPEL.len();
            reste = &SOURCE[offset..];
        }
        assert_eq!(
            sites, 3,
            "trois sites poussent le repli mono vers la sortie locale \
             (chemin de lecture, `refresh_zone_pure_dsp`, \
             `refresh_zone_mono_downmix`). Le compte a changé : le site ajouté \
             ou retiré doit être confronté à `mono_downmix_status` (#3254)."
        );
    }

    /// La prémisse INVERSE : le chemin réseau ne porte pas le repli.
    ///
    /// `transcode_source_to_file` est la seule porte serveur qui traite le PCM
    /// destiné à un renderer. Elle prend `eq`, `convolver`, `replaygain` — et
    /// rien d'autre. Le jour où elle prendrait le repli mono,
    /// `MonoDownmixConstraint::NonLocalOutput` deviendrait faux, et ce test
    /// l'exigera AVANT que l'écran ne se mette à mentir dans l'autre sens.
    ///
    /// ⛔ Élargir le repli au chemin transcodé n'est pas la correction : FLAC,
    /// WAV, MP3 et AAC atteignent une zone réseau SANS transcodage. L'effet ne
    /// s'appliquerait qu'aux formats exotiques — actif sur une piste, muet sur
    /// la suivante, dans la même zone. Un mensonge pire que le silence.
    #[test]
    fn le_chemin_transcode_ne_porte_toujours_pas_le_repli_mono() {
        const SOURCE: &str = include_str!("../orchestrator.rs");
        let debut = SOURCE
            .find("async fn transcode_source_to_file(")
            .expect("`transcode_source_to_file` a été renommée — cette garde ne garde plus rien");
        let signature = &SOURCE[debut..];
        let fin = signature
            .find(") -> ")
            .expect("fin de signature introuvable");
        let signature = &signature[..fin];
        assert!(
            !signature.contains("mono"),
            "`transcode_source_to_file` porte désormais le repli mono : la \
             raison `non_local_output` de `mono_downmix_status` est devenue \
             fausse et doit être corrigée AVEC ce changement (#3254). \
             Signature lue : {signature}"
        );
    }
}
