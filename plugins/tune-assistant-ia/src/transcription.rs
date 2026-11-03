//! La transcription vocale (Whisper) par le COFFRE et dans le PLAFOND
//! (décision de Bertrand du 23/10/2026).
//!
//! `POST /api/v1/voice-search` transcrivait l'audio par Whisper avec la clé
//! de la variable d'environnement `TUNE_OPENAI_API_KEY` : en clair, hors du
//! coffre, et hors du plafond mensuel de l'assistant. Désormais :
//!
//! 1. la clé vient du **coffre** : une clé de transcription dédiée, scellée
//!    sous [`CLE_TRANSCRIPTION_CHIFFREE`] (son nom porte `api_key`, que
//!    [`tune_core::secrets::est_secret`] retire de `/system/config`, des
//!    exports et de la sauvegarde), ou, à défaut, la clé principale de
//!    l'assistant quand son fournisseur EST OpenAI (compatible OpenAI sur
//!    `api.openai.com`) ;
//! 2. **repli rétro-compatible** : sans clé au coffre, la variable
//!    `TUNE_OPENAI_API_KEY` sert encore, pour ne casser aucune installation
//!    qui la pose — mais l'appel est compté et plafonné de même, et l'écran
//!    dit d'où vient la clé (`source: "environnement"`) ;
//! 3. chaque transcription est **comptée** à l'usage [`USAGE_TRANSCRIPTION`],
//!    au tarif daté [`PRIX_USD_PAR_MINUTE`], et **refusée avant l'envoi** si
//!    son coût — connu d'avance, il ne dépend que de la durée de l'audio —
//!    ferait franchir le plafond.
//!
//! Aucune ligne de ce module ne journalise l'audio, la clé ni le texte
//! transcrit : la durée, le coût et la source de la clé seulement.

use std::sync::Arc;

use serde_json::{Value, json};
use tracing::{info, warn};
use tune_core::db::backend::DbBackend;
use tune_core::db::settings_repo::SettingsRepo;

use crate::appel::ErreurAppel;
use crate::coffre::{Coffre, fin_de_cle};
use crate::couts::Releve;
use crate::fournisseur::{self, ErreurFournisseur, Famille};
use crate::reglages::{self, EtatCle, Reglages};

/// L'usage compté.
pub const USAGE_TRANSCRIPTION: &str = "transcription";
/// La clé de transcription scellée. Le nom DOIT porter `api_key`.
pub const CLE_TRANSCRIPTION_CHIFFREE: &str = "assistant_ia_transcription_api_key_chiffree";
/// L'adresse du service de transcription (compatible OpenAI). Vide : OpenAI.
pub const CLE_TRANSCRIPTION_URL: &str = "assistant_ia_transcription_url_base";
/// L'adresse par défaut.
pub const URL_PAR_DEFAUT: &str = "https://api.openai.com/v1";
/// Le modèle de transcription.
pub const MODELE_TRANSCRIPTION: &str = "whisper-1";
/// La variable d'environnement héritée.
pub const VARIABLE_HERITEE: &str = "TUNE_OPENAI_API_KEY";

/// Le prix de Whisper, en dollars par minute d'audio : « Whisper — $0.006 /
/// minute » (tableau « Transcription models »), **relevé le 23/10/2026** sur
/// <https://developers.openai.com/api/docs/pricing>. La page ne porte pas de
/// date.
pub const PRIX_USD_PAR_MINUTE: f64 = 0.006;
/// Le relevé de [`PRIX_USD_PAR_MINUTE`].
pub const RELEVE_TRANSCRIPTION: Releve = Releve {
    releve_le: "2026-10-23",
    source_url: Some("https://developers.openai.com/api/docs/pricing"),
    a_relever_avant: None,
};

/// La taille maximale acceptée par l'API de transcription d'OpenAI (25 Mo).
pub const AUDIO_MAX_OCTETS: usize = 25 * 1024 * 1024;

/// Octets par seconde supposés quand l'en-tête WAV est illisible : 8 000,
/// soit l'audio le plus compact que l'on reçoive (8 kHz, 8 bits, mono). La
/// durée — donc le coût — est SURESTIMÉE pour tout format plus riche, jamais
/// sous-estimée.
pub const OCTETS_PAR_SECONDE_PRUDENTS: u64 = 8_000;

