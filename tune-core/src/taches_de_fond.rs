//! Registre des traitements de fond, et leur PAUSE.
//!
//! Les passes de fond de Tune durent des heures : sur le .18 de Bertrand,
//! 57 % de 47 118 pistes pour la plage dynamique, 7 % de 2 483 pour le
//! ReplayGain. Elles décodent des fichiers entiers, écrivent le disque et
//! tiennent le CPU pendant qu'il écoute. Le scan avait un `cancel` ; **aucune
//! autre passe n'avait le moindre geste** — ni pause, ni arrêt.
//!
//! Ce module est le mécanisme UNIQUE. Pas cinq bricolages : un registre, des
//! traitements nommés, trois états.
//!
//! ## Ce qui existait déjà, et pourquoi ça ne suffisait pas
//!
//! Le cœur sait déjà s'effacer tout seul : devant la lecture
//! ([`crate::audio::replaygain::any_zone_playing`], #1310/#1515), devant un
//! scan ([`crate::scanner::activite`], #2469), devant la chaleur
//! ([`crate::audio::thermal`], #1576), et les passes lourdes se succèdent au
//! lieu de s'additionner ([`crate::audio::replaygain::ANALYSIS_SLOT`]).
//! Toutes ces gardes sont AUTOMATIQUES et transitoires : elles répondent à un
//! état de la machine, jamais à une décision de l'utilisateur, et aucune ne
//! survit au redémarrage. « Je veux que ça s'arrête pendant que j'écoute ce
//! soir » n'avait aucun porteur.
//!
//! ## Pause COOPÉRATIVE
//!
//! Rien n'est tué. La pause est un drapeau que les boucles relisent à une
//! **frontière propre** — entre deux pistes, entre deux artistes, jamais au
//! milieu d'un décodage ni d'une écriture. Une piste commencée est finie et
//! écrite ; c'est seulement la SUIVANTE qui n'est pas prise. Rien ne reste à
//! moitié écrit, rien n'est perdu : les passes reprennent leurs candidats par
//! requête, la reprise repart exactement là où la pause a laissé le travail.
//!
//! Deux primitives, et le choix entre les deux n'est pas cosmétique :
//!
//! * [`est_en_pause`] — pour une passe **reprenable par requête** (la cascade
//!   ReplayGain → empreintes → plage dynamique). Elle sort de son lot et rend
//!   ce qu'elle a fait ; le prochain tour retrouvera les mêmes candidats.
//! * [`attendre_la_reprise`] — pour une passe **qui tient sa liste en
//!   mémoire** (enrichissement des métadonnées, images d'artistes). La tuer
//!   perdrait le curseur, alors on la GARE : la tâche reste vivante, endormie
//!   à la frontière, et repart au même index.
//!
//! ## Pourquoi l'état est PERSISTANT
//!
//! Une pause qui ne survit pas au redémarrage ne sert à rien sur une passe de
//! huit heures : la mise à jour du soir, un `Restart=always` après un OOM, et
//! le décodage repart tout seul au milieu de l'écoute. L'état vit donc dans la
//! table `settings`, sous [`Tache::cle_reglage`], et [`hydrater`] le relit au
//! démarrage — un traitement en pause ne repart pas de lui-même.
//!
//! C'est l'arbitrage INVERSE de [`crate::audio::replaygain::progression`], et
//! à dessein : l'avancement est une propriété du processus (un serveur tué en
//! plein balayage ne doit pas laisser une campagne « en cours » que personne
//! ne fermera), la pause est une DÉCISION de l'utilisateur, qui doit au
//! contraire lui survivre.
//!
//! ## Pourquoi un miroir en mémoire
//!
//! Les boucles relisent le drapeau à chaque piste. Une lecture SQL par piste,
//! sur la base qui sert en même temps la lecture, est exactement la dépense
//! que `progression` refuse. Le miroir est un masque de bits atomique, écrit
//! en même temps que la base et relu gratuitement ; la base reste la source de
//! vérité au démarrage.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use crate::db::backend::DbBackend;
use crate::db::settings_repo::SettingsRepo;

