//! #3318 — la clé qui joint les deux moitiés de la mesure.
//!
//! ## Le trou que ces épreuves bouchent
//!
//! Quand la lecture se coupe sur une sortie locale, deux lignes de journal
//! décrivent le même instant depuis les deux bouts du tuyau :
//!
//! * côté CONSOMMATEUR — `local_audio_slow_read` / `local_audio_read_error`,
//!   écrites par le fil de lecture de `outputs/local.rs` : « j'ai attendu »,
//!   ou « on m'a rendu une erreur » ;
//! * côté PRODUCTEUR — `stream_delivery_stall`
//!   (`tune-stream-http/src/lib.rs`) : « le canal était vide et j'attendais le
//!   décodeur » (`attente_producteur_ms`) ou « les octets étaient là et ne
//!   partaient pas » (`attente_transport_ms`).
//!
//! Lues ENSEMBLE, elles tranchent : si le producteur a servi à l'heure, le
//! blocage est en aval ; s'il n'a rien servi, c'est lui. Lues séparément,
//! elles ne tranchent rien.
//!
//! Or la ligne du producteur portait `stream_id` et celles du consommateur ne
//! portaient **ni `stream_id`, ni `zone_id`, ni `device`** — seulement des
//! octets et des millisecondes. La seule jointure possible était donc
//! l'horodatage, valide uniquement si UNE SEULE zone joue pendant la fenêtre.
//! C'est ce qui a laissé le dossier en plan : les deux mesures existaient et
//! ne se joignaient pas.
//!
//! ## Pourquoi ces épreuves-ci, et pas d'autres
//!
//! Le site d'émission vit au fond d'un `std::thread::spawn` qui veut un
//! périphérique audio ouvert et un flux HTTP vivant : aucun test unitaire ne
//! l'atteint. Les deux écritures ont donc été SORTIES en fonctions nommées,
//! que ces épreuves appellent pour de vrai en capturant le journal.
//!
//! Et parce que « écrit mais pas branché » est exactement le défaut qui
//! produirait un correctif inutile ici, les deux dernières épreuves relisent
//! le code de production pour vérifier que le fil de lecture les appelle bien
//! AVEC la clé — une fonction juste que personne n'appelle avec le bon
//! argument ne corrigerait rien.

use super::{FLUX_INCONNU, journaliser_erreur_de_lecture, journaliser_lecture_lente};

/// Identifiant de session tel que le serveur de flux en fabrique
/// (`http::streamer`), et tel qu'il apparaît dans `stream_delivery_stall`.
const FLUX: &str = "e32c865e-9a1f-4d20-9b77-2c0f5a1d3b44";