/// D'où vient la clé de transcription.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceCle {
    /// La clé de transcription, au coffre.
    Coffre,
    /// La clé principale de l'assistant (fournisseur OpenAI), au coffre.
    ClePrincipale,
    /// `TUNE_OPENAI_API_KEY` (repli rétro-compatible).
    Environnement,
}

impl SourceCle {
    pub fn nom(self) -> &'static str {
        match self {
            Self::Coffre => "coffre",
            Self::ClePrincipale => "cle_principale",
            Self::Environnement => "environnement",
        }
    }
}

/// La durée d'un audio, en secondes : lue dans l'en-tête WAV (`byte rate`
/// du bloc `fmt `, taille du bloc `data`), sinon estimée PRUDEMMENT
/// ([`OCTETS_PAR_SECONDE_PRUDENTS`]).
pub fn duree_secondes(audio: &[u8]) -> f64 {
    duree_wav(audio).unwrap_or(audio.len() as f64 / OCTETS_PAR_SECONDE_PRUDENTS as f64)
}

fn duree_wav(a: &[u8]) -> Option<f64> {
    if a.len() < 12 || &a[0..4] != b"RIFF" || &a[8..12] != b"WAVE" {
        return None;
    }
    let mut i = 12;
    let mut octets_par_seconde = None;
    while i + 8 <= a.len() {
        let id = &a[i..i + 4];
        let taille = u32::from_le_bytes(a[i + 4..i + 8].try_into().ok()?) as usize;
        let debut = i + 8;
        if id == b"fmt " && debut + 12 <= a.len() {
            let bps = u32::from_le_bytes(a[debut + 8..debut + 12].try_into().ok()?);
            octets_par_seconde = Some(bps).filter(|b| *b > 0);
        }
        if id == b"data" {
            // Un bloc `data` à taille inconnue (0 ou au-delà du fichier, cas
            // d'un enregistrement en flux) : ce qui reste du fichier.
            let reste = a.len().saturating_sub(debut);
            let n = if taille == 0 || taille > reste {
                reste
            } else {
                taille
            };
            return Some(n as f64 / f64::from(octets_par_seconde?));
        }
        i = debut.checked_add(taille)?.checked_add(taille % 2)?;
    }
    None
}

/// Le coût d'une transcription de `secondes`, en micro-euros, arrondi au
/// micro-euro supérieur (au moins un : rien n'est gratuit par arrondi).
pub fn cout_micro_eur(secondes: f64, taux_usd_eur: f64) -> u64 {
    let usd = secondes.max(0.0) / 60.0 * PRIX_USD_PAR_MINUTE;
    let micro = usd * taux_usd_eur * 1_000_000.0;
    (micro.ceil() as u64).max(1)
}

fn lire(s: &SettingsRepo, cle: &str) -> String {
    s.get(cle).ok().flatten().unwrap_or_default()
}

/// La clé principale de l'assistant peut-elle transcrire ? Seulement si son
/// fournisseur est OpenAI lui-même (un point d'accès « compatible » Mistral
/// ou autre ne connaît pas Whisper).
fn cle_principale_est_openai(r: &Reglages) -> bool {
    r.famille == Some(Famille::CompatibleOpenai)
        && (r.url_base.trim().is_empty() || r.url_base.contains("api.openai.com"))
}

/// La clé, l'adresse et la source de la transcription, ou pourquoi elle ne
/// peut pas partir. `cle_env` : la valeur de [`VARIABLE_HERITEE`], s'il y en a.
pub fn acces(
    db: &Arc<dyn DbBackend>,
    coffre: &Coffre,
    cle_env: Option<&str>,
) -> Result<(String, String, SourceCle), ErreurAppel> {
    let s = SettingsRepo::with_backend(db.clone());
    let url = {
        let u = lire(&s, CLE_TRANSCRIPTION_URL);
        let u = u.trim().trim_end_matches('/');
        if u.is_empty() {
            URL_PAR_DEFAUT.to_string()
        } else {
            u.to_string()
        }
    };
    let scelle = lire(&s, CLE_TRANSCRIPTION_CHIFFREE);
    if !scelle.trim().is_empty() {
        return match coffre.ouvrir(&scelle) {
            Ok(k) => Ok((k, url, SourceCle::Coffre)),
            Err(_) => Err(ErreurAppel::CleIllisible),
        };
    }
    let r = Reglages::charger(db);
    if cle_principale_est_openai(&r) {
        match reglages::cle_en_clair(db, coffre) {
            EtatCle::Ouverte(k) => {
                let base = if r.url_base.trim().is_empty() {
                    url
                } else {
                    r.url_base.trim().trim_end_matches('/').to_string()
                };
                return Ok((k, base, SourceCle::ClePrincipale));
            }
            EtatCle::Illisible => return Err(ErreurAppel::CleIllisible),
            EtatCle::Absente => {}
        }
    }
    match cle_env.map(str::trim).filter(|k| !k.is_empty()) {
        Some(k) => Ok((k.to_string(), url, SourceCle::Environnement)),
        None => Err(ErreurAppel::NonConfigure),
    }
}