/// Un traitement de fond que l'utilisateur peut suspendre.
///
/// ⚠️ **Le scan n'en est pas**, et ce n'est pas un oubli : voir
/// [`pourquoi_le_scan_n_est_pas_suspendable`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Tache {
    /// La passe ReplayGain — rang 1 de la cascade de fond. Décode chaque
    /// piste pour mesurer la sonie EBU R128.
    ReplayGain,
    /// Les empreintes audio — rang 2 de la cascade (BIB-B2).
    Empreintes,
    /// La plage dynamique — rang 3 de la cascade, et la passe à la demande de
    /// `POST /system/dynamic-range/analyze` (#4185).
    PlageDynamique,
    /// L'analyse acoustique (embeddings CLAP).
    Acoustique,
    /// L'enrichissement des métadonnées (MusicBrainz).
    Enrichissement,
    /// Les images d'artistes et les pochettes.
    ImagesArtistes,
    /// L'identification de la bibliothèque : le pilote de lot de
    /// `POST /library/identify-all` (#4805). Il enchaîne, album par album, la
    /// chaîne déjà écrite de `POST /library/albums/{id}/reidentify`.
    ///
    /// Elle est ici pour la MÊME raison que les six autres : 2 h 23 de
    /// requêtes sortantes mesurées sur les 3 909 albums locaux du .18 ne
    /// peuvent pas être une passe qu'on ne peut plus arrêter.
    Identification,
}

impl Tache {
    /// Toutes les tâches, dans l'ordre d'affichage de l'écran « État du
    /// serveur ». C'est la liste que balaient l'interrupteur général et le
    /// relevé : ajouter une tâche ici suffit à l'y faire entrer.
    pub const TOUTES: [Tache; 7] = [
        Tache::ReplayGain,
        Tache::Empreintes,
        Tache::PlageDynamique,
        Tache::Acoustique,
        Tache::Enrichissement,
        Tache::ImagesArtistes,
        Tache::Identification,
    ];

    /// Identifiant STABLE — c'est le mot que lisent l'API et le client web.
    ///
    /// Les quatre premiers reprennent mot pour mot les identifiants déjà
    /// servis ailleurs (`dynamic_range` est la clé du registre
    /// `background_tasks` de `routes/system/dynamic_range.rs`, `replaygain`
    /// est le préfixe de `/system/replaygain/progress`) : un second
    /// vocabulaire pour les mêmes passes obligerait le client à tenir une
    /// table de correspondance.
    pub fn id(self) -> &'static str {
        match self {
            Tache::ReplayGain => "replaygain",
            Tache::Empreintes => "fingerprints",
            Tache::PlageDynamique => "dynamic_range",
            Tache::Acoustique => "acoustic",
            Tache::Enrichissement => "enrichment",
            Tache::ImagesArtistes => "artist_images",
            Tache::Identification => "identification",
        }
    }

    /// L'inverse de [`Self::id`], pour la route `/{id}/pause`. `None` sur un
    /// identifiant inconnu — la route rend alors 404 plutôt que de suspendre
    /// quelque chose au hasard.
    pub fn depuis_id(id: &str) -> Option<Self> {
        Tache::TOUTES.into_iter().find(|t| t.id() == id)
    }

    /// La clé de la table `settings` qui porte la pause.
    ///
    /// Préfixe commun : un `SELECT ... LIKE 'tache_de_fond_pause_%'` suffit à
    /// tout relire, et une clé d'un futur traitement retiré ne se confond avec
    /// aucun autre réglage.
    pub fn cle_reglage(self) -> &'static str {
        match self {
            Tache::ReplayGain => "tache_de_fond_pause_replaygain",
            Tache::Empreintes => "tache_de_fond_pause_fingerprints",
            Tache::PlageDynamique => "tache_de_fond_pause_dynamic_range",
            Tache::Acoustique => "tache_de_fond_pause_acoustic",
            Tache::Enrichissement => "tache_de_fond_pause_enrichment",
            Tache::ImagesArtistes => "tache_de_fond_pause_artist_images",
            Tache::Identification => "tache_de_fond_pause_identification",
        }
    }

    /// Le bit du miroir en mémoire. Dérivé de la position dans
    /// [`Self::TOUTES`], donc impossible à faire diverger de la liste.
    fn bit(self) -> u32 {
        let position = Tache::TOUTES
            .iter()
            .position(|t| *t == self)
            .expect("toute tâche figure dans TOUTES");
        1u32 << position
    }
}

