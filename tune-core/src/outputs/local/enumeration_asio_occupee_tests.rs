use super::{AsioEnumerationPlan, plan_audio_enumeration};

/// #1267 — pendant qu'une session exclusive verrouille le pilote ASIO,
/// l'énumération générique doit servir le cache, pas rouvrir le pilote.
#[test]
fn le_pilote_asio_occupe_fait_servir_le_cache() {
    assert_eq!(
        plan_audio_enumeration("asio", true),
        AsioEnumerationPlan::ServeCache
    );
    // La valeur vient de la base ou de l'environnement : la casse varie.
    assert_eq!(
        plan_audio_enumeration("ASIO", true),
        AsioEnumerationPlan::ServeCache
    );
    assert_eq!(
        plan_audio_enumeration("Asio", true),
        AsioEnumerationPlan::ServeCache
    );
}

/// Pilote libre : rien ne change, on interroge le matériel. Sans cela le
/// correctif transformerait la panne en une liste figée à vie.
#[test]
fn le_pilote_asio_libre_laisse_sonder() {
    assert_eq!(
        plan_audio_enumeration("asio", false),
        AsioEnumerationPlan::Probe
    );
}

/// Les autres backends n'ouvrent JAMAIS le host ASIO — `auto` passe par
/// WASAPI. Les priver de balayage parce qu'une zone ASIO joue ferait
/// disparaître les DAC USB de la liste (défaut #1084, à ne pas rejouer).
#[test]
fn les_autres_backends_sondent_meme_pilote_asio_occupe() {
    for backend in ["auto", "wasapi", "AUTO", "coreaudio", "alsa", ""] {
        assert_eq!(
            plan_audio_enumeration(backend, true),
            AsioEnumerationPlan::Probe,
            "« {backend} » n'ouvre pas le host ASIO : il doit continuer à sonder"
        );
    }
}

/// Le VERROU de branchement.
///
/// La décision ci-dessus est éprouvée partout ; son BRANCHEMENT, lui, ne
/// se voit qu'à l'exécution sous Windows avec un pilote ASIO réel, que
/// personne ne peut jouer en CI. Sans ce garde on pourrait supprimer le
/// retour anticipé de `list_audio_devices_with_backend` et les trois tests
/// ci-dessus resteraient verts pendant que #1267 reviendrait.
///
/// Même procédé que `chaque_sortie_de_select_host_enregistre_le_backend_ouvert`.
#[test]
fn list_audio_devices_with_backend_consulte_le_plan_avant_de_sonder() {
    let source = include_str!("../local.rs");
    let debut = source
        .find("pub fn list_audio_devices_with_backend(")
        .expect("list_audio_devices_with_backend introuvable");
    let corps = &source[debut..];
    let fin = corps
        .find("\n}\n")
        .expect("fin du corps de list_audio_devices_with_backend introuvable");
    let corps = &corps[..fin];

    let pos_plan = corps
        .find("plan_audio_enumeration(backend, asio_device_busy())")
        .expect(
            "list_audio_devices_with_backend ne consulte plus le plan : la page Diagnostic \
         rouvrira le pilote ASIO pendant que la sortie tente de se verrouiller (#1267)",
        );
    let pos_sonde = corps
        .find("list_audio_devices_uncached(")
        .expect("le balayage matériel a disparu du corps");
    assert!(
        pos_plan < pos_sonde,
        "le plan doit être consulté AVANT le balayage matériel, sinon le pilote est \
         déjà rouvert quand on décide de ne pas le rouvrir (#1267)"
    );
    assert!(
        corps.contains("return cached_audio_devices();"),
        "le chemin ServeCache doit rendre le dernier inventaire connu, pas une liste vide : \
         une zone ASIO active ferait autrement disparaître toutes les sorties de l'interface"
    );
}
