//! #3108 — un refus de sortie exclusive doit dire sa cause, pas figer la zone.
//!
//! Le correctif est livré (f4a1f716, v0.9.131 / v0.9.132). Ses gardes ne le
//! sont pas : elles vivent toutes dans `tune-core/src/outputs/local.rs`, un
//! fichier derrière `feature = "local-audio"` que le job `Test` de la CI
//! n'active pas (`--no-default-features --features oaat,cloud-relay,bandcamp`),
//! et dont les trois sites CoreAudio vivent en plus sous
//! `#[cfg(target_os = "macos")]`. Le seul travail macOS de la CI, `macos-pr`,
//! fait un `cargo check` : il ne COMPILE même pas les tests. Autrement dit, on
//! peut aujourd'hui retirer `record_feed_stall_failure` du chemin CoreAudio
//! exclusif et voir toute la CI rester verte, pendant que le défaut de
//! Bertrand du 01/09 revient à l'identique.
//!
//! D'où une garde de SITE : elle lit le TEXTE du fichier avec `include_str!`,
//! donc aucun `cfg` ni aucune fonctionnalité ne s'y applique. Elle tourne sur
//! Linux, sans carte son, dans le binaire `server_contracts` que le job `Test`
//! exécute sur chaque PR.
//!
//! Ce que la garde tient — les quatre maillons du côté serveur :
//!   1. les trois transports exclusifs (CoreAudio, ASIO, WASAPI) arment le
//!      canal sur un refus d'OUVERTURE ;
//!   2. le chemin CoreAudio exclusif LIT le verdict de blocage de l'anneau au
//!      lieu de le jeter — c'est la branche « figée à 2 s » du constat ;
//!   3. son vidage d'anneau reste borné (sans quoi le fil survit et la zone
//!      reste « en lecture ») ;
//!   4. tout passe par le canal DÉJÀ ouvert (`open_failure` →
//!      `take_output_failure()`), jamais par un second.
//!
//! R6 bis (#2219) : les trois bras exclusifs de `play_url` vivent chacun dans
//! leur module sous `tune-core/src/outputs/local/` (`bras_coreaudio.rs`,
//! `bras_asio.rs`, `bras_wasapi.rs`). La garde lit donc `local.rs` ET les
//! trois modules — concaténés, jamais remplacés : une assertion d'ABSENCE
//! (maillon 4) qui ne lirait plus que l'un d'eux perdrait du périmètre en
//! silence. Et elle vérifie que `play_url` APPELLE le bras CoreAudio : un
//! module écrit mais pas branché rendrait tout le reste complaisant.
//!
//! REF-8 (#2219) : le bras CoreAudio n'a plus de boucle propre. Il implémente
//! `BackendLocal` (`BackendCoreAudio`, dans `bras_coreaudio.rs`), son anneau
//! est créé par `ExclusiveOutput::new` (`coreaudio_exclusive.rs`), et il passe
//! par la boucle producteur commune de `local.rs`. Les maillons 2 et 3 sont
//! donc relus sur ce que le bras fait MAINTENANT — le verdict de blocage
//! remonte par le puits (`PuitsAnneauCoreAudio`), la boucle commune le
//! rapporte une fois sous le nom que le backend lui donne (REF-7), le bras
//! le relit ; le vidage borné est `drainer`. Rien n'est affaibli :
//! chaque assertion d'avant a son équivalent, et la contenance de l'anneau
//! est suivie dans le fichier qui la porte désormais.

const LOCAL: &str = include_str!("../../tune-core/src/outputs/local.rs");
const BRAS_COREAUDIO: &str = include_str!("../../tune-core/src/outputs/local/bras_coreaudio.rs");
const BRAS_ASIO: &str = include_str!("../../tune-core/src/outputs/local/bras_asio.rs");
const BRAS_WASAPI: &str = include_str!("../../tune-core/src/outputs/local/bras_wasapi.rs");
// REF-8 (#2219) : le backend CPAL partagé (trait `BackendLocal`, anneau, cascade
// d'ouverture, vidage) — lu EN PLUS de `local.rs`, jamais à sa place.
const BACKEND: &str = include_str!("../../tune-core/src/outputs/local/backend.rs");
// REF-8 (#2219) : la sortie CoreAudio exclusive possède son anneau (D2) — sa
// contenance vit là, et la garde qui la tenait la suit, sans cesser de lire
// le bras.
const COREAUDIO_EXCLUSIVE: &str =
    include_str!("../../tune-core/src/outputs/coreaudio_exclusive.rs");