/// L'état d'un traitement, tel que l'écran le montre.
///
/// Trois valeurs, pas deux : « en pause » et « au repos » ne s'affichent pas
/// pareil et n'appellent pas le même bouton. Une carte au repos n'a rien à
/// suspendre ; une carte en pause doit porter « Reprendre ».
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Etat {
    /// Le traitement a du travail devant lui et le fait.
    EnCours,
    /// L'utilisateur l'a suspendu. Il ne reprendra pas tout seul, pas même
    /// après un redémarrage.
    EnPause,
    /// Rien à faire, ou passe désarmée.
    AuRepos,
}

impl Etat {
    /// Le mot que lit le client web.
    pub fn code(self) -> &'static str {
        match self {
            Etat::EnCours => "en_cours",
            Etat::EnPause => "en_pause",
            Etat::AuRepos => "au_repos",
        }
    }
}

/// Miroir en mémoire des pauses : un bit par tâche.
///
/// Part à zéro et non « tout en pause » : un serveur qui n'a pas encore
/// hydraté ne doit pas geler ses passes par accident. [`hydrater`] est appelé
/// au démarrage, AVANT que les passes ne sortent de leur sieste de 120 s.
static PAUSES: AtomicU32 = AtomicU32::new(0);

/// Cadence à laquelle une passe garée relit le drapeau.
///
/// Deux secondes : assez court pour qu'un clic sur « Reprendre » se voie
/// aussitôt, assez long pour qu'une passe garée toute la soirée ne coûte rien
/// — c'est une lecture atomique, pas une requête.
pub const CADENCE_RELECTURE_PAUSE: std::time::Duration = std::time::Duration::from_secs(2);

/// Ce traitement est-il suspendu ?
///
/// Lecture atomique, gratuite : c'est ce qui permet de la poser au plus près
/// de la frontière, à chaque piste, plutôt qu'une fois par lot de 25.
pub fn est_en_pause(tache: Tache) -> bool {
    PAUSES.load(Ordering::Relaxed) & tache.bit() != 0
}

/// Les traitements suspendus, dans l'ordre de [`Tache::TOUTES`].
pub fn suspendus() -> Vec<Tache> {
    Tache::TOUTES
        .into_iter()
        .filter(|t| est_en_pause(*t))
        .collect()
}

/// Tous les traitements sont-ils suspendus ? C'est l'état de l'interrupteur
/// général de l'écran.
pub fn tout_est_suspendu() -> bool {
    Tache::TOUTES.into_iter().all(est_en_pause)
}

/// Relire les pauses depuis la base, au démarrage du serveur.
///
/// Sans cet appel, une pause posée hier serait oubliée au prochain
/// redémarrage et la passe repartirait toute seule — précisément ce que la
/// persistance existe pour empêcher.
///
/// Écrit le miroir en UNE fois : un `store` par tâche laisserait une fenêtre
/// où la moitié des pauses seraient levées.
pub fn hydrater(backend: &Arc<dyn DbBackend>) {
    let reglages = SettingsRepo::with_backend(backend.clone());
    let mut masque = 0u32;
    for tache in Tache::TOUTES {
        if lire_en_base(&reglages, tache) {
            masque |= tache.bit();
        }
    }
    PAUSES.store(masque, Ordering::Relaxed);
    if masque != 0 {
        tracing::info!(
            suspendus = ?suspendus().iter().map(|t| t.id()).collect::<Vec<_>>(),
            "taches_de_fond_pauses_restaurees"
        );
    }
}

