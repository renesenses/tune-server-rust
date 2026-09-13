//! #3575 — le relevé des teneurs du PCM est-il BRANCHÉ sur le chemin qui
//! échoue ?
//!
//! `crate::audio::pcm_teneur` est éprouvé de son côté par ses propres témoins,
//! sur une arborescence `/proc` fabriquée. Ces témoins resteraient verts
//! pendant que `outputs/local.rs` n'appelle rien : c'est « écrit mais pas
//! branché », la faute que ce dépôt a payée assez souvent pour lui avoir donné
//! un nom.
//!
//! # Pourquoi une garde de SITE par `include_str!`
//!
//! `tune-core/src/outputs/local.rs` vit derrière la feature `local-audio`, que
//! le job `Test` de `ci.yml` n'active pas ; les jobs qui l'activent sont
//! conditionnés à `full` et ne sont jamais joués sur une PR vers `batch/*`. Un
//! test qui compilerait ce module serait vert contre rien (#2816). Lire le
//! *texte* du fichier échappe aux `cfg`.
//!
//! Et il n'y a pas d'autre moyen : Shrek n'a **pas de carte son**
//! (`/proc/asound` n'existe pas), donc aucune ouverture ALSA réelle — réussie
//! ou refusée — n'y est exécutable.
//!
//! ⚠️ `tune-server` porte `autotests = false` : ce fichier n'est compilé que
//! parce qu'il est déclaré dans l'agrégateur `server_contracts.rs`.

const LOCAL_RS: &str = include_str!("../../tune-core/src/outputs/local.rs");
// REF-8 (#2219) : le chemin d'échec du flux WAV (`audio_stream_build_failed_all_formats`)
// vit dans `BackendCpal::ouvrir` (`local/backend.rs`) ; la garde lit les deux
// fichiers concaténés, jamais l'un à la place de l'autre.
const BACKEND_RS: &str = include_str!("../../tune-core/src/outputs/local/backend.rs");

/// La production seule — le `mod tests` de fin citerait nos motifs.
fn production() -> String {
    let fin = LOCAL_RS
        .find("#[cfg(test)]\nmod tests")
        .expect("local.rs doit garder son `#[cfg(test)] mod tests` en fin de fichier");
    [&LOCAL_RS[..fin], BACKEND_RS].concat()
}

/// Texte sans commentaires ni blancs ; `://` épargné (URL, greffons ALSA).
fn sans_commentaires_ni_blancs(source: &str) -> String {
    let mut assemble = String::with_capacity(source.len());
    for ligne in source.lines() {
        let mut garde = ligne;
        let mut depart = 0usize;
        while let Some(relatif) = ligne[depart..].find("//") {
            let absolu = depart + relatif;
            if absolu > 0 && ligne.as_bytes()[absolu - 1] == b':' {
                depart = absolu + 2;
                continue;
            }
            garde = &ligne[..absolu];
            break;
        }
        assemble.push_str(garde);
        assemble.push('\n');
    }
    assemble.chars().filter(|c| !c.is_whitespace()).collect()
}

/// LES DEUX sites d'échec — et pas un seul.
///
/// `outputs/local.rs` a **deux** chemins qui arrêtent la zone après avoir
/// refusé tous les formats : le flux WAV/PCM
/// (`audio_stream_build_failed_all_formats`) et le flux compressé décodé
/// (`audio_stream_build_failed_compressed`). Le relevé de Belkadi Yacine porte
/// les deux. Instrumenter un seul des deux, c'est fabriquer un angle mort qui
/// se lira comme une absence de teneur.
#[test]
fn les_deux_chemins_d_echec_relevent_qui_tient_le_pcm() {
    let source = sans_commentaires_ni_blancs(&production());
    // Le motif s'arrête AVANT la parenthèse fermante : rustfmt écrit l'un des
    // deux appels sur une ligne (sans virgule finale) et l'autre en colonne
    // (avec), et une garde qui exigerait la même ponctuation des deux rougirait
    // au prochain passage de l'outil sans qu'aucune décision ait changé.
    let appel = [
        "ifcause==OpenFailure::IndisponibleMotifPerdu{",
        "journaliser_les_teneurs_du_pcm(&pcm_ouvert,&device_name",
    ]
    .concat();
    let sites = source.matches(&appel).count();
    assert_eq!(
        sites, 2,
        "#3575 — les deux chemins qui arrêtent la zone sur un refus d'ouverture \
         (`audio_stream_build_failed_all_formats`, chemin PCM, et \
         `audio_stream_build_failed_compressed`, chemin décodé) doivent relever \
         QUI tient le nœud `/dev/snd/pcmC…D…p`, avec l'endpoint réellement \
         OUVERT (`pcm_ouvert`) et non celui réglé sur la zone. J'en \
         compte {sites}. Sans ce relevé, la question posée au testeur depuis le \
         07/09 — `fuser -v /dev/snd/*` AVANT tout redémarrage — reste sans \
         réponse, et un PCM tenu par une instance précédente du processus reste \
         indiscernable d'un DAC débranché."
    );
}