/// Tout ce qui compose la sortie locale : `local.rs`, le backend CPAL, les
/// trois bras exclusifs et la sortie CoreAudio. C'est sur CE texte que
/// portent les assertions qui parlent de « la sortie locale » en général —
/// présence sur les trois transports, absence d'un second canal.
fn toute_la_sortie_locale() -> String {
    [
        LOCAL,
        BACKEND,
        BRAS_COREAUDIO,
        BRAS_ASIO,
        BRAS_WASAPI,
        COREAUDIO_EXCLUSIVE,
    ]
    .concat()
}

/// Le corps du chemin CoreAudio exclusif, délimité par ses deux journaux
/// d'entrée et de sortie, dans SON module. Borner la recherche évite qu'un
/// appel d'un AUTRE chemin (ASIO, WASAPI, cpal partagé) fasse passer une garde
/// qui prétend parler de celui-ci.
fn bloc_coreaudio_exclusif() -> &'static str {
    let debut = BRAS_COREAUDIO
        .find("\"local_audio_exclusive_mode_active\"")
        .expect("le journal d'entrée du chemin CoreAudio exclusif a disparu de bras_coreaudio.rs");
    let fin = BRAS_COREAUDIO[debut..]
        .find("\"local_audio_exclusive_stopped\"")
        .expect("le journal de sortie du chemin CoreAudio exclusif a disparu de bras_coreaudio.rs")
        + debut;
    &BRAS_COREAUDIO[debut..fin]
}

/// Le corps d'une méthode ou fonction de `bras_coreaudio.rs`, de sa
/// signature à la première accolade fermante de même retrait — les impl du
/// backend y sont indentées d'un niveau, les fonctions libres d'aucun.
fn corps_dans_le_bras(signature: &str) -> &'static str {
    let debut = BRAS_COREAUDIO
        .find(signature)
        .unwrap_or_else(|| panic!("`{signature}` a disparu de bras_coreaudio.rs"));
    // Le retrait est celui de la signature elle-même : la fermeture cherchée
    // est la première accolade posée au même retrait.
    let retrait = signature.len() - signature.trim_start().len();
    let fermeture = format!("\n{}}}", " ".repeat(retrait));
    let fin = BRAS_COREAUDIO[debut..]
        .find(&fermeture)
        .unwrap_or_else(|| panic!("`{signature}` ne se referme pas dans bras_coreaudio.rs"))
        + debut;
    &BRAS_COREAUDIO[debut..fin]
}

/// Le module du bras CoreAudio n'est une garde de rien s'il n'est pas appelé :
/// `play_url` doit l'invoquer sous sa bannière, là où le bloc vivait.
#[test]
fn play_url_appelle_le_bras_coreaudio_exclusif() {
    let debut = LOCAL
        .find("// ------- Exclusive mode path (macOS only) -------")
        .expect("la bannière du bras CoreAudio exclusif a disparu de play_url");
    let fin = LOCAL[debut..]
        .find("// ------- Exclusive mode path (Windows ASIO) -------")
        .expect("la bannière du bras ASIO a disparu de play_url")
        + debut;
    assert!(
        LOCAL[debut..fin].contains("bras_coreaudio::jouer_via_coreaudio("),
        "play_url n'appelle plus `bras_coreaudio::jouer_via_coreaudio` : les maillons 2 et 3 \
         de cette garde parlent d'un module que personne n'exécute (R6 bis, #2219)"
    );
}

/// Un appel à `nom` dont les 240 octets suivants contiennent le littéral
/// `"{argument}"`. Assez large pour traverser la mise en forme de `rustfmt`
/// (un appel sur cinq lignes), assez étroit pour ne pas déborder sur l'appel
/// suivant.
fn appelle_avec(texte: &str, nom: &str, argument: &str) -> bool {
    let littéral = format!("\"{argument}\"");
    texte.match_indices(nom).any(|(i, _)| {
        texte[i..]
            .chars()
            .take(240)
            .collect::<String>()
            .contains(&littéral)
    })
}

