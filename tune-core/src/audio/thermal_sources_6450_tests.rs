//! #6450 — le choix entre les sources de température de Windows, avec des
//! sources simulées : la lecture réelle (PDH, WMI) ne s'exécute que sur une
//! machine Windows.
//!
//! Le poste du testeur (Windows 11, Ryzen 7 3800X) ne rendait rien par la
//! seule source lue, et rien ne disait pourquoi. Ces témoins tiennent que :
//! * une source qui échoue ne cache plus les suivantes ;
//! * chaque source essayée laisse son motif d'échec ;
//! * sans aucune source lisible, la température est inconnue (et les
//!   analyses gardent leur vitesse, voir `thermal_repli_5410_tests.rs`) ;
//! * le journal ne parle qu'au changement de constat ;
//! * les sources ne sont pas relues à chaque requête, même quand elles
//!   échouent toutes.

use super::super::{
    LectureThermique, MotifIllisible, ThermalGate, Verdict, celsius_depuis_dixiemes_de_kelvin,
    celsius_depuis_kelvins, largeur_selon_la_chaleur, lecture_de,
};
use super::{
    EchecDeSource, Essai, RELECTURE_DES_SOURCES, SourceDeTemperature, journaliser_si_change,
    premiere_source_lisible, relire_au_besoin,
};

/// Une source simulée : ce qu'elle rend, et combien de fois elle a été lue.
struct Simulee {
    nom: &'static str,
    rend: Result<Vec<f64>, EchecDeSource>,
    conversion: fn(f64) -> Option<f64>,
    lectures: std::cell::Cell<u32>,
}

impl Simulee {
    fn new(nom: &'static str, rend: Result<Vec<f64>, EchecDeSource>) -> Self {
        Simulee {
            nom,
            rend,
            conversion: celsius_depuis_dixiemes_de_kelvin,
            lectures: std::cell::Cell::new(0),
        }
    }

    fn en_kelvins(mut self) -> Self {
        self.conversion = celsius_depuis_kelvins;
        self
    }
}

impl SourceDeTemperature for Simulee {
    fn nom(&self) -> &'static str {
        self.nom
    }

    fn lire_brut(&self) -> Result<Vec<f64>, EchecDeSource> {
        self.lectures.set(self.lectures.get() + 1);
        self.rend.clone()
    }

    fn en_celsius(&self, brut: f64) -> Option<f64> {
        (self.conversion)(brut)
    }
}

fn absente(code: &str) -> EchecDeSource {
    EchecDeSource::Absente(format!("PdhAddEnglishCounterW : PDH {code}"))
}

/// 🔴 Le cœur de #6450 : la première source échoue, la suivante répond.
/// Avant, une seule source était lue et son échec valait « illisible ».
#[test]
fn une_source_qui_echoue_ne_cache_plus_les_suivantes_6450() {
    let pdh = Simulee::new("pdh_high_precision_temperature", Err(absente("0xC0000BB8")));
    let pdh_k = Simulee::new("pdh_temperature", Err(absente("0xC0000BB8"))).en_kelvins();
    // 3231 dixièmes de kelvin = 49,95 °C ; deux zones, la plus chaude compte.
    let wmi = Simulee::new(
        "wmi_cimv2_thermal_zone_information",
        Ok(vec![3_131.0, 3_231.0]),
    );
    let acpi = Simulee::new("wmi_root_wmi_msacpi_thermal_zone", Ok(vec![3_500.0]));

    let (temp, essais) = premiere_source_lisible(&[&pdh, &pdh_k, &wmi, &acpi]);

    let temp = temp.expect("la source WMI répond : la température doit être lue");
    assert!((temp - 49.95).abs() < 1e-9, "{temp}");
    assert_eq!(
        essais.iter().map(|e| e.source).collect::<Vec<_>>(),
        [
            "pdh_high_precision_temperature",
            "pdh_temperature",
            "wmi_cimv2_thermal_zone_information"
        ],
        "chaque source essayée est rapportée, jusqu'à la retenue"
    );
    assert_eq!(essais[0].resultat, Err(absente("0xC0000BB8")));
    assert_eq!(
        acpi.lectures.get(),
        0,
        "une fois une source lisible trouvée, les suivantes ne sont pas lues"
    );
    // Lue, la température passe la garde comme sous Linux.
    let lecture = lecture_de(Some(temp));
    assert_eq!(lecture, LectureThermique::Lisible(temp));
}

