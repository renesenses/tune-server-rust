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

const LOCAL: &str = include_str!("../../tune-core/src/outputs/local.rs");

/// Le corps du chemin CoreAudio exclusif, délimité par ses deux journaux
/// d'entrée et de sortie. Borner la recherche évite qu'un appel d'un AUTRE
/// chemin (ASIO, WASAPI, cpal partagé) fasse passer une garde qui prétend
/// parler de celui-ci.
fn bloc_coreaudio_exclusif() -> &'static str {
    let debut = LOCAL
        .find("\"local_audio_exclusive_mode_active\"")
        .expect("le journal d'entrée du chemin CoreAudio exclusif a disparu de local.rs");
    let fin = LOCAL[debut..]
        .find("\"local_audio_exclusive_stopped\"")
        .expect("le journal de sortie du chemin CoreAudio exclusif a disparu de local.rs")
        + debut;
    &LOCAL[debut..fin]
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
#[test]
fn les_trois_transports_exclusifs_arment_le_canal_sur_un_refus_d_ouverture() {
    for transport in ["CoreAudio", "ASIO", "WASAPI"] {
        assert!(
            appelle_avec(LOCAL, "record_exclusive_open_failure(", transport),
            "aucun site n'appelle `record_exclusive_open_failure` pour {transport} : un refus \
             d'ouverture exclusive sur ce transport redevient muet, la zone reste figée sans \
             message (#3108)"
        );
    }
}

/// Maillon 2 — la branche « figée à 2 s » du constat.
///
/// L'ouverture a RÉUSSI et le rappel de rendu CoreAudio ne tire rien. L'anneau
/// se remplit une fois, `feed_ring_abortable` rend `false`, et ce verdict était
/// JETÉ aux sites de ce chemin — seul de tout le fichier à l'ignorer.
#[test]
fn le_chemin_coreaudio_exclusif_lit_le_verdict_de_blocage_au_lieu_de_le_jeter() {
    let bloc = bloc_coreaudio_exclusif();
    let armements = bloc.matches("feed_stalled = true").count();
    assert!(
        armements >= 2,
        "le chemin CoreAudio exclusif n'arme le drapeau de blocage qu'à {armements} site(s) : \
         un `feed_ring_abortable` dont le verdict retombe dans le vide rend la zone muette et \
         figée sur la position atteinte (#3108)"
    );
    assert!(
        bloc.contains("if feed_stalled {"),
        "le drapeau de blocage n'est plus relu à la sortie de la boucle : plus personne ne \
         rapporte le blocage (#3108)"
    );
    assert!(
        appelle_avec(bloc, "record_feed_stall_failure(", "CoreAudio"),
        "le chemin CoreAudio exclusif ne rapporte plus son blocage par \
         `record_feed_stall_failure` : c'est exactement le silence du constat du 01/09 (#3108)"
    );
    assert!(
        bloc.contains("position_ms.load("),
        "le rapport de blocage ne porte plus la position où l'écran s'est figé — le seul \
         chiffre qui relie ce que le testeur voit (« 2 s ») à ce que le journal dit (#3108)"
    );
}

/// La « fige à 2 s » n'est pas un délai nommé : c'est la CONTENANCE de
/// l'anneau exclusif, deux secondes d'audio à la cadence de la source. Il se
/// remplit une fois, puis plus rien n'avance. Changer ce facteur change le
/// chiffre que l'utilisateur voit et que le message rapporte : que ce soit un
/// geste conscient.
#[test]
fn l_anneau_coreaudio_exclusif_tient_les_deux_secondes_du_constat() {
    let bloc = bloc_coreaudio_exclusif();
    assert!(
        bloc.contains("let ring_cap = (sample_rate as usize) * (channels as usize) * 2;"),
        "la contenance de l'anneau CoreAudio exclusif a changé : c'est elle, et non un délai \
         nommé, qui produit le « figée à 2 s » du constat de #3108 — mettre à jour le message \
         et cette garde ensemble"
    );
}

/// Maillon 3 — sans borne de vidage, le fil de lecture survit à un rappel de
/// rendu mort : la zone reste « en lecture », et le réexamen des branchements
/// gèle avec elle. Les chemins ASIO, WASAPI et partagé bornaient déjà le leur ;
/// celui-ci, seul, tournait tant que l'anneau n'était pas vide.
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
        bloc.contains("\"local_audio_exclusive_drain_timeout\""),
        "l'échéance de vidage ne laisse plus de trace au journal : un vidage abandonné doit \
         être lisible après coup (#3108)"
    );
}

/// Le chemin cpal partagé — celui de l'arrachage d'un DAC USB sur macOS, où le
/// rappel d'erreur ne se déclenche jamais — et son enchaînement sans blanc
/// doivent rapporter le même blocage. Deux sites, tous deux livrés par le même
/// correctif, tous deux hors de portée de la CI.
#[test]
fn le_chemin_partage_et_son_enchainement_rapportent_aussi_leur_blocage() {
    let sites = LOCAL
        .match_indices("record_feed_stall_failure(")
        .filter(|(i, _)| {
            LOCAL[*i..]
                .chars()
                .take(240)
                .collect::<String>()
                .contains("\"CPAL\"")
        })
        .count();
    assert!(
        sites >= 2,
        "seulement {sites} site(s) cpal rapportent un blocage d'anneau : il en faut deux — la \
         boucle de lecture principale ET l'enchaînement sans blanc, sans quoi une piste \
         enchaînée qui meurt est aussi muette qu'une première piste (#3108)"
    );
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
    assert!(
        !LOCAL.contains(".emit("),
        "la sortie locale émet elle-même sur le bus d'événements : c'est un SECOND canal, en \
         doublon de `take_output_failure()` que le poller draine déjà à chaque tour (#3108)"
    );
}