/// Maillon 1 — le refus d'OUVERTURE, sur les TROIS transports.
///
/// L'issue le dit dans sa portée : « un chemin corrigé et les autres nus » est
/// une famille de défauts de ce dépôt. Le constat vient de CoreAudio, mais ASIO
/// et WASAPI ont le même refus et doivent le dire pareil.
///
/// REF-8 (#2219) : CoreAudio passe par le trait — son refus est typé
/// (`OuvertureExclusiveRefusee { backend: "CoreAudio", … }`) et c'est
/// `RefusDOuverture::rapporter` qui appelle `record_exclusive_open_failure`.
/// La garde suit les trois maillons de cette chaîne au lieu d'un seul appel ;
/// ASIO et WASAPI, pas encore migrés, appellent toujours en direct.
#[test]
fn les_trois_transports_exclusifs_arment_le_canal_sur_un_refus_d_ouverture() {
    let sortie_locale = toute_la_sortie_locale();
    for transport in ["ASIO", "WASAPI"] {
        assert!(
            appelle_avec(&sortie_locale, "record_exclusive_open_failure(", transport),
            "aucun site n'appelle `record_exclusive_open_failure` pour {transport} : un refus \
             d'ouverture exclusive sur ce transport redevient muet, la zone reste figée sans \
             message (#3108)"
        );
    }

    // CoreAudio : le refus est CONSTRUIT sous son nom, aux deux sites qui
    // peuvent refuser (l'ouverture et le démarrage de l'AudioUnit)…
    let refus_coreaudio = BRAS_COREAUDIO
        .match_indices("RefusDOuverture::OuvertureExclusiveRefusee {")
        .filter(|(i, _)| {
            BRAS_COREAUDIO[*i..]
                .chars()
                .take(240)
                .collect::<String>()
                .contains("\"CoreAudio\"")
        })
        .count();
    assert!(
        refus_coreaudio >= 2,
        "le bras CoreAudio ne construit `OuvertureExclusiveRefusee {{ backend: \"CoreAudio\" }}` \
         qu'à {refus_coreaudio} site(s) : l'ouverture ET le démarrage doivent refuser sous ce \
         nom, sinon l'un des deux redevient muet (#3108, REF-8)"
    );
    // …le rapporteur du trait le passe au rapporteur historique…
    let rapporter = BACKEND
        .split("pub(super) fn rapporter(")
        .nth(1)
        .and_then(|s| s.split("\n    }").next())
        .expect("`RefusDOuverture::rapporter` doit rester identifiable dans backend.rs");
    assert!(
        rapporter.contains("RefusDOuverture::OuvertureExclusiveRefusee { backend, erreur } =>")
            && rapporter.contains(
                "record_exclusive_open_failure(backend, device_name, erreur, open_failure)"
            ),
        "`rapporter` ne passe plus `OuvertureExclusiveRefusee` à `record_exclusive_open_failure` : \
         le refus CoreAudio typé n'arme plus le canal (#3108, REF-8)"
    );
    // …et le bras RAPPORTE chaque refus avant d'éteindre la zone.
    let bloc = bloc_coreaudio_exclusif();
    let rapports = bloc
        .matches(".rapporter(&device_name, &open_failure)")
        .count();
    assert!(
        rapports >= 2,
        "le bras CoreAudio ne rapporte son refus qu'à {rapports} site(s) : ouverture et \
         démarrage doivent tous deux passer par `rapporter` avant `playing.store(false` (#3108)"
    );
    for (i, _) in bloc.match_indices(".rapporter(&device_name, &open_failure)") {
        let suite: String = bloc[i..].chars().take(240).collect();
        assert!(
            suite.contains("playing.store(false"),
            "un refus CoreAudio est rapporté sans éteindre la zone : elle resterait « en \
             lecture » sur un périphérique jamais ouvert (#3108)"
        );
    }
}