/// Toutes les sources échouent : chaque motif est rapporté, la température
/// est inconnue, et les analyses ne sont pas bridées pour autant.
#[test]
fn sans_aucune_source_lisible_chaque_motif_est_rapporte_6450() {
    let pdh = Simulee::new("pdh_high_precision_temperature", Err(absente("0xC0000BB8")));
    let pdh_k = Simulee::new("pdh_temperature", Ok(vec![])).en_kelvins();
    // 0 K : une sentinelle, pas une mesure.
    let wmi = Simulee::new("wmi_cimv2_thermal_zone_information", Ok(vec![0.0, 0.0]));
    let acpi = Simulee::new(
        "wmi_root_wmi_msacpi_thermal_zone",
        Err(EchecDeSource::AccesRefuse(
            "IEnumWbemClassObject::Next : HRESULT 0x80041003".into(),
        )),
    );

    let (temp, essais) = premiere_source_lisible(&[&pdh, &pdh_k, &wmi, &acpi]);

    assert_eq!(temp, None);
    let motifs: Vec<(&str, &str)> = essais
        .iter()
        .map(|e| (e.source, e.resultat.as_ref().unwrap_err().id()))
        .collect();
    assert_eq!(
        motifs,
        [
            ("pdh_high_precision_temperature", "source_absente"),
            ("pdh_temperature", "aucune_instance"),
            ("wmi_cimv2_thermal_zone_information", "valeurs_hors_bornes"),
            ("wmi_root_wmi_msacpi_thermal_zone", "acces_refuse"),
        ]
    );
    assert_eq!(
        essais[2].resultat.as_ref().unwrap_err().detail(),
        "2 valeur(s) hors de 5 à 125 °C"
    );
    assert!(
        essais[3]
            .resultat
            .as_ref()
            .unwrap_err()
            .detail()
            .contains("0x80041003"),
        "le code de l'API reste lisible au journal"
    );

    let lecture = lecture_de(temp);
    assert_eq!(
        lecture,
        LectureThermique::Illisible(MotifIllisible::AucunCapteur)
    );
    assert_eq!(largeur_selon_la_chaleur(4, &lecture), 4);
    assert_eq!(
        ThermalGate::new().decide(temp),
        Verdict::Go {
            temp_c: None,
            leaving: false
        }
    );
}

/// Une source lue au-dessus du seuil suspend les analyses : le repli ne
/// fait pas taire la garde chaude.
#[test]
fn une_source_de_repli_lue_trop_chaude_suspend_les_analyses_6450() {
    let pdh = Simulee::new("pdh_high_precision_temperature", Err(absente("0xC0000BB8")));
    // 3632 dixièmes de kelvin = 90,05 °C.
    let wmi = Simulee::new("wmi_cimv2_thermal_zone_information", Ok(vec![3_632.0]));
    let (temp, _) = premiere_source_lisible(&[&pdh, &wmi]);
    assert!(matches!(
        ThermalGate::new().decide(temp),
        Verdict::Hold { entering: true, .. }
    ));
}

/// Le journal parle au premier constat, puis seulement quand il change :
/// une température qui varie ne le fait pas reparler, une source qui se met
/// à échouer autrement, si.
#[test]
fn le_journal_ne_parle_qu_au_changement_de_constat_6450() {
    let essai = |source, resultat| Essai { source, resultat };
    let premier = [
        essai("pdh_high_precision_temperature", Err(absente("0xC0000BB8"))),
        essai("wmi_cimv2_thermal_zone_information", Ok(41.0)),
    ];
    let mut dernier = None;
    assert!(
        journaliser_si_change(&premier, &mut dernier),
        "premier constat"
    );
    assert!(
        !journaliser_si_change(&premier, &mut dernier),
        "même constat"
    );

    let plus_chaud = [
        essai("pdh_high_precision_temperature", Err(absente("0xC0000BB8"))),
        essai("wmi_cimv2_thermal_zone_information", Ok(47.5)),
    ];
    assert!(
        !journaliser_si_change(&plus_chaud, &mut dernier),
        "une autre valeur, même constat : pas de nouvelle ligne"
    );

    let refus = [
        essai("pdh_high_precision_temperature", Err(absente("0xC0000BB8"))),
        essai(
            "wmi_cimv2_thermal_zone_information",
            Err(EchecDeSource::AccesRefuse(
                "ExecQuery : HRESULT 0x80041003".into(),
            )),
        ),
    ];
    assert!(journaliser_si_change(&refus, &mut dernier), "nouveau motif");
}

#[test]
fn les_sources_ne_sont_relues_qu_apres_l_intervalle_meme_illisibles_6450() {
    let debut = std::time::Instant::now();
    let lectures = std::cell::Cell::new(0u32);
    let lire = || {
        lectures.set(lectures.get() + 1);
        None
    };
    let mut derniere = None;

    assert_eq!(
        relire_au_besoin(&mut derniere, debut, RELECTURE_DES_SOURCES, lire),
        None
    );
    let peu_apres = debut + std::time::Duration::from_secs(1);
    assert_eq!(
        relire_au_besoin(&mut derniere, peu_apres, RELECTURE_DES_SOURCES, lire),
        None
    );
    assert_eq!(
        lectures.get(),
        1,
        "aucune source lisible : la seconde requête, une seconde après, ne doit pas relancer \
         les quatre sources (connexions WMI comprises)"
    );

    let plus_tard = debut + RELECTURE_DES_SOURCES;
    relire_au_besoin(&mut derniere, plus_tard, RELECTURE_DES_SOURCES, || {
        lectures.set(lectures.get() + 1);
        Some(52.0)
    });
    assert_eq!(
        lectures.get(),
        2,
        "l'intervalle passé, les sources sont relues"
    );
    assert_eq!(
        relire_au_besoin(&mut derniere, plus_tard, RELECTURE_DES_SOURCES, lire),
        Some(52.0),
        "la valeur relue est gardée"
    );
}
