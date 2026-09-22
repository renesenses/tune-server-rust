//! Crossfeed plugin compatibility and host applicability status.
pub struct CrossfeedProcessor {
    engine: CrossfeedEngine,
    amount: f32,
    delay_samples: usize,
    /// #4685 — niveau moyen du filtre, calculé UNE fois à la construction
    /// (voir [`Self::gain_moyen_db`]).
    gain_moyen_db: f64,
}
enum CrossfeedEngine {
    Bundled(tune_plugin_crossfeed::CrossfeedProcessor),
    Native(tune_plugin_native::stage::Stage),
    Unavailable,
}
impl CrossfeedProcessor {
    pub fn new(sample_rate: u32, amount: f32, delay_ms: f32) -> Self {
        let reference =
            tune_plugin_crossfeed::CrossfeedProcessor::new(sample_rate, amount, delay_ms);
        let delay_samples = reference.delay_samples();
        let engine = if tune_plugin_native::failure("crossfeed").is_some() {
            CrossfeedEngine::Unavailable
        } else if let Some(provider) = tune_plugin_native::provider("crossfeed") {
            match tune_plugin_native::stage::Stage::prepare(
                provider,
                sample_rate,
                2,
                &serde_json::json!({"enabled":amount!=0.0,"amount":amount,"delay_ms":delay_ms}),
            ) {
                Ok(stage) => CrossfeedEngine::Native(stage),
                Err(error) => {
                    tracing::error!(%error,"native_crossfeed_prepare_failed");
                    CrossfeedEngine::Unavailable
                }
            }
        } else {
            CrossfeedEngine::Bundled(reference)
        };
        // Un moteur indisponible ne mélange rien : rien à compenser.
        let gain_moyen_db = if matches!(engine, CrossfeedEngine::Unavailable) {
            0.0
        } else {
            tune_plugin_crossfeed::gain_moyen_db(sample_rate, amount, delay_ms)
        };
        Self {
            engine,
            amount,
            delay_samples,
            gain_moyen_db,
        }
    }

    /// #4685 — ce que ce crossfeed fait gagner ou perdre au niveau MOYEN d'un
    /// canal, en dB, sur un bruit rose stéréo de corrélation
    /// `tune_plugin_crossfeed::CORRELATION_DE_REFERENCE`. Calculé depuis le
    /// filtre, pas depuis la musique : c'est un gain FIXE.
    pub fn gain_moyen_db(&self) -> f64 {
        self.gain_moyen_db
    }
    pub fn process_interleaved(&mut self, samples: &mut [f32]) {
        match &mut self.engine {
            CrossfeedEngine::Bundled(p) => p.process_interleaved(samples),
            CrossfeedEngine::Native(p) => {
                if let Err(error) = p.process_f32(samples) {
                    tracing::error!(%error,"native_crossfeed_processing_failed");
                }
            }
            CrossfeedEngine::Unavailable => {}
        }
    }
    pub fn process_pcm(&mut self, pcm: &mut [u8], bit_depth: u16, channels: u16) {
        if channels != 2 {
            return;
        }
        match &mut self.engine {
            CrossfeedEngine::Bundled(p) => p.process_pcm(pcm, bit_depth, channels),
            CrossfeedEngine::Native(p) => {
                if let Err(error) = p.process_pcm(pcm, bit_depth) {
                    tracing::error!(%error,"native_crossfeed_processing_failed");
                }
            }
            CrossfeedEngine::Unavailable => {}
        }
    }
    pub fn inherit_state_from(&mut self, previous: &Self) {
        match (&mut self.engine, &previous.engine) {
            (CrossfeedEngine::Bundled(new), CrossfeedEngine::Bundled(old)) => {
                new.inherit_state_from(old)
            }
            (CrossfeedEngine::Native(new), CrossfeedEngine::Native(old)) => {
                if let Err(error) = new.inherit(old) {
                    tracing::debug!(%error,"native_crossfeed_history_not_compatible");
                }
            }
            _ => {}
        }
    }
    pub fn amount(&self) -> f32 {
        if matches!(self.engine, CrossfeedEngine::Unavailable) {
            0.0
        } else {
            self.amount
        }
    }
    pub fn delay_samples(&self) -> usize {
        self.delay_samples
    }
}