/// Le relevé doit être conditionné au motif PERDU, et pas déclenché partout.
///
/// Sur `DeviceGone` ou `ServerUnreachable` le motif est déjà connu : parcourir
/// tout `/proc` à chaque refus coûterait sans rien apprendre, et une ligne
/// `teneurs=aucun_teneur_visible` posée sur un motif connu se lirait comme une
/// information alors qu'elle n'en est pas une.
#[test]
fn le_releve_ne_se_declenche_que_sur_le_motif_detruit_par_cpal() {
    let source = sans_commentaires_ni_blancs(&production());
    // Tous les appels, gardés ou non — puis les seuls gardés. L'égalité est ce
    // qui interdit d'en ajouter un troisième sans sa condition : compter
    // seulement les gardés laisserait passer un appel nu posé à côté.
    let appels = source
        .matches("journaliser_les_teneurs_du_pcm(&pcm_ouvert")
        .count();
    let gardes = source
        .matches(
            &[
                "ifcause==OpenFailure::IndisponibleMotifPerdu{",
                "journaliser_les_teneurs_du_pcm(&pcm_ouvert",
            ]
            .concat(),
        )
        .count();
    assert_eq!(
        appels, gardes,
        "#3575 — {appels} appel(s) au relevé, dont {gardes} gardé(s) par \
         `cause == OpenFailure::IndisponibleMotifPerdu`. Sur `DeviceGone` ou \
         `ServerUnreachable` le motif est DÉJÀ connu : parcourir tout /proc n'y \
         apprend rien, et une ligne `teneurs=aucun_teneur_visible` posée sur un \
         motif connu se lit comme une information alors qu'elle n'en est pas une"
    );
    assert!(
        source.contains("#[cfg(target_os=\"linux\")]fnjournaliser_les_teneurs_du_pcm("),
        "#3575 — le relevé lit `/proc` : il n'a de sens que sur Linux et doit \
         rester derrière `#[cfg(target_os = \"linux\")]`, sinon la caisse ne \
         compile plus sur Windows ni macOS"
    );
}

/// Le relevé ne doit RIEN faire d'autre que lire et journaliser.
///
/// Sur une P0 de sortie audio, une rustine qui « libère » un PCM — en tuant un
/// processus, en forçant une fermeture — rendrait muette la chaîne d'un
/// testeur. La garde interdit explicitement cette dérive dans la fonction.
#[test]
fn le_releve_ne_tue_personne_et_ne_ferme_rien() {
    let production = production();
    let debut = production
        .find("fn journaliser_les_teneurs_du_pcm(")
        .expect("#3575 — la fonction de relevé a disparu de `outputs/local.rs`");
    let corps = &production[debut..];
    let fin = corps
        .find("\n}\n")
        .expect("la fonction doit se refermer en colonne zéro");
    let corps = &corps[..fin];
    for interdit in ["kill(", "Command::new", "libc::", "unsafe"] {
        assert!(
            !corps.contains(interdit),
            "#3575 — `journaliser_les_teneurs_du_pcm` porte « {interdit} » : ce \
             relevé est un DIAGNOSTIC. Il lit /proc et écrit une ligne. \
             Reprendre le PCM de force, c'est risquer de rendre muette la \
             chaîne d'un testeur pour un mécanisme qui n'est pas établi."
        );
    }
}