/// Maillon 2 — la branche « figée à 2 s » du constat.
///
/// L'ouverture a RÉUSSI et le rappel de rendu CoreAudio ne tire rien. L'anneau
/// se remplit une fois, `feed_ring_abortable` rend `false`, et ce verdict était
/// JETÉ aux sites de ce chemin — seul de tout le fichier à l'ignorer.
///
/// REF-8 + REF-7 (#2219) : le verdict traverse trois maillons au lieu d'un
/// drapeau local — le puits du backend le rend ET le mémorise ; la boucle
/// commune s'arrête dessus et rapporte la famine UNE fois, sous le nom que
/// le backend lui donne (`BackendLocal::nom` → « CoreAudio ») ; le bras relit
/// le témoin pour ne rien rendre de plus à un rappel mort. Chacun est tenu ;
/// en perdre un rend le blocage muet, ou le nomme d'un autre nom.
#[test]
fn le_chemin_coreaudio_exclusif_lit_le_verdict_de_blocage_au_lieu_de_le_jeter() {
    // (a) Le puits rend le verdict de `feed_ring_abortable` — il ne le jette
    // pas — et le mémorise pour le bras.
    let ecrire = corps_dans_le_bras("    fn ecrire(&mut self, mots: &[f32]) -> bool {");
    assert!(
        ecrire.contains("feed_ring_abortable(")
            && ecrire.contains("if !vivant {")
            && ecrire.contains("self.bloque.store(true"),
        "le puits CoreAudio ne mémorise plus le verdict de `feed_ring_abortable` : le blocage \
         de l'anneau retombe dans le vide (#3108, REF-8)"
    );
    assert!(
        ecrire.trim_end().ends_with("vivant"),
        "le puits CoreAudio ne REND plus le verdict : la boucle commune croirait le rappel \
         vivant et attendrait 5 s à CHAQUE bloc, position figée (#3108)"
    );

    // (b) Le bras passe par la boucle commune, et lui donne SON nom : c'est
    // elle qui appelle `record_feed_stall_failure(self.backend, …)` (vérifié
    // par `le_chemin_partage_et_son_enchainement_rapportent_aussi_leur_blocage`).
    let bloc = bloc_coreaudio_exclusif();
    assert!(
        bloc.contains("backend: backend.nom(),") && bloc.contains(".tourner("),
        "le bras CoreAudio n'appelle plus la boucle commune avec le nom de son backend : \
         le blocage serait rapporté sous un autre nom, ou pas du tout (#3108, REF-7)"
    );
    assert!(
        appelle_avec(
            BRAS_COREAUDIO,
            "fn nom(&self) -> &'static str {",
            "CoreAudio"
        ),
        "`BackendCoreAudio::nom` ne dit plus « CoreAudio » : le rapport de famine de ce \
         chemin porterait un autre nom que celui que les journaux ont toujours porté (#3108)"
    );
    // …et il ne rapporte PAS lui-même : la famine est dite une fois.
    assert!(
        !bloc.contains("record_feed_stall_failure("),
        "le bras CoreAudio rapporte la famine en plus de la boucle commune : deux rapports \
         pour un blocage, le second écrase le premier (#3108, REF-7)"
    );
    // (c) Et il relit le témoin du puits à la sortie de la boucle : rien de
    // plus n'est rendu à un rappel mort.
    assert!(
        bloc.contains("let feed_stalled = backend.puits_bloque();")
            && bloc.contains("if !feed_stalled {")
            && bloc.contains("etage.rendre_la_queue_du_dsp("),
        "le bras ne relit plus le verdict du puits avant de rendre la queue du DSP : il \
         attendrait 5 s de plus sur un rappel mort (#3108, REF-8)"
    );
}

/// La « fige à 2 s » n'est pas un délai nommé : c'est la CONTENANCE de
/// l'anneau exclusif, deux secondes d'audio à la cadence de la source. Il se
/// remplit une fois, puis plus rien n'avance. Changer ce facteur change le
/// chiffre que l'utilisateur voit et que le message rapporte : que ce soit un
/// geste conscient.
///
/// REF-8 (#2219, D2) : l'anneau appartient à la sortie — sa contenance vit
/// dans `ExclusiveOutput::new` (`coreaudio_exclusive.rs`), et le bras n'en
/// calcule plus. La garde suit le fichier porteur, et tient que le bras ouvre
/// bien par là.
#[test]
fn l_anneau_coreaudio_exclusif_tient_les_deux_secondes_du_constat() {
    let ouverture = COREAUDIO_EXCLUSIVE
        .split("    pub fn new(")
        .nth(1)
        .and_then(|s| s.split("\n    }").next())
        .expect("`ExclusiveOutput::new` doit rester identifiable dans coreaudio_exclusive.rs");
    assert!(
        ouverture.contains("let ring_cap = (sample_rate as usize) * (channels as usize) * 2;"),
        "la contenance de l'anneau CoreAudio exclusif a changé ou a quitté \
         `ExclusiveOutput::new` : c'est elle, et non un délai nommé, qui produit le « figée à \
         2 s » du constat de #3108 — mettre à jour le message et cette garde ensemble"
    );
    assert!(
        ouverture.contains("RingBuf::new_metered(ring_cap, starvation)"),
        "l'anneau CoreAudio n'est plus créé à cette contenance dans `ExclusiveOutput::new` \
         (D2, #2219)"
    );
    assert!(
        !BRAS_COREAUDIO.contains("let ring_cap =") && !BRAS_COREAUDIO.contains("RingBuf::new"),
        "le bras CoreAudio recalcule ou recrée un anneau : D2 (le backend possède son anneau) \
         n'est plus tenue"
    );
    let ouvrir = corps_dans_le_bras(
        "    fn ouvrir(demande: &DemandeDOuverture<'a>) -> Result<Self, RefusDOuverture> {",
    );
    assert!(
        ouvrir.contains("ExclusiveOutput::new("),
        "`BackendCoreAudio::ouvrir` n'ouvre plus par `ExclusiveOutput::new` : l'anneau de deux \
         secondes n'est plus celui que le bras alimente (#3108, REF-8)"
    );
}