/// `true` seulement sur la chaîne `"true"`. Une clé absente, vide ou illisible
/// vaut « pas en pause » : dans le doute, la passe TRAVAILLE — l'inverse
/// gèlerait une bibliothèque pour une valeur mal écrite, sans rien à l'écran
/// pour l'expliquer.
fn lire_en_base(reglages: &SettingsRepo, tache: Tache) -> bool {
    reglages
        .get(tache.cle_reglage())
        .ok()
        .flatten()
        .is_some_and(|v| v == "true")
}

/// Suspendre un traitement. Idempotent.
pub fn mettre_en_pause(backend: &Arc<dyn DbBackend>, tache: Tache) -> Result<(), String> {
    ecrire(backend, tache, true)
}

/// Reprendre un traitement. Idempotent.
pub fn reprendre(backend: &Arc<dyn DbBackend>, tache: Tache) -> Result<(), String> {
    ecrire(backend, tache, false)
}

/// L'interrupteur général : tout suspendre.
///
/// Boucle sur [`Tache::TOUTES`] plutôt que d'écrire un drapeau « global » à
/// part : un second drapeau se désynchroniserait des six premiers dès qu'on
/// reprend UNE tâche, et l'écran afficherait « tout suspendu » sur une passe
/// qui décode.
pub fn tout_suspendre(backend: &Arc<dyn DbBackend>) -> Result<(), String> {
    for tache in Tache::TOUTES {
        mettre_en_pause(backend, tache)?;
    }
    Ok(())
}

/// L'interrupteur général : tout reprendre.
pub fn tout_reprendre(backend: &Arc<dyn DbBackend>) -> Result<(), String> {
    for tache in Tache::TOUTES {
        reprendre(backend, tache)?;
    }
    Ok(())
}

/// La base D'ABORD, le miroir ensuite.
///
/// Cet ordre est le seul sûr : si l'écriture échoue, le miroir n'a pas bougé
/// et l'interface rendra l'erreur. L'inverse laisserait une pause visible en
/// mémoire que le redémarrage lèverait sans prévenir.
fn ecrire(backend: &Arc<dyn DbBackend>, tache: Tache, en_pause: bool) -> Result<(), String> {
    let reglages = SettingsRepo::with_backend(backend.clone());
    reglages.set(tache.cle_reglage(), if en_pause { "true" } else { "false" })?;
    if en_pause {
        PAUSES.fetch_or(tache.bit(), Ordering::Relaxed);
    } else {
        PAUSES.fetch_and(!tache.bit(), Ordering::Relaxed);
    }
    tracing::info!(tache = tache.id(), en_pause, "tache_de_fond_pause_modifiee");
    Ok(())
}

/// Garer la passe tant qu'elle est suspendue, puis la laisser repartir.
///
/// Pour les passes **qui tiennent leur liste en mémoire** : l'enrichissement
/// des métadonnées parcourt un `Vec` de candidats calculé à l'ouverture, la
/// passe d'images d'artistes fait de même. Sortir de la boucle perdrait le
/// curseur et la reprise refarait tout depuis le début — ou pire, ne referait
/// rien du tout parce que la tâche serait finie. On l'endort donc à la
/// frontière, et elle repart au même index.
///
/// Ne dort PAS quand rien n'est suspendu : c'est le cas courant, et il doit
/// être gratuit.
pub async fn attendre_la_reprise(tache: Tache) {
    if !est_en_pause(tache) {
        return;
    }
    tracing::info!(tache = tache.id(), "tache_de_fond_garee");
    while est_en_pause(tache) {
        tokio::time::sleep(CADENCE_RELECTURE_PAUSE).await;
    }
    tracing::info!(tache = tache.id(), "tache_de_fond_repartie");
}

