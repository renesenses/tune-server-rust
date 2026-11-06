//! Lecture d'un profil d'égaliseur OPRA — #6385.
//!
//! [OPRA](https://github.com/opra-project/OPRA) est l'annuaire ouvert de
//! corrections de casques que Roon publie et consomme. Une correction y est un
//! objet JSON décrit par le schéma `schemas/eq_info.json` du dépôt :
//!
//! ```json
//! { "author": "oratory1990", "type": "parametric_eq",
//!   "parameters": { "gain_db": -9.3,
//!     "bands": [ { "type": "peak_dip", "frequency": 20, "gain_db": 4, "q": 1.1 } ] } }
//! ```
//!
//! Le module accepte aussi une ligne de la base `database_v1.jsonl`
//! (`{"type":"eq","id":…,"data":{…}}`), qui enveloppe le même objet.
//!
//! | OPRA | Tune ([`EqBandSpec`]) |
//! |---|---|
//! | `peak_dip` | `peak` |
//! | `low_shelf` | `low_shelf` |
//! | `high_shelf` | `high_shelf` |
//! | `low_pass` (pente 12 dB/oct) | `low_pass` |
//! | `high_pass` (pente 12 dB/oct) | `high_pass` |
//! | `band_stop` | `notch` |
//!
//! `band_pass` est refusé, comme le `BP` d'Equalizer APO : `audio::eq` ne le
//! construit pas. Un passe-haut ou passe-bas d'une autre pente que 12 dB/oct
//! l'est aussi : un biquad n'en fait pas d'autre, et une pente approchée
//! serait une autre correction. Les règles de [`super::autoeq`] valent ici :
//! refuser plutôt que rogner, et ne pas appliquer le gain global (`gain_db`),
//! rendu comme le `Preamp` d'AutoEq.

use serde_json::Value;

use super::autoeq::{ErreurAutoEq, FREQ_MAX_HZ, GAIN_MAX_DB, ProfilAutoEq, Q_MAX, Q_MIN};
use super::eq::EqBandSpec;

/// Analyse un profil OPRA (`info.json`, ou une ligne `eq` de la base JSONL).
pub fn analyser(texte: &str) -> Result<ProfilAutoEq, ErreurAutoEq> {
    let racine: Value = serde_json::from_str(texte).map_err(|e| ErreurAutoEq::Opra {
        bande: None,
        cle: "invalid_json",
        detail: format!("JSON illisible ({e})"),
    })?;
    // Une ligne de `database_v1.jsonl` enveloppe l'objet dans `data`.
    let eq = match racine.get("data") {
        Some(data) if racine.get("type").and_then(Value::as_str) == Some("eq") => data,
        _ => &racine,
    };

    if let Some(genre) = eq.get("type").and_then(Value::as_str)
        && genre != "parametric_eq"
    {
        return Err(ErreurAutoEq::Opra {
            bande: None,
            cle: "unsupported_eq_type",
            detail: format!(
                "type d'égaliseur « {genre} » non pris en charge (attendu parametric_eq)"
            ),
        });
    }

    let parametres = eq
        .get("parameters")
        .ok_or_else(|| champ_absent(None, "parameters"))?;
    let preamp_db = match parametres.get("gain_db") {
        None | Some(Value::Null) => 0.0,
        Some(v) => borne(
            nombre(v, None, "gain_db")?,
            None,
            "gain_db",
            -GAIN_MAX_DB,
            GAIN_MAX_DB,
        )?,
    };
    let bandes_brutes = parametres
        .get("bands")
        .and_then(Value::as_array)
        .ok_or_else(|| champ_absent(None, "bands"))?;

    let mut bandes = Vec::with_capacity(bandes_brutes.len());
    for (index, brute) in bandes_brutes.iter().enumerate() {
        bandes.push(analyser_bande(brute, index + 1)?);
    }
    if bandes.is_empty() {
        return Err(ErreurAutoEq::AucuneBande);
    }

    Ok(ProfilAutoEq {
        preamp_db,
        bandes,
        // OPRA n'a pas de filtre désactivé : rien à écarter.
        ignores: Vec::new(),
    })
}