/// Maillon 3 — sans borne de vidage, le fil de lecture survit à un rappel de
/// rendu mort : la zone reste « en lecture », et le réexamen des branchements
/// gèle avec elle. Les chemins ASIO, WASAPI et partagé bornaient déjà le leur ;
/// celui-ci, seul, tournait tant que l'anneau n'était pas vide.
///
/// REF-8 (#2219) : la boucle de vidage est `BackendCoreAudio::drainer` ; le
/// bras calcule sa borne par `drain_deadline_for` et la lui passe.
#[test]
fn le_vidage_de_l_anneau_coreaudio_exclusif_reste_borne() {
    let bloc = bloc_coreaudio_exclusif();
    assert!(
        bloc.contains("drain_deadline_for("),
        "le vidage de l'anneau CoreAudio exclusif n'a plus d'échéance : face à un rappel de \
         rendu mort il ne se vide JAMAIS, le fil reste vivant et la zone reste « en \
         lecture » (#3108)"
    );
    assert!(
        bloc.contains("backend.drainer(drain_deadline)"),
        "la borne calculée n'est plus celle que le vidage reçoit : une échéance calculée et \
         non passée ne borne rien (#3108, REF-8)"
    );
    let drainer =
        corps_dans_le_bras("    fn drainer(&mut self, borne: std::time::Duration) -> Vidage {");
    assert!(
        drainer.contains("let drain_deadline = borne;")
            && drainer.contains("drain_started.elapsed() >= drain_deadline"),
        "`drainer` ne compare plus le temps écoulé à sa borne : le vidage redevient sans fin \
         (#3108, REF-8)"
    );
    assert!(
        drainer.contains("\"local_audio_exclusive_drain_timeout\""),
        "l'échéance de vidage ne laisse plus de trace au journal : un vidage abandonné doit \
         être lisible après coup (#3108)"
    );
}

/// Le chemin cpal partagé — celui de l'arrachage d'un DAC USB sur macOS, où le
/// rappel d'erreur ne se déclenche jamais — et son enchaînement sans blanc
/// doivent rapporter le même blocage.
///
/// ⚠️ R1 (#2219) : les deux sites que ce test comptait étaient la MÊME ligne,
/// recopiée dans deux boucles jumelles. Il n'y en a plus qu'une, dans la
/// boucle producteur commune, et c'est elle qui sert les deux pistes. Compter
/// les copies ne veut donc plus rien dire ; on vérifie que le site unique
/// rapporte bien POUR LES DEUX — c'est ce que le comptage cherchait à dire.
#[test]
fn le_chemin_partage_et_son_enchainement_rapportent_aussi_leur_blocage() {
    let boucle = LOCAL
        .split("    fn tourner<E: Etage>(")
        .nth(1)
        .and_then(|s| s.split("\n#[async_trait::async_trait]").next())
        .expect("la boucle producteur commune doit rester identifiable (#3108)");

    // REF-7 (#2219) : la boucle est commune à tous les backends, le nom ne
    // l'est pas. Elle rapporte avec `self.backend`, que `play_url` remplit par
    // `BackendLocal::nom()` — et c'est `BackendCpal::nom` qui dit « CPAL ».
    // Trois maillons, tous vérifiés : le littéral seul dans la boucle serait
    // redevenu faux dès le second backend.
    assert!(
        boucle.contains("record_feed_stall_failure(")
            && boucle[boucle
                .find("record_feed_stall_failure(")
                .expect("site de blocage")..]
                .chars()
                .take(240)
                .collect::<String>()
                .contains("self.backend,"),
        "la boucle producteur commune ne rapporte plus le blocage de l'anneau avec le nom de \
         son backend : une piste qui meurt sur un rappel de rendu mort s'arrête sans un mot, \
         ou sous un nom qui n'est pas le sien (#3108, REF-7)"
    );
    assert!(
        appelle_avec(BACKEND, "fn nom(&self) -> &'static str {", "CPAL"),
        "`BackendCpal::nom` ne dit plus « CPAL » : le rapport de famine du chemin partagé \
         porterait un autre nom que celui que les journaux ont toujours porté (#3108, REF-7)"
    );
    assert_eq!(
        LOCAL.matches("backend: backend.nom(),").count(),
        2,
        "les DEUX boucles producteur de `play_url` — piste initiale, piste enchaînée — doivent \
         recevoir le nom du backend par `BackendLocal::nom()` (#3108, REF-7)"
    );

    // Et elle le rapporte pour les DEUX pistes : les deux noms d'événement
    // vivent au même endroit, sous le rôle de la boucle. C'est ce que les deux
    // sites d'avant garantissaient en se recopiant.
    for evenement in [
        "\"local_audio_stopped_feed_stall\"",
        "\"local_audio_gapless_stopped_feed_stall\"",
    ] {
        assert!(
            boucle.contains(evenement),
            "la boucle producteur ne journalise plus {evenement} : une piste enchaînée qui \
             meurt serait aussi muette qu'une première piste (#3108)"
        );
    }
}

