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
fn tous_les_chemins_attendent_leur_reponse() {
    let src = source();
    assert_eq!(
        appels_au_helper(&src),
        6,
        "les chemins qui proposent un format doivent tous appeler \
         `attendre_accord_format` — connexion/reconnexion, DSD natif, PCM direct, \
         chemin principal et transition gapless. Un `FormatReject` ignoré lance \
         la lecture alors que l'endpoint a dit non ; un `FormatAccept` étranger \
         décale le flux suivant (#2282, #2283)"
    );
}

/// Le helper doit rester le SEUL endroit qui décide.
///
/// ⚠️ Ce test est un garde de BRANCHEMENT, pas une preuve de comportement.
/// Ma première version lisait le texte source du helper et se contentait d'y
/// trouver les chaînes `FormatReject` et `Ok(None)` : elle restait verte quand
/// on remplaçait le bras `FormatReject => Err(..)` par `Ok(())` — le test censé
/// garantir qu'un refus empêche la lecture passait alors que le refus était
/// accepté (JP Robbe, #2297).
///
/// La décision vit maintenant dans `juger_reponse`, une fonction pure, et c'est
/// elle qui est APPELÉE — sur les huit issues — par
/// `outputs::oaat::integration_test::juger_reponse_decide_les_huit_issues`.
/// Ici on vérifie seulement que le helper délègue toujours à cette fonction au
/// lieu de rejuger dans son coin.
///
/// ## Depuis #2758, l'attente vit un cran plus bas
///
/// `attendre_accord_format` prend un `ConnectedEndpoint`, qui ne se construit
/// que par un `connect()` TCP suivi d'un handshake : rien de tout cela ne se
/// pilote depuis un test. La boucle a donc été déplacée dans
/// `attendre_accord_format_sur`, qui prend le CANAL de réponses — un `mpsc`
/// ordinaire qu'un test alimente à la main. C'est ce déplacement qui rend le
/// défaut du Zicmu éprouvable sans DAC.
///
/// Le garde suit la mécanique là où elle vit, et ajoute une exigence : le
/// helper qui reste doit **déléguer**, pas rejuger. S'il se remettait à
/// décider, sa décision échapperait de nouveau aux tests de comportement — ce
/// qui est exactement le piège de #2297.
#[test]
fn le_helper_delegue_sa_decision_a_la_fonction_pure() {
    let src = source();

    let corps_de = |signature: &str| -> String {
        let debut = src
            .find(signature)
            .unwrap_or_else(|| panic!("`{signature}` doit exister dans {SRC}"));
        let fin = src[debut..]
            .find("\n}\n")
            .map(|i| debut + i)
            .unwrap_or(src.len());
        src[debut..fin].to_string()
    };

    // 1. L'enveloppe ne fait que passer le canal : aucun jugement chez elle.
    let enveloppe = corps_de("async fn attendre_accord_format(");
    assert_eq!(
        enveloppe.matches("attendre_accord_format_sur(").count(),
        1,
        "`attendre_accord_format` doit déléguer à `attendre_accord_format_sur` : \
         c'est ce qui garantit que les six points d'appel de production passent \
         par la mécanique réellement testée, et non par une copie (#2758)"
    );
    assert_eq!(
        enveloppe.matches("juger_reponse(").count(),
        0,
        "l'enveloppe ne doit rien juger elle-même : tout ce qu'elle déciderait \
         serait hors de portée des tests (#2297)"
    );

    // 2. La mécanique délègue les trois issues à la fonction pure.
    let corps = corps_de("pub(crate) async fn attendre_accord_format_sur(");
    assert_eq!(
        corps.matches("juger_reponse(").count(),
        3,
        "la boucle doit déléguer les TROIS issues de l'attente — réponse reçue, \
         canal fermé, silence — à `juger_reponse` ; toute décision qu'elle prend \
         elle-même échappe aux tests de comportement (#2297)"
    );
    assert!(
        !corps.contains("FormatReject"),
        "la boucle ne doit pas statuer sur les messages : c'est le rôle de \
         `juger_reponse`, qui est testée pour ce qu'elle décide (#2297)"
    );
}