/// Vider le miroir en mémoire, SANS toucher à la base.
///
/// C'est exactement l'état d'un serveur qui vient de démarrer : la base porte
/// les pauses, le processus ne les connaît pas encore. Les témoins s'en servent
/// pour jouer un redémarrage sans relancer un serveur — appeler cette fonction
/// puis [`hydrater`] reproduit à l'identique ce que fait
/// `spawn_background_tasks`.
///
/// `pub` et non `#[cfg(test)]` : `cfg(test)` ne traverse pas les frontières de
/// caisse, et les témoins de `tune-core/tests/` comme ceux de `tune-server`
/// sont des caisses EXTERNES.
pub fn oublier_pour_les_essais() {
    PAUSES.store(0, Ordering::Relaxed);
}

/// Pourquoi le scan de bibliothèque n'est pas dans [`Tache`].
///
/// Ce n'est pas un oubli, et la fonction existe pour que la raison soit
/// lisible depuis le code plutôt que depuis un fil de discussion.
///
/// 1. **Il a déjà son geste** : `POST /system/scan/cancel`, et le relancer
///    reprend là où il en était — le pré-filtre `file_needs_scan` écarte tout
///    ce qui n'a pas bougé. « Arrêter puis relancer » EST la pause du scan,
///    sans nouvel état à tenir.
/// 2. **Sa porte est un état de processus, pas un réglage** : `ScanGate` tient
///    une génération et un jeton non clonable (`ScanLease`). Un scan garé
///    garderait son jeton — donc la porte fermée — et sa `MarqueDeScan`, qui
///    efface le balayage acoustique tant qu'elle vit (#2469). Suspendre le
///    scan une soirée gèlerait l'analyse acoustique par ricochet, exactement
///    l'inverse de ce qu'on demande.
/// 3. **Une pause persistante n'aurait rien à reprendre** : l'état du parcours
///    (la pile de répertoires, la liste des fichiers à reprendre) vit en
///    mémoire et meurt au redémarrage. Une pause qui survit au redémarrage
///    rouvrirait un scan qui n'existe plus.
///
/// Le scan reste donc sur `cancel`, et c'est ce que dit l'écran.
pub const fn pourquoi_le_scan_n_est_pas_suspendable() -> &'static str {
    "le scan a POST /system/scan/cancel, et sa reprise est un simple relancement : \
     son avancement est un état de processus, pas un curseur en base"
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Les bits ne doivent pas se marcher dessus : deux tâches au même bit
    /// feraient qu'en suspendre une en suspendrait deux.
    #[test]
    fn chaque_tache_a_son_bit() {
        let mut vus = 0u32;
        for tache in Tache::TOUTES {
            let bit = tache.bit();
            assert_eq!(vus & bit, 0, "bit déjà pris par une autre tâche: {tache:?}");
            vus |= bit;
        }
        assert_eq!(vus.count_ones() as usize, Tache::TOUTES.len());
    }

    /// L'aller-retour identifiant ↔ tâche. Une faute de frappe dans `id()`
    /// rendrait une route `/{id}/pause` muette.
    #[test]
    fn les_identifiants_font_l_aller_retour() {
        for tache in Tache::TOUTES {
            assert_eq!(Tache::depuis_id(tache.id()), Some(tache));
        }
        assert_eq!(Tache::depuis_id("scan"), None);
        assert_eq!(Tache::depuis_id(""), None);
    }

    /// Les clés de réglage sont distinctes : une clé partagée ferait qu'une
    /// pause en suspendrait deux, et la persistance mentirait.
    #[test]
    fn les_cles_de_reglage_sont_distinctes() {
        let mut cles: Vec<&str> = Tache::TOUTES.iter().map(|t| t.cle_reglage()).collect();
        cles.sort_unstable();
        let avant = cles.len();
        cles.dedup();
        assert_eq!(
            cles.len(),
            avant,
            "deux tâches partagent une clé de réglage"
        );
    }
}