/// Maillon 4 — un seul canal. `take_output_failure()` est décrit dans le code
/// comme « le canal déjà ouvert » ; la sortie locale ne doit pas en ouvrir un
/// second en émettant elle-même sur le bus d'événements.
#[test]
fn la_remontee_passe_par_le_canal_deja_ouvert_et_pas_par_un_second() {
    assert!(
        LOCAL.contains("fn take_output_failure(&self) -> Option<String> {")
            && LOCAL.contains("self.open_failure.lock()"),
        "`take_output_failure()` ne draine plus `open_failure` : le canal que le poller lit à \
         chaque tour est rompu (#3108)"
    );
    // Porter sur l'APPEL, pas sur le nom de l'événement : `local.rs` cite
    // `zone.playback_error` dans le commentaire de `record_feed_stall_failure`
    // pour dire OÙ va le canal — une mention documentaire, pas une émission.
    // La première rédaction de cette garde cherchait le nom nu et partait
    // rouge sur du texte de commentaire.
    // R6 bis (#2219) : l'absence se vérifie sur `local.rs` ET les trois bras —
    // ne lire que l'un d'eux laisserait un `.emit(` s'installer dans un autre.
    assert!(
        !toute_la_sortie_locale().contains(".emit("),
        "la sortie locale émet elle-même sur le bus d'événements : c'est un SECOND canal, en \
         doublon de `take_output_failure()` que le poller draine déjà à chaque tour (#3108)"
    );
}

/// REF-8 (#2219) — l'anneau du chemin CPAL partagé tient aussi deux secondes.
///
/// D2 : le backend possède son anneau, et `play_url` ne calcule plus sa
/// contenance. Elle a suivi le déplacement telle quelle : `cadence × canaux ×
/// 2`, aux deux cadences de la cascade. C'est elle qui produit le « figée à
/// 2 s » de #3108 quand le rappel meurt ; la garde suit le fichier qui la
/// porte désormais, `local/backend.rs`, sans cesser de lire `local.rs`.
#[test]
fn l_anneau_cpal_partage_tient_aussi_les_deux_secondes_du_constat() {
    let sans_blancs: String = BACKEND.chars().filter(|c| !c.is_whitespace()).collect();
    for contenance in [
        "letring_cap=(output_config.sample_rateasusize)*(output_config.channelsasusize)*2;",
        "letring_cap_fb=(source_cfg.sample_rateasusize)*(source_cfg.channelsasusize)*2;",
        "letcap=(cand.sample_rateasusize)*(cand.channelsasusize)*2;",
    ] {
        assert!(
            sans_blancs.contains(contenance),
            "la contenance de l'anneau CPAL partagé a changé ou a quitté \
             `local/backend.rs` (`{contenance}` introuvable) : c'est elle, et non un \
             délai nommé, qui produit le « figée à 2 s » du constat de #3108 — mettre \
             à jour le message et cette garde ensemble"
        );
    }
    assert!(
        !LOCAL.contains("let ring_cap ="),
        "`play_url` recalcule une contenance d'anneau : D2 (le backend possède son \
         anneau) n'est plus tenue"
    );
}