/// Le corps `multipart/form-data` de la requête (reqwest est compilé sans sa
/// fonction `multipart`) : le fichier, puis le modèle.
pub fn corps_multipart(audio: &[u8], frontiere: &str) -> Vec<u8> {
    let mut b = Vec::with_capacity(audio.len() + 256);
    b.extend_from_slice(
        format!(
            "--{frontiere}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"audio.wav\"\r\nContent-Type: audio/wav\r\n\r\n"
        )
        .as_bytes(),
    );
    b.extend_from_slice(audio);
    b.extend_from_slice(
        format!(
            "\r\n--{frontiere}\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\n{MODELE_TRANSCRIPTION}\r\n--{frontiere}--\r\n"
        )
        .as_bytes(),
    );
    b
}

/// Une transcription faite, et ce qu'elle a coûté.
#[derive(Clone, Debug)]
pub struct Transcription {
    pub texte: String,
    pub cout_micro_eur: u64,
    pub secondes: f64,
    pub source: SourceCle,
}

/// Transcrit `audio`. Le plafond est regardé AVANT l'envoi, avec le coût de
/// CETTE transcription (connu d'avance), sous LE verrou de consommation ; le
/// coût est compté après une réponse reçue.
pub async fn transcrire(
    db: &Arc<dyn DbBackend>,
    coffre: &Coffre,
    verrou: &tokio::sync::Mutex<()>,
    audio: &[u8],
    cle_env: Option<&str>,
) -> Result<Transcription, ErreurAppel> {
    let (cle, url, source) = acces(db, coffre, cle_env)?;
    let r = Reglages::charger(db);
    let secondes = duree_secondes(audio);
    let cout = cout_micro_eur(secondes, r.taux_usd_eur);

    let _verrou = verrou.lock().await;
    let mut c = reglages::charger_consommation(db);
    if c.micro_eur.saturating_add(cout) > r.plafond_micro_eur {
        info!(
            usage = USAGE_TRANSCRIPTION,
            cout_estime_micro_eur = cout,
            "assistant_ia_plafond_atteint"
        );
        return Err(ErreurAppel::PlafondAtteint);
    }
    if source == SourceCle::Environnement {
        warn!(
            variable = VARIABLE_HERITEE,
            "assistant_ia_transcription_cle_heritee : saisissez la clé dans Réglages ▸ Assistant IA"
        );
    }
    let frontiere = "----TuneTranscription";
    let famille = Famille::CompatibleOpenai;
    let rep = fournisseur::client()
        .post(format!("{url}/audio/transcriptions"))
        .bearer_auth(&cle)
        .header(
            "Content-Type",
            format!("multipart/form-data; boundary={frontiere}"),
        )
        .timeout(fournisseur::DELAI)
        .body(corps_multipart(audio, frontiere))
        .send()
        .await
        .map_err(|e| ErreurAppel::Fournisseur(fournisseur::erreur_reseau(famille, &e)))?;
    let statut = rep.status().as_u16();
    let texte = rep
        .text()
        .await
        .map_err(|e| ErreurAppel::Fournisseur(fournisseur::erreur_reseau(famille, &e)))?;
    if !(200..300).contains(&statut) {
        return Err(ErreurAppel::Fournisseur(fournisseur::erreur_de_statut(
            famille, statut, &texte,
        )));
    }
    let v: Value = serde_json::from_str(&texte)
        .map_err(|_| ErreurAppel::Fournisseur(ErreurFournisseur::corps_illisible(statut)))?;
    // L'audio a été envoyé et traité : il est compté, même si le texte est vide.
    c.ajouter_appel(USAGE_TRANSCRIPTION, cout, 0, 0);
    let _ = reglages::ranger_consommation(db, &c);
    info!(
        usage = USAGE_TRANSCRIPTION,
        source = source.nom(),
        secondes = format!("{secondes:.1}"),
        cout_micro_eur = cout,
        "assistant_ia_appel"
    );
    Ok(Transcription {
        texte: v
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        cout_micro_eur: cout,
        secondes,
        source,
    })
}