#[derive(Clone, Default)]
struct JournalCapture(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl JournalCapture {
    fn texte(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

impl std::io::Write for JournalCapture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for JournalCapture {
    type Writer = JournalCapture;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// Capture le journal de niveau WARN émis par `emission`.
fn journal_de(emission: impl FnOnce()) -> String {
    let journal = JournalCapture::default();
    let abonne = tracing_subscriber::fmt()
        .with_writer(journal.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::WARN)
        .finish();
    let garde = tracing::subscriber::set_default(abonne);
    emission();
    drop(garde);
    journal.texte()
}

/// L'attente : `local_audio_slow_read` porte de quoi la joindre au flux et de
/// quoi savoir QUELLE sortie a attendu.
#[test]
fn une_lecture_lente_porte_la_cle_du_flux_et_l_appareil() {
    let log = journal_de(|| {
        journaliser_lecture_lente(
            "DENAFRIPS USB Audio V3.14, USB Audio",
            Some(FLUX),
            65_536,
            38_594,
            6_621_166,
        )
    });

    assert!(
        log.contains("local_audio_slow_read"),
        "l'événement doit rester cherchable sous son nom : {log}"
    );
    assert!(
        log.contains(&format!("stream_id={FLUX}")),
        "sans `stream_id`, cette ligne ne se joint au `stream_delivery_stall` \
         du producteur que par l'horodatage — jointure qui ne vaut que si une \
         seule zone joue : {log}"
    );
    assert!(
        log.contains("device=DENAFRIPS USB Audio V3.14"),
        "sans l'appareil, on ne sait pas laquelle des sorties énumérées a \
         attendu : {log}"
    );
    assert!(
        log.contains("wait_ms=38594"),
        "la durée de l'attente reste la mesure : {log}"
    );
    assert!(
        log.contains("total_bytes_read=6621166"),
        "le cumul d'octets déjà lus reste la mesure : {log}"
    );
}

/// La coupure franche : `local_audio_read_error` porte la même clé que
/// l'attente. Les deux sortent du même `reader.read()` ; les séparer par leur
/// clé rendrait la moitié des relevés inutilisables.
#[test]
fn une_erreur_de_lecture_porte_la_cle_du_flux_et_l_appareil() {
    let log = journal_de(|| {
        journaliser_erreur_de_lecture(
            "DENAFRIPS USB Audio V3.14, USB Audio",
            Some(FLUX),
            "request or response body error",
            91_090_944,
        )
    });

    assert!(
        log.contains("local_audio_read_error"),
        "l'événement doit rester cherchable sous son nom : {log}"
    );
    assert!(
        log.contains(&format!("stream_id={FLUX}")),
        "la coupure franche doit se joindre au producteur comme l'attente : \
         {log}"
    );
    assert!(log.contains("device=DENAFRIPS USB Audio V3.14"), "{log}");
    assert!(
        log.contains("error=request or response body error"),
        "l'erreur rendue par le client HTTP reste écrite telle quelle : {log}"
    );
    assert!(log.contains("total_bytes_read=91090944"), "{log}");
}

/// Une URL sans identifiant de flux — une radio, un fichier servi par un
/// tiers — écrit un tiret, PAS un champ absent.
///
/// Un champ qui disparaît se lit comme une ligne d'une autre version, et
/// pousse à conclure que le serveur ne mesure pas. Un tiret dit « mesuré,
/// pas d'identifiant à donner ».
#[test]
fn un_flux_sans_identifiant_ecrit_un_tiret_et_non_un_champ_absent() {
    let log = journal_de(|| journaliser_lecture_lente("Salon", None, 4_096, 7_000, 10_000));

    assert!(
        log.contains(&format!("stream_id={FLUX_INCONNU}")),
        "le champ doit être présent même sans identifiant : {log}"
    );
}

/// La clé n'est pas inventée : elle est RELUE dans l'URL que le fil de
/// lecture est en train de tirer, par la découpe du serveur de flux lui-même.
#[test]
fn la_cle_est_relue_dans_l_url_du_flux_interne() {
    let url = format!("http://192.168.1.20:8888/stream/{FLUX}.flac");

    assert_eq!(
        crate::poller::decisions::stream_id_de_l_uri(Some(&url)).as_deref(),
        Some(FLUX),
        "l'identifiant que le producteur journalise est celui qui est dans \
         l'URL : url = {url}"
    );
    assert_eq!(
        crate::poller::decisions::stream_id_de_l_uri(Some("http://radio.example/aac")),
        None,
        "une URL qui n'est pas un flux Tune ne doit pas fabriquer une clé"
    );
}

// ───────────────────────────────────────────────────────────────────────────
// « Écrit mais pas branché » — les deux épreuves qui cherchent l'APPELANT.
//
// Les fonctions ci-dessus peuvent être parfaites et n'être appelées avec
// aucune clé : le journal du testeur serait alors identique à celui d'avant,
// et le correctif ne servirait à rien sans qu'aucune épreuve ne rougisse.
// ───────────────────────────────────────────────────────────────────────────

/// ⚠️ `include_str!` rend le fichier ENTIER. On coupe à ce module pour que les
/// motifs cherchés ne puissent pas se trouver eux-mêmes dans les messages
/// d'assertion qui suivent — même découpe que
/// `renseignement_materiel_guard.rs` (#2082).
fn code_de_production() -> &'static str {
    const TOUT: &str = include_str!("../local.rs");
    const BORNE: &str = "mod cle_de_correlation_i3318";
    let fin = TOUT
        .find(BORNE)
        .unwrap_or_else(|| panic!("ce module a été renommé : la découpe ne protège plus rien"));
    &TOUT[..fin]
}

/// Le fil de lecture calcule la clé À PARTIR DE L'URL qu'il tire, une fois,
/// avant sa boucle.
#[test]
fn le_fil_de_lecture_relit_la_cle_dans_son_url() {
    assert!(
        code_de_production().contains(
            "let cle_de_flux = crate::poller::decisions::stream_id_de_l_uri(Some(&url));"
        ),
        "le fil de lecture doit RELIRE l'identifiant dans l'URL qu'il tire. \
         Le fabriquer autrement, ou l'omettre, redonnerait deux journaux qui \
         ne se joignent pas (#3318)."
    );
}

/// Et il passe cette clé — et l'appareil — aux DEUX écritures.
#[test]
fn les_deux_ecritures_sont_appelees_avec_la_cle_et_l_appareil() {
    let code = code_de_production();

    for (fonction, symptome) in [
        ("journaliser_lecture_lente(", "l'attente de lecture"),
        ("journaliser_erreur_de_lecture(", "la coupure franche"),
    ] {
        let debut = code
            .find(fonction)
            .unwrap_or_else(|| panic!("{symptome} n'est plus journalisée par `{fonction}`"));
        let bloc = &code[debut..];
        let fin = bloc
            .find(");")
            .unwrap_or_else(|| panic!("appel de `{fonction}` non délimité"));
        let appel = &bloc[..fin];

        assert!(
            appel.contains("cle_de_flux"),
            "{symptome} est journalisée SANS la clé de flux : la ligne \
             repartirait sans de quoi la joindre au producteur (#3318). \
             Appel lu : {appel}"
        );
        // R1 (#2219) : le producteur porte désormais le nom d'appareil comme
        // CHAMP (`self.device_name`, déjà un `&str`) au lieu de l'emprunter à
        // une variable locale. L'exigence est inchangée — l'appareil doit être
        // un argument de l'appel — seule l'esperluette a disparu.
        assert!(
            appel.contains("device_name"),
            "{symptome} est journalisée SANS l'appareil : on ne saurait pas \
             laquelle des sorties a lâché (#3318). Appel lu : {appel}"
        );
    }
}