/// Contrainte qui prive le réglage « crossfeed » de son effet.
///
/// #2742 — Tades : « Crossfeed n'a aucune action ». Le serveur avait raison sur
/// le fond — le crossfeed est un effet de CASQUE, il n'est appliqué que par la
/// sortie locale — mais il l'imposait EN SILENCE. `GET /zones/{id}/dsp` rend le
/// réglage pour n'importe quelle zone, `PUT` le persiste pour n'importe quelle
/// zone, et les TROIS sites qui installent réellement un `CrossfeedProcessor`
/// (`orchestrator.rs` : le chemin de lecture, `refresh_zone_crossfeed`,
/// `refresh_zone_pure_dsp`) sont tous derrière la même double garde
/// `device_id.starts_with("local:")` + `downcast_ref::<LocalOutput>()`. Sur une
/// zone réseau le crossfeed n'a littéralement aucun chemin de code : la case
/// s'allume, la valeur part en base, et rien ne se passe.
///
/// Même défaut que #3192 (« mode exclusif » décoché sans effet sous ASIO) :
/// le défaut n'est pas la règle, c'est que le réglage MENT. D'où le même
/// vocabulaire — un `code()` stable pour la machine, un `detail()` en clair
/// pour un écran sans table de traduction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CrossfeedConstraint {
    /// The saved configuration survives the Free -> Premium transition.
    PremiumRequired,
    /// Entitled, but the provider is disabled, uninstalled, or failed to load.
    PluginUnavailable,
    /// La zone ne joue ni par une sortie LOCALE, ni par une sortie RÉSEAU :
    /// OAAT, navigateur, sorties PULL, ou zone dont aucun périphérique n'est
    /// résolu (elle ne joue nulle part).
    ///
    /// Depuis LAT-F1 ce motif ne couvre plus les zones réseau — elles ont leurs
    /// deux motifs propres. Il reste la réponse PRUDENTE pour les familles dont
    /// le chemin n'a pas été mesuré : certaines traversent probablement le
    /// relais progressif, mais annoncer « disponible » sans preuve serait le
    /// défaut de #2742 pris à l'envers.
    NonLocalOutput,
    /// Le mode PURE (audiophile) désarme volontairement le crossfeed pour
    /// garder le chemin bit-perfect (`load_crossfeed_processor` rend `None`).
    /// C'est un choix assumé, pas une panne — mais tant qu'il dure, le réglage
    /// est sans effet et l'écran doit le dire.
    PureMode,
    /// Zone RÉSEAU dont le flux progressif n'est pas armé. Depuis LAT-F1, le
    /// crossfeed d'une zone réseau est appliqué par le relais du bras
    /// progressif ; ce bras n'est emprunté que si l'opt-in global
    /// `dsp_progressif_reseau` (Réglages → Lecture) est coché. À froid, la
    /// zone part encore par le fichier pré-transcodé, qui ne porte aucun
    /// crossfeed — et c'est un garde-fou du dépôt, pas un oubli.
    NetworkProgressiveOff,
    /// Zone RÉSEAU dont le renderer n'a pas annoncé le LPCM à la profondeur
    /// servie (sonde `GetProtocolInfo`, réponse inconcluante comprise). Le
    /// bras progressif sert du WAV : un renderer qui ne le déclare pas est
    /// renvoyé au fichier, donc sans crossfeed.
    NetworkRendererNoLpcm,
}

impl CrossfeedConstraint {
    /// Code stable, celui que porte la charge utile JSON.
    pub fn code(self) -> &'static str {
        match self {
            Self::PremiumRequired => "premium_required",
            Self::PluginUnavailable => "plugin_unavailable",
            Self::NonLocalOutput => "non_local_output",
            Self::PureMode => "pure_mode",
            Self::NetworkProgressiveOff => "network_progressive_off",
            Self::NetworkRendererNoLpcm => "network_renderer_no_lpcm",
        }
    }

    /// Phrase courte, dans la langue du chemin du signal — le serveur y écrit
    /// déjà ses `detail` en français.
    pub fn detail(self) -> &'static str {
        match self {
            Self::PremiumRequired => {
                "Le crossfeed fait désormais partie de Tune Premium. Vos réglages sont conservés pour sa réactivation avec Premium."
            }
            Self::PluginUnavailable => {
                "Le greffon crossfeed est indisponible. Réactivez-le dans les greffons ; vos réglages sont conservés."
            }
            Self::NonLocalOutput => {
                "Le crossfeed est un effet de casque : il n'est appliqué que par \
                 une sortie LOCALE (DAC USB ou carte son de la machine). Cette \
                 zone ne joue pas par une sortie locale, le réglage est \
                 enregistré mais n'atteint pas le son. Pour l'entendre, écoutez \
                 sur une zone à sortie locale."
            }
            Self::PureMode => {
                "Le mode PURE garde le chemin bit-perfect : aucun traitement ne \
                 touche le signal, crossfeed compris. Le réglage est conservé et \
                 reprendra effet dès que le mode PURE sera désactivé."
            }
            Self::NetworkProgressiveOff => {
                "Sur une zone réseau, le crossfeed passe par le flux progressif. \
                 Activez « DSP progressif réseau » dans Réglages → Lecture pour \
                 l'entendre ; sans cette option la zone reçoit un fichier \
                 pré-transcodé, qui ne porte pas le crossfeed."
            }
            Self::NetworkRendererNoLpcm => {
                "Ce lecteur réseau n'annonce pas savoir lire le PCM non compressé \
                 à cette profondeur. Le flux progressif — le seul chemin du \
                 crossfeed sur une zone réseau — ne peut donc pas lui être servi. \
                 Le réglage est conservé et vaudra pour une autre sortie."
            }
        }
    }

    /// Toutes les variantes. Sert la contre-épreuve permanente : une contrainte
    /// ajoutée sans code ni libellé fait tomber le test qui parcourt cette liste.
    pub const ALL: [Self; 6] = [
        Self::PremiumRequired,
        Self::PluginUnavailable,
        Self::NonLocalOutput,
        Self::PureMode,
        Self::NetworkProgressiveOff,
        Self::NetworkRendererNoLpcm,
    ];
}

