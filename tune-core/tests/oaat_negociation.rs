//! Les deux chemins qui court-circuitaient le handshake doivent attendre leur
//! réponse.
//!
//! DSD natif et PCM direct proposaient un format puis appelaient `send_play`
//! **sans jamais consulter `response_rx`** : un `FormatReject` était ignoré, un
//! `FormatCounter` aussi, et la réponse non consommée pouvait être prise pour
//! celle d'une négociation ultérieure (JP Robbe, #2282).
//!
//! Ces chemins sont de longs `tokio::spawn` qui parlent à un endpoint réseau :
//! ils ne se testent pas en unitaire. On verrouille donc les points d'appel
//! dans la source, comme `dsp_track_boundary.rs` le fait pour la frontière de
//! piste.
//!
//! ⚠️ Ce test épingle DEUX sites nommés, pas une invariante générale. Ma
//! première version comptait les propositions et les lectures de réponse : elle
//! restait verte en débranchant un site, parce que `response_rx.recv()` sert
//! aussi à autre chose que la négociation. Un compte approximatif ne mord pas.

use std::path::Path;

const SRC: &str = "src/outputs/oaat/output.rs";

fn source() -> String {
    std::fs::read_to_string(Path::new(SRC)).expect("output.rs doit être lisible")
}

/// Le nombre de POINTS D'APPEL du helper, définition exclue.
fn appels_au_helper(src: &str) -> usize {
    src.matches("attendre_accord_format(")
        .count()
        .saturating_sub(1)
}

#[test]
fn le_dsd_natif_et_le_pcm_direct_attendent_leur_reponse() {
    let src = source();
    assert_eq!(
        appels_au_helper(&src),
        2,
        "les deux chemins qui proposaient puis jouaient sans regarder la réponse \
         doivent appeler `attendre_accord_format` — un `FormatReject` ignoré \
         lance la lecture alors que l'endpoint a dit non (#2282)"
    );
}

/// Et le helper doit rester le seul endroit qui décide : s'il cessait de
/// refuser, les deux sites appelleraient une fonction devenue complaisante.
#[test]
fn le_helper_refuse_bien_les_trois_cas() {
    let src = source();
    let debut = src
        .find("async fn attendre_accord_format(")
        .expect("le helper doit exister");
    let fin = src[debut..]
        .find("\n}\n")
        .map(|i| debut + i)
        .unwrap_or(src.len());
    let corps = &src[debut..fin];

    assert!(
        corps.contains("FormatReject"),
        "un refus doit être traité, pas ignoré"
    );
    assert!(
        corps.contains("contre_proposition_honorable"),
        "une contre-proposition doit être jugée sur les six champs négociés"
    );
    assert!(
        corps.contains("Ok(None)"),
        "un endpoint qui ferme pendant la négociation n'est pas un accord"
    );
}