fn analyser_bande(brute: &Value, bande: usize) -> Result<EqBandSpec, ErreurAutoEq> {
    let b = Some(bande);
    let type_brut = brute
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| champ_absent(b, "type"))?;
    let band_type = match type_brut {
        "peak_dip" => "peak",
        "low_shelf" => "low_shelf",
        "high_shelf" => "high_shelf",
        "low_pass" => "low_pass",
        "high_pass" => "high_pass",
        "band_stop" => "notch",
        autre => {
            return Err(ErreurAutoEq::Opra {
                bande: b,
                cle: "unknown_filter_type",
                detail: format!(
                    "type de filtre « {autre} » inconnu (attendu peak_dip, low_shelf, high_shelf, low_pass, high_pass ou band_stop)"
                ),
            });
        }
    };

    let freq = brute
        .get("frequency")
        .ok_or_else(|| champ_absent(b, "frequency"))?;
    let freq = borne(
        nombre(freq, b, "frequency")?,
        b,
        "frequency",
        1.0,
        FREQ_MAX_HZ,
    )?;

    let passe = matches!(band_type, "low_pass" | "high_pass");
    let gain = match brute.get("gain_db") {
        None | Some(Value::Null) => 0.0,
        Some(v) => borne(
            nombre(v, b, "gain_db")?,
            b,
            "gain_db",
            -GAIN_MAX_DB,
            GAIN_MAX_DB,
        )?,
    };

    if passe {
        // Le schéma fixe 12 dB/oct par défaut : c'est la pente d'un biquad.
        let pente = match brute.get("slope") {
            None | Some(Value::Null) => 12.0,
            Some(v) => nombre(v, b, "slope")?,
        };
        if pente != 12.0 {
            return Err(ErreurAutoEq::Opra {
                bande: b,
                cle: "unsupported_slope",
                detail: format!(
                    "pente de {pente} dB/oct non prise en charge (12 dB/oct seulement)"
                ),
            });
        }
    }

    // Le schéma exige `q` hors des passes ; pour un passe, le biquad de pente
    // 12 dB/oct sans résonance est le Butterworth (Q = 1/√2).
    let q = match brute.get("q") {
        None | Some(Value::Null) if passe => std::f64::consts::FRAC_1_SQRT_2,
        None | Some(Value::Null) => return Err(champ_absent(b, "q")),
        Some(v) => borne(nombre(v, b, "q")?, b, "q", Q_MIN, Q_MAX)?,
    };

    Ok(EqBandSpec {
        freq,
        gain,
        q,
        band_type: band_type.to_string(),
        channel: None,
    })
}

fn champ_absent(bande: Option<usize>, champ: &str) -> ErreurAutoEq {
    ErreurAutoEq::Opra {
        bande,
        cle: "missing_field",
        detail: format!("champ « {champ} » absent"),
    }
}

fn nombre(v: &Value, bande: Option<usize>, champ: &str) -> Result<f64, ErreurAutoEq> {
    v.as_f64()
        .filter(|x| x.is_finite())
        .ok_or_else(|| ErreurAutoEq::Opra {
            bande,
            cle: "invalid_number",
            detail: format!("« {champ} » vaut {v}, qui n'est pas un nombre"),
        })
}

fn borne(
    valeur: f64,
    bande: Option<usize>,
    champ: &str,
    bas: f64,
    haut: f64,
) -> Result<f64, ErreurAutoEq> {
    if valeur < bas || valeur > haut {
        return Err(ErreurAutoEq::Opra {
            bande,
            cle: "value_out_of_range",
            detail: format!(
                "« {champ} » vaut {valeur}, hors du domaine reproductible [{bas} ; {haut}]"
            ),
        });
    }
    Ok(valeur)
}