/// Ce que le crossfeed VAUT réellement pour une zone, à côté de ce que le
/// réglage demande — et pourquoi, quand les deux diffèrent.
///
/// Additif : aucun champ ne remplace l'objet `crossfeed` de
/// `GET/PUT /zones/{id}/dsp`, qui reste publié tel quel. Un client qui ne lit
/// pas cette structure voit le même écran qu'avant.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct CrossfeedStatus {
    /// Ce que l'utilisateur a demandé (la case `enabled` du réglage persisté).
    pub requested: bool,
    /// Ce qui sera réellement appliqué au son de cette zone.
    pub effective: bool,
    /// `true` dès que la contrainte s'applique — **y compris quand la case
    /// était déjà décochée**. C'est ce champ qui doit VERROUILLER le contrôle :
    /// la question n'est pas « le réglage a-t-il été changé ? » mais « ce
    /// réglage a-t-il encore un sens sur cette zone ? ».
    pub unavailable: bool,
    /// Pourquoi. `None` = le réglage est honoré tel quel.
    pub reason: Option<CrossfeedConstraint>,
    /// La même chose en clair, pour un écran qui n'a pas de table de traduction.
    pub detail: Option<&'static str>,
}

/// Le prédicat des trois sites d'installation, écrit UNE fois.
///
/// `orchestrator.rs` garde chacun d'eux par `device_id.starts_with("local:")`,
/// et `create_zone` le dit noir sur blanc : « une sortie locale s'identifie par
/// `local:{nom}` — c'est ce préfixe, et lui seul, qui dit à l'orchestrateur
/// "carte son" plutôt que "renderer réseau" ». Le statut publié doit donc
/// interroger EXACTEMENT ce préfixe, sinon l'écran et le son se répondraient
/// sur deux règles différentes.
///
/// `None` (zone sans périphérique résolu) rend `false` : elle ne joue nulle
/// part, donc pas davantage par une sortie locale.
pub fn crossfeed_runs_on_output(output_device_id: Option<&str>) -> bool {
    output_device_id.is_some_and(|d| d.starts_with("local:"))
}

/// La règle, isolée de toute base de données pour être vérifiable partout.
///
/// `output_is_local` et `audiophile` sont des PARAMÈTRES, pas des lectures : la
/// règle doit être éprouvable sans monter une zone ni une sortie, et sur une
/// cible compilée sans `local-audio` — où les trois sites d'installation
/// n'existent même pas, ce qui ne rend le réglage que plus muet. Même intention
/// que le `on_windows` d'`exclusive_mode_status` (#3192).
///
/// Ordre des motifs : une zone réseau ne verra JAMAIS de crossfeed, PURE ou
/// non ; c'est donc `NonLocalOutput` qui prime, parce que c'est celui qui ne
/// se lève pas en décochant une case.
pub fn crossfeed_status(
    requested: bool,
    output_is_local: bool,
    output_is_network: bool,
    audiophile: bool,
    progressif_arme: bool,
    renderer_accepte_lpcm: bool,
) -> CrossfeedStatus {
    // L'ORDRE a changé avec LAT-F1, et le changement est le sujet.
    //
    // Avant, `NonLocalOutput` primait : une zone réseau ne verrait JAMAIS de
    // crossfeed, PURE ou non, donc autant nommer la contrainte définitive.
    // Depuis que le bras progressif porte le crossfeed, ce n'est plus vrai —
    // une zone réseau PEUT l'entendre. La contrainte qu'aucun chemin
    // n'esquive est désormais le mode PURE : `load_crossfeed_processor` rend
    // `None`, quelle que soit la sortie. C'est donc lui qui passe devant.
    let reason = if audiophile {
        Some(CrossfeedConstraint::PureMode)
    } else if output_is_local {
        None
    } else if output_is_network {
        // Les deux conditions du bras progressif, dans l'ordre où
        // l'utilisateur peut agir dessus : l'opt-in est une case qu'il coche,
        // le LPCM du renderer ne se négocie pas.
        if !progressif_arme {
            Some(CrossfeedConstraint::NetworkProgressiveOff)
        } else if !renderer_accepte_lpcm {
            Some(CrossfeedConstraint::NetworkRendererNoLpcm)
        } else {
            None
        }
    } else {
        // Ni locale, ni réseau : OAAT, navigateur, sorties PULL, zone sans
        // périphérique résolu. Elles ne sont PAS couvertes par ce correctif —
        // certaines traversent probablement le relais (OAAT transcode en WAV
        // par session), mais ce n'est pas mesuré, et annoncer « disponible »
        // sans preuve est exactement le défaut de #2742 pris à l'envers.
        Some(CrossfeedConstraint::NonLocalOutput)
    };
    let unavailable = reason.is_some();
    CrossfeedStatus {
        requested,
        effective: requested && !unavailable,
        unavailable,
        reason,
        detail: reason.map(CrossfeedConstraint::detail),
    }
}