/// Pose ou efface (`""`) la clé de transcription, scellée.
pub fn poser_cle(db: &Arc<dyn DbBackend>, coffre: &Coffre, cle: &str) -> Result<(), String> {
    let s = SettingsRepo::with_backend(db.clone());
    let cle = cle.trim();
    if cle.is_empty() {
        s.delete(CLE_TRANSCRIPTION_CHIFFREE)
    } else {
        s.set(CLE_TRANSCRIPTION_CHIFFREE, &coffre.sceller(cle)?)
    }
}

/// Ce que l'écran voit de la transcription : d'où viendrait la clé, ses
/// quatre derniers caractères (jamais la clé), et le tarif daté.
pub fn vue(db: &Arc<dyn DbBackend>, coffre: &Coffre, cle_env: Option<&str>) -> Value {
    let s = SettingsRepo::with_backend(db.clone());
    let dediee = !lire(&s, CLE_TRANSCRIPTION_CHIFFREE).trim().is_empty();
    let (source, fin, illisible) = match acces(db, coffre, cle_env) {
        Ok((k, _, src)) => (Some(src.nom()), Some(fin_de_cle(&k)), false),
        Err(ErreurAppel::CleIllisible) => (None, None, true),
        Err(_) => (None, None, false),
    };
    json!({
        "cle_definie": dediee,
        "cle_illisible": illisible,
        "source": source,
        "cle_fin": fin,
        "modele": MODELE_TRANSCRIPTION,
        "prix_usd_minute": PRIX_USD_PAR_MINUTE,
        "releve_le": RELEVE_TRANSCRIPTION.releve_le,
        "source_url": RELEVE_TRANSCRIPTION.source_url,
        "variable_heritee": VARIABLE_HERITEE,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wav(octets_par_seconde: u32, donnees: usize) -> Vec<u8> {
        let mut a = Vec::new();
        a.extend_from_slice(b"RIFF");
        a.extend_from_slice(&((36 + donnees) as u32).to_le_bytes());
        a.extend_from_slice(b"WAVE");
        a.extend_from_slice(b"fmt ");
        a.extend_from_slice(&16u32.to_le_bytes());
        a.extend_from_slice(&1u16.to_le_bytes()); // PCM
        a.extend_from_slice(&1u16.to_le_bytes()); // mono
        a.extend_from_slice(&16_000u32.to_le_bytes());
        a.extend_from_slice(&octets_par_seconde.to_le_bytes());
        a.extend_from_slice(&2u16.to_le_bytes());
        a.extend_from_slice(&16u16.to_le_bytes());
        a.extend_from_slice(b"data");
        a.extend_from_slice(&(donnees as u32).to_le_bytes());
        a.extend(std::iter::repeat_n(0u8, donnees));
        a
    }

    #[test]
    fn la_duree_se_lit_dans_l_en_tete_wav() {
        // 16 kHz, 16 bits, mono : 32 000 octets par seconde ; 3 s.
        let a = wav(32_000, 96_000);
        assert!((duree_secondes(&a) - 3.0).abs() < 1e-9);
    }

    #[test]
    fn un_audio_illisible_est_estime_au_plus_long() {
        let a = vec![7u8; 80_000];
        // 80 000 octets à 8 000 octets/s : 10 s, plus que tout WAV réel.
        assert!((duree_secondes(&a) - 10.0).abs() < 1e-9);
    }

    #[test]
    fn une_minute_coute_ce_que_dit_le_releve() {
        // 0,006 $ la minute, taux 1 : 6 000 micro-euros.
        assert_eq!(cout_micro_eur(60.0, 1.0), 6_000);
        assert_eq!(
            cout_micro_eur(0.0, 1.0),
            1,
            "rien n'est gratuit par arrondi"
        );
    }

    #[test]
    fn le_nom_de_la_cle_est_un_secret() {
        assert!(tune_core::secrets::est_secret(CLE_TRANSCRIPTION_CHIFFREE));
    }
}