/// Ajoute les DROITS (licence, greffon) au statut que la sortie impose — #4511.
///
/// L'ordre des motifs est une promesse faite à l'utilisateur : on nomme d'abord
/// ce qu'aucun geste ne lèvera. Une sortie qui ne portera jamais le crossfeed
/// (`NonLocalOutput` : Diretta et les autres sorties pull, OAAT, navigateur ;
/// `NetworkRendererNoLpcm` : un renderer qui ne lit pas le PCM) le dit AVANT
/// de parler de licence. Sinon l'écran invite à passer Premium ou à activer un
/// greffon pour un effet que cette zone n'entendra de toute façon pas — c'est
/// ce qu'a vu Ludovic Audouin sur sa zone Diretta en 0.9.156.
///
/// Viennent ensuite les droits, puis ce que l'utilisateur lève d'un geste sur
/// la zone elle-même (`PureMode`, `NetworkProgressiveOff`), déjà rangés par
/// [`crossfeed_status`].
pub fn avec_les_droits(
    sortie: CrossfeedStatus,
    premium: bool,
    greffon_actif: bool,
) -> CrossfeedStatus {
    if matches!(
        sortie.reason,
        Some(CrossfeedConstraint::NonLocalOutput | CrossfeedConstraint::NetworkRendererNoLpcm)
    ) {
        return sortie;
    }
    let droit = if !premium {
        Some(CrossfeedConstraint::PremiumRequired)
    } else if !greffon_actif {
        Some(CrossfeedConstraint::PluginUnavailable)
    } else {
        None
    };
    match droit {
        Some(reason) => CrossfeedStatus {
            requested: sortie.requested,
            effective: false,
            unavailable: true,
            reason: Some(reason),
            detail: Some(reason.detail()),
        },
        None => sortie,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------
    // #4511 — les droits ne passent plus devant la sortie.
    // -----------------------------------------------------------------

    /// Le cas de Ludovic : zone Diretta (sortie pull, ni locale ni réseau),
    /// licence Free. L'écran doit dire que la SORTIE ne porte pas l'effet, pas
    /// l'inviter à payer pour rien.
    #[test]
    fn une_sortie_pull_sans_premium_dit_la_sortie_pas_la_licence() {
        let sortie = crossfeed_status(true, false, false, false, false, false);
        let s = avec_les_droits(sortie, false, false);
        assert_eq!(s.reason, Some(CrossfeedConstraint::NonLocalOutput));
        assert!(s.unavailable && !s.effective);
    }

    #[test]
    fn un_renderer_sans_lpcm_prime_aussi_sur_le_greffon() {
        let sortie = crossfeed_status(true, false, true, false, true, false);
        assert_eq!(
            sortie.reason,
            Some(CrossfeedConstraint::NetworkRendererNoLpcm)
        );
        let s = avec_les_droits(sortie, true, false);
        assert_eq!(s.reason, Some(CrossfeedConstraint::NetworkRendererNoLpcm));
    }

    /// Contre-épreuve : sur une sortie qui PEUT porter le crossfeed, les droits
    /// restent nommés — sinon on aurait simplement tu la licence.
    #[test]
    fn sur_une_sortie_locale_les_droits_restent_nommes() {
        let locale = crossfeed_status(true, true, false, false, false, false);
        let s = avec_les_droits(locale.clone(), false, true);
        assert_eq!(s.reason, Some(CrossfeedConstraint::PremiumRequired));
        assert!(!s.effective && s.unavailable && s.requested);
        assert_eq!(
            s.detail,
            Some(CrossfeedConstraint::PremiumRequired.detail())
        );
        let s = avec_les_droits(locale.clone(), true, false);
        assert_eq!(s.reason, Some(CrossfeedConstraint::PluginUnavailable));
        assert_eq!(avec_les_droits(locale.clone(), true, true), locale);
    }

    /// Les droits passent devant ce qui se lève d'un geste sur la zone : PURE
    /// désactivé, un utilisateur Free n'entendrait toujours rien.
    #[test]
    fn les_droits_passent_devant_pure_et_l_opt_in_reseau() {
        let pure = crossfeed_status(true, true, false, true, false, false);
        assert_eq!(pure.reason, Some(CrossfeedConstraint::PureMode));
        assert_eq!(
            avec_les_droits(pure, false, true).reason,
            Some(CrossfeedConstraint::PremiumRequired)
        );
        let opt_in = crossfeed_status(true, false, true, false, false, false);
        assert_eq!(
            opt_in.reason,
            Some(CrossfeedConstraint::NetworkProgressiveOff)
        );
        assert_eq!(
            avec_les_droits(opt_in, true, false).reason,
            Some(CrossfeedConstraint::PluginUnavailable)
        );
    }

    /// Le branchement : la route calcule la sortie AVANT les droits et passe
    /// par `avec_les_droits`. Coupé au corps de la fonction pour qu'une
    /// mention ailleurs dans le fichier ne satisfasse pas la garde.
    #[test]
    fn la_route_passe_par_avec_les_droits() {
        const ROUTE: &str = include_str!("../../../tune-server/src/routes/zones/dsp.rs");
        let debut = ROUTE
            .find("async fn crossfeed_status_de_zone(")
            .expect("la route du statut crossfeed existe");
        let corps = &ROUTE[debut..];
        let corps = &corps[..corps.find("\n}\n").expect("fin de la fonction")];
        let sortie = corps
            .find("crossfeed::crossfeed_status(")
            .expect("la route calcule le statut de la sortie");
        let droits = corps
            .find("avec_les_droits(")
            .expect("la route applique les droits par avec_les_droits");
        assert!(
            sortie < droits,
            "la sortie doit être évaluée AVANT les droits"
        );
        assert!(
            !corps.contains("return CrossfeedStatus {"),
            "plus de retour anticipé sur la licence avant la sortie"
        );
    }
    // -----------------------------------------------------------------
    // #2742 — le réglage « crossfeed » ne doit plus MENTIR.
    //
    // Ces essais portent sur `crossfeed_status`, qui prend le type de sortie
    // et le mode PURE en PARAMÈTRES. C'est délibéré : les trois sites qui
    // installent un `CrossfeedProcessor` sont derrière
    // `#[cfg(feature = "local-audio")]`, et un essai entouré du même `cfg` ne
    // serait exécuté par aucune des cibles qui compilent sans — vert contre
    // rien, alors que c'est justement là que le réglage est le plus muet.
    // -----------------------------------------------------------------

    /// Le TÉMOIN de la règle : sortie locale, hors PURE, le réglage est honoré
    /// tel quel et **rien** ne vient s'ajouter à l'écran. Si ce test rougit,
    /// c'est qu'on a désarmé le cas nominal en corrigeant le cas réseau.
    #[test]
    fn une_sortie_locale_honore_le_crossfeed_sans_rien_annoncer() {
        let s = crossfeed_status(true, true, false, false, false, false);
        assert!(
            s.effective,
            "sortie locale hors PURE : le crossfeed s'applique"
        );
        assert!(
            !s.unavailable,
            "le contrôle doit rester ACTIF : c'est le cas nominal"
        );
        assert_eq!(s.reason, None);
        assert_eq!(s.detail, None);
        assert!(s.requested);

        // Décoché sur une sortie locale : rien à annoncer non plus.
        let eteint = crossfeed_status(false, true, false, false, false, false);
        assert!(!eteint.effective);
        assert!(!eteint.unavailable);
        assert_eq!(eteint.reason, None);
    }

    /// 1. Zone réseau + case COCHÉE + flux progressif DÉSARMÉ : le réglage est
    ///    sans effet, **et la raison est donnée**. C'est tout le ticket : avant,
    ///    le premier point était vrai et le second manquait.
    ///
    ///    Le motif a changé avec LAT-F1 : ce n'est plus « pas de sortie locale »
    ///    (une zone réseau PEUT désormais entendre le crossfeed) mais « le flux
    ///    progressif n'est pas armé ». La nuance compte pour l'utilisateur :
    ///    l'ancien message le renvoyait à changer de zone, le nouveau à cocher
    ///    une case.
    #[test]
    fn une_sortie_reseau_sans_flux_progressif_dit_que_le_crossfeed_n_agit_pas() {
        let s = crossfeed_status(true, false, true, false, false, false);
        assert!(
            !s.effective,
            "sans le bras progressif, une zone réseau n'a aucun chemin"
        );
        assert!(
            s.unavailable,
            "et le contrôle doit être annoncé comme INDISPONIBLE, pas honoré"
        );
        assert_eq!(s.reason, Some(CrossfeedConstraint::NetworkProgressiveOff));
        let detail = s
            .detail
            .expect("une contrainte sans explication, c'est le défaut de #2742");
        assert!(
            detail.contains("progressif"),
            "l'explication doit dire à l'utilisateur ce qu'il PEUT faire \
             (armer le flux progressif), pas seulement ce qu'il subit : {detail}"
        );
        assert!(
            s.requested,
            "`requested` doit rester ce que l'utilisateur a demandé, sinon \
             l'écran ne peut pas dire que son choix est resté lettre morte"
        );
    }

    /// 2. Zone réseau + case DÉCOCHÉE : `unavailable` se lève quand même. La
    ///    question n'est pas « le réglage a-t-il été changé ? » mais « ce
    ///    réglage a-t-il encore un sens ici ? » — sinon le client ne verrouille
    ///    le contrôle qu'APRÈS que l'utilisateur a cliqué pour rien.
    #[test]
    fn une_sortie_reseau_verrouille_meme_case_decochee() {
        let s = crossfeed_status(false, false, true, false, false, false);
        assert!(!s.effective);
        assert!(s.unavailable);
        assert_eq!(s.reason, Some(CrossfeedConstraint::NetworkProgressiveOff));
    }

    /// 3. Le mode PURE désarme le crossfeed sur une sortie locale, et le dit.
    ///    `load_crossfeed_processor` rend déjà `None` en PURE ; ce qui manquait
    ///    était de l'annoncer.
    #[test]
    fn le_mode_pure_desarme_le_crossfeed_et_le_dit() {
        let s = crossfeed_status(true, true, false, true, false, false);
        assert!(!s.effective);
        assert!(s.unavailable);
        assert_eq!(s.reason, Some(CrossfeedConstraint::PureMode));
        assert!(
            s.detail.is_some_and(|d| d.contains("PURE")),
            "l'explication doit nommer le mode qui désarme"
        );
    }

    /// 4. Zone réseau ET en PURE : c'est désormais **PURE** qui prime, et
    ///    l'inversion est le sujet.
    ///
    ///    Avant LAT-F1, `NonLocalOutput` passait devant parce qu'une zone
    ///    réseau ne verrait JAMAIS de crossfeed : nommer PURE aurait laissé
    ///    croire qu'il suffisait de le désactiver. Ce n'est plus vrai — le bras
    ///    progressif porte le crossfeed. La contrainte qu'aucun chemin
    ///    n'esquive est maintenant PURE : `load_crossfeed_processor` rend
    ///    `None` quelle que soit la sortie. C'est donc elle qu'il faut nommer.
    #[test]
    fn le_mode_pure_prime_sur_les_contraintes_de_chemin() {
        let s = crossfeed_status(true, false, true, true, true, true);
        assert_eq!(s.reason, Some(CrossfeedConstraint::PureMode));
    }

    /// LAT-F1 — une zone RÉSEAU dont tout est réuni entend enfin son crossfeed.
    ///
    /// C'est la ligne qui n'existait pas : jusqu'ici aucune combinaison de
    /// paramètres ne pouvait rendre `unavailable == false` sur une sortie non
    /// locale. Si ce test rougit, le crossfeed réseau est reparti au placard.
    #[test]
    fn une_zone_reseau_armee_entend_enfin_son_crossfeed() {
        let s = crossfeed_status(true, false, true, false, true, true);
        assert!(
            s.effective,
            "opt-in armé + renderer LPCM : le chemin existe"
        );
        assert!(!s.unavailable);
        assert!(s.reason.is_none(), "rien à annoncer : {s:?}");
        assert!(s.detail.is_none());
    }

    /// …et le renderer qui n'annonce pas le LPCM est nommé pour lui-même.
    ///
    /// Deux motifs distincts et non deux façons de dire « non » : l'opt-in est
    /// une case que l'utilisateur coche, le LPCM du renderer ne se négocie pas.
    /// Les confondre renverrait quelqu'un cocher une case déjà cochée.
    #[test]
    fn un_renderer_sans_lpcm_est_nomme_pour_lui_meme() {
        let s = crossfeed_status(true, false, true, false, true, false);
        assert!(s.unavailable);
        assert_eq!(s.reason, Some(CrossfeedConstraint::NetworkRendererNoLpcm));
        assert!(
            s.detail.is_some_and(|d| d.contains("PCM")),
            "l'explication doit nommer ce que le lecteur refuse"
        );
    }

    /// Ni locale, ni réseau : la prudence de #2742 tient.
    ///
    /// OAAT, navigateur, sorties PULL, zone sans périphérique résolu. Certaines
    /// traversent probablement le relais — mais ce n'est pas mesuré, et
    /// annoncer « disponible » sans preuve est le défaut de #2742 pris à
    /// l'envers. Ce test grave le choix, il ne le célèbre pas.
    #[test]
    fn une_sortie_ni_locale_ni_reseau_reste_annoncee_indisponible() {
        let s = crossfeed_status(true, false, false, false, true, true);
        assert!(s.unavailable);
        assert_eq!(s.reason, Some(CrossfeedConstraint::NonLocalOutput));
    }

    /// Le prédicat publié doit être EXACTEMENT celui des trois sites
    /// d'installation : `starts_with("local:")`, et rien d'autre. Un `dlna:` ou
    /// un nom nu ne doit jamais passer pour une sortie locale — c'est
    /// précisément la confusion que `create_zone` documente.
    #[test]
    fn seul_le_prefixe_local_fait_courir_le_crossfeed() {
        assert!(crossfeed_runs_on_output(Some("local:Realtek")));
        assert!(crossfeed_runs_on_output(Some("local:")));
        assert!(!crossfeed_runs_on_output(Some("dlna:uuid-1234")));
        assert!(!crossfeed_runs_on_output(Some("chromecast:salon")));
        assert!(!crossfeed_runs_on_output(Some("Realtek")));
        assert!(!crossfeed_runs_on_output(Some("browser:1")));
        assert!(
            !crossfeed_runs_on_output(None),
            "une zone sans périphérique ne joue nulle part"
        );
    }

    /// Contre-épreuve permanente : toute contrainte doit porter un code STABLE,
    /// unique, et une explication non vide. Une variante ajoutée sans être
    /// décrite fait tomber ce test.
    #[test]
    fn chaque_contrainte_a_un_code_unique_et_une_explication() {
        let mut codes: Vec<&str> = Vec::new();
        for c in CrossfeedConstraint::ALL {
            assert!(!c.code().is_empty(), "code vide");
            assert!(
                !c.detail().trim().is_empty(),
                "contrainte sans explication : {}",
                c.code()
            );
            assert!(!codes.contains(&c.code()), "code dupliqué : {}", c.code());
            codes.push(c.code());
        }
        assert_eq!(codes.len(), CrossfeedConstraint::ALL.len());
    }

    /// Le code stable doit être celui que porte le JSON — pas une chaîne
    /// recopiée à côté. C'est ce que le client lira pour choisir sa traduction.
    #[test]
    fn le_code_serialise_est_le_code_stable() {
        // Ni locale, ni réseau : le seul montage qui rend encore
        // `NonLocalOutput` depuis LAT-F1.
        let s = crossfeed_status(true, false, false, false, false, false);
        let v = serde_json::to_value(&s).expect("le statut doit être sérialisable");
        assert_eq!(
            v["reason"].as_str(),
            Some(CrossfeedConstraint::NonLocalOutput.code()),
            "le client lit ce code, il ne doit pas dériver du nom Rust"
        );
        assert_eq!(v["unavailable"].as_bool(), Some(true));
        assert_eq!(v["requested"].as_bool(), Some(true));
        assert_eq!(v["effective"].as_bool(), Some(false));
        assert!(v["detail"].as_str().is_some_and(|d| !d.is_empty()));

        // Le cas nominal ne publie AUCUN motif : `null`, pas une chaîne vide.
        let nominal =
            serde_json::to_value(crossfeed_status(true, true, false, false, false, false)).unwrap();
        assert!(nominal["reason"].is_null());
        assert!(nominal["detail"].is_null());

        // Et le lien code() ↔ JSON vaut pour TOUTES les variantes, pas pour la
        // seule qu'un test aurait choisie. Un `#[serde(rename)]` oublié sur une
        // variante ajoutée fait tomber cette boucle.
        for c in CrossfeedConstraint::ALL {
            let json = serde_json::to_value(c).expect("une contrainte doit être sérialisable");
            assert_eq!(
                json.as_str(),
                Some(c.code()),
                "le JSON d'une contrainte doit être son code stable"
            );
        }
    }

    /// ⭐ GARDE DE SITE — la prémisse de toute la règle, relue dans le code de
    /// PRODUCTION.
    ///
    /// ⚠️ **La prémisse a changé avec LAT-F1, et ce test a failli devenir un
    /// garde-fou aveugle.** Il ne cherche que `.set_crossfeed(` et
    /// `.replace_crossfeed_live(` : le quatrième site d'installation, celui du
    /// relais progressif, n'utilise NI l'un NI l'autre — il pose un
    /// `CrossfeedProcessor` dans `StreamingDsp`. Le test serait donc resté vert
    /// en gardant une affirmation devenue fausse. C'est le mode de panne qu'il
    /// existe pour empêcher, retourné contre lui-même.
    ///
    /// La règle exacte aujourd'hui : hors sortie locale, le crossfeed passe par
    /// le relais progressif **et par lui seul**. Trois choses à tenir, donc :
    ///
    /// 1. les trois sites `set_crossfeed` / `replace_crossfeed_live` restent
    ///    derrière la garde `local:` — sinon la sortie locale traiterait deux
    ///    fois, comme l'égaliseur l'a fait en 0.9.139 ;
    /// 2. le chemin FICHIER (`transcode_source_to_file`) n'en porte toujours
    ///    aucun — le renderer y reçoit un morceau entier pré-transcodé, et
    ///    `crossfeed_status` s'appuie dessus pour dire `network_progressive_off` ;
    /// 3. le relais progressif, LUI, en porte un — sinon `crossfeed_status`
    ///    annoncerait « disponible » sur une zone réseau où plus rien ne
    ///    l'applique : le mensonge de #2742, à l'envers.
    ///
    /// On relit le code de production par `include_str!`, l'idiome du dépôt.
    #[test]
    fn aucun_site_d_installation_du_crossfeed_hors_de_la_garde_locale() {
        // Le bloc `impl PlaybackOrchestrator` est réparti par familles (REF-2,
        // #2219) : on lit chaque fichier qui porte un site, mis bout à bout.
        const ORCHESTRATEUR: &str = concat!(
            include_str!("../orchestrator.rs"),
            include_str!("../orchestrator/dsp.rs"),
            include_str!("../orchestrator/transport.rs"),
            include_str!("../orchestrator/transcodage.rs"),
        );
        // Témoin : si `include_str!` pointait sur un fichier vide ou faux, tout
        // le reste passerait pour vert sans rien avoir lu.
        assert!(
            ORCHESTRATEUR.contains("fn load_crossfeed_processor"),
            "include_str! ne lit pas l'orchestrateur attendu"
        );
        let lignes: Vec<&str> = ORCHESTRATEUR.lines().collect();

        // Début d'une fonction — la BORNE de la recherche en amont. Une garde
        // posée dans une AUTRE fonction ne garde rien ; sans cette borne, une
        // simple fenêtre de N lignes attraperait le `local:` du voisin et
        // resterait verte contre une garde supprimée.
        fn debut_de_fonction(l: &str) -> bool {
            let t = l.trim_start();
            [
                "fn ",
                "pub fn ",
                "async fn ",
                "pub async fn ",
                "pub(crate) fn ",
                "pub(crate) async fn ",
                "pub(super) fn ",
                "pub(super) async fn ",
            ]
            .iter()
            .any(|p| t.starts_with(p))
        }

        // Remonter depuis `depuis` (exclu) jusqu'à `motif`, sans franchir le
        // début de la fonction courante. Rend la ligne trouvée, ou `None`.
        fn remonter(lignes: &[&str], depuis: usize, motif: &str) -> Option<usize> {
            for i in (0..depuis).rev() {
                if lignes[i].contains(motif) {
                    return Some(i);
                }
                if debut_de_fonction(lignes[i]) {
                    return None;
                }
            }
            None
        }

        let mut sites = 0usize;
        for (i, ligne) in lignes.iter().enumerate() {
            // Les APPELS, pas les définitions ni les commentaires.
            let nu = ligne.trim_start();
            let appel = (ligne.contains(".set_crossfeed(")
                || ligne.contains(".replace_crossfeed_live("))
                && !nu.starts_with("//");
            if !appel {
                continue;
            }
            sites += 1;
            let downcast = remonter(
                &lignes,
                i,
                "downcast_ref::<crate::outputs::local::LocalOutput>()",
            )
            .unwrap_or_else(|| {
                panic!(
                    "site d'installation du crossfeed sans `LocalOutput` dans sa \
                     propre fonction (orchestrator.rs + dsp.rs + transport.rs, ligne {}) : {}",
                    i + 1,
                    ligne.trim()
                )
            });
            assert!(
                remonter(&lignes, downcast, "starts_with(\"local:\")").is_some(),
                "site d'installation du crossfeed sans garde `local:` dans sa \
                 propre fonction (orchestrator.rs + dsp.rs + transport.rs, ligne {}) : {}\n\
                 Si le crossfeed atteint désormais une sortie NON locale, \
                 `crossfeed_status` ment et doit être corrigé AVEC ce site.",
                i + 1,
                ligne.trim()
            );
        }
        // Le compte est un plancher VÉRIFIÉ, pas une supposition : au 03/09/2026
        // il y a exactement trois sites — le chemin de lecture,
        // `refresh_zone_crossfeed` et `refresh_zone_pure_dsp`. Un quatrième est
        // le bienvenu ; zéro signifierait que ce test ne mesure plus rien.
        assert!(
            sites >= 3,
            "seulement {sites} site(s) d'installation trouvé(s) : le test ne \
             garde plus le chemin qu'il prétend garder"
        );

        // La prémisse INVERSE, et c'est la plus importante : le chemin réseau
        // ne doit pas se mettre à appliquer un crossfeed dans notre dos.
        // `transcode_source_to_file` est la seule porte du chemin transcodé
        // (DLNA, OpenHome, Chromecast, BluOS…) ; sa signature ne porte que
        // l'égaliseur, le convolveur et le ReplayGain. Le jour où l'on y ajoute
        // le crossfeed, `crossfeed_status` mentira dans l'AUTRE sens — un écran
        // qui annonce « sans effet » pendant que le DAC reçoit un signal
        // traité — et ce test doit l'exiger AVANT que ça n'arrive.
        let (_, apres) = ORCHESTRATEUR
            .split_once("async fn transcode_source_to_file(")
            .expect("la porte du chemin transcodé doit exister");
        let (signature, _) = apres
            .split_once(") -> ")
            .expect("signature de transcode_source_to_file illisible");
        assert!(
            !signature.contains("crossfeed"),
            "le chemin transcodé accepte désormais un crossfeed : \
             `CrossfeedConstraint::NonLocalOutput` n'est plus vrai et doit \
             être revu ici AVANT d'être publié à l'écran.\nsignature : {signature}"
        );

        // 3. Et le RELAIS progressif, lui, doit en porter un.
        //
        // C'est la moitié que les deux blocs précédents ne voient pas : ils
        // gardent les chemins qui NE DOIVENT PAS appliquer de crossfeed. Sans
        // celle-ci, quelqu'un qui retire le quatrième étage laisse
        // `crossfeed_status` annoncer « disponible » sur une zone réseau où
        // plus rien ne l'applique — exactement le mensonge de #2742, à
        // l'envers, et les trois assertions d'au-dessus resteraient vertes.
        assert!(
            ORCHESTRATEUR
                .contains("crossfeed: Option<crate::audio::crossfeed::CrossfeedProcessor>"),
            "`StreamingDsp` ne porte plus d'étage crossfeed : une zone réseau \
             n'a plus aucun chemin, et `crossfeed_status` doit redevenir \
             `NonLocalOutput` AVANT que cet étage disparaisse"
        );
        assert!(
            ORCHESTRATEUR.contains("crossfeed: self.load_crossfeed_processor("),
            "`load_streaming_dsp` ne charge plus le crossfeed : l'étage existe \
             mais reste vide, donc muet — un `None` permanent est plus \
             sournois qu'un champ supprimé, car il compile et se teste vert"
        );
        assert!(
            ORCHESTRATEUR.contains("cf.process_pcm(pcm, bit_depth, self.channels)"),
            "`StreamingDsp::process` n'applique plus le crossfeed : il est \
             chargé, transporté, et jeté sans être exécuté"
        );
    }
}
