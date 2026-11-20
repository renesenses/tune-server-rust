//! #9015 — la sortie d'erreur du démon AirPlay 2 rejoint le journal de Tune.
//!
//! Le démon y écrit ce qui explique un échec : la route d'appairage qui a
//! échoué et pourquoi (`connect: transient failed: …`), un TEARDOWN sans
//! réponse. Elle était jetée (`Stdio::null()`) : au fil 2893, une zone
//! arrêtée après 30 s d'attente de `connected` n'a laissé aucune trace de ce
//! que faisait le démon.
//!
//! Trois garanties :
//! - **rien de secret** : chaque ligne passe par
//!   [`crate::confidentialite::masquer_acces_distant`] (jetons, `Bearer`,
//!   JWT, URL avec identifiants), puis par un masque des valeurs nommées
//!   code PIN, mot de passe, clé, secret, jeton, preuve ou sel SRP, et des longues suites hexa ou
//!   base64 (clés d'appairage) ;
//! - **débit borné** : au plus [`LIGNES_PAR_FENETRE`] lignes par
//!   [`FENETRE`] ; au-delà, un seul décompte des lignes tues ;
//! - **le démon n'est jamais bloqué** : le tube est vidé en continu par une
//!   tâche asynchrone, qu'une ligne soit journalisée ou non, et une ligne
//!   trop longue est coupée à [`LONGUEUR_MAX`] octets sans être gardée en
//!   mémoire au-delà.

use std::sync::LazyLock;
use std::time::{Duration, Instant};

use regex::Regex;
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};

/// Le préfixe de chaque ligne relayée dans le journal de Tune.
pub(super) const PREFIXE: &str = "airplay-daemon stderr";
/// Lignes relayées au plus par fenêtre.
pub(super) const LIGNES_PAR_FENETRE: u32 = 20;
pub(super) const FENETRE: Duration = Duration::from_secs(10);
/// Au-delà, la ligne est coupée.
pub(super) const LONGUEUR_MAX: usize = 1024;

const MASQUE: &str = "********";

/// `pin=1234`, `password: x`, `secret_key=…`, `token "…"`.
static RE_VALEUR_NOMMEE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?i)\b([a-z_\-]*(?:pin|pass(?:word|wd)?|secret|token|key|psk|srp|proof|salt|signature|identity)[a-z_\-]*)(["']?\s*[:=]\s*["']?)[^\s,;"'}\]]+"#,
    )
    .expect("motif valide")
});

/// Une suite hexa de 32 caractères ou plus, ou base64 de 40 ou plus : une
/// clé, une preuve SRP, un sel — jamais un mot de diagnostic.
static RE_LONGUE_SUITE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b[0-9A-Fa-f]{32,}\b|[A-Za-z0-9+/_\-]{40,}={0,2}").expect("motif valide")
});

/// La ligne telle qu'elle peut entrer dans le journal, ou `None` si elle est
/// vide.
pub(super) fn nettoyer_ligne(brute: &str) -> Option<String> {
    let ligne = brute.trim();
    if ligne.is_empty() {
        return None;
    }
    let ligne = crate::confidentialite::masquer_acces_distant(ligne);
    let ligne = crate::confidentialite::masquer_url(&ligne);
    let ligne = RE_VALEUR_NOMMEE.replace_all(&ligne, format!("${{1}}${{2}}{MASQUE}"));
    let ligne = RE_LONGUE_SUITE.replace_all(&ligne, MASQUE);
    Some(ligne.into_owned())
}

/// Le budget de lignes d'une fenêtre.
pub(super) struct Debit {
    debut: Instant,
    relayees: u32,
    tues: u64,
}

/// Ce qu'il faut écrire pour une ligne qui arrive.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Verdict {
    /// Relayer la ligne ; `tues` lignes de la fenêtre précédente ont été
    /// tues (à dire d'abord, s'il y en a).
    Relayer {
        tues: u64,
    },
    Taire,
}

impl Debit {
    pub(super) fn new(maintenant: Instant) -> Self {
        Self {
            debut: maintenant,
            relayees: 0,
            tues: 0,
        }
    }

    pub(super) fn ligne(&mut self, maintenant: Instant) -> Verdict {
        let mut tues = 0;
        // Une fenêtre neuve rend la parole et dit d'abord combien de lignes
        // la précédente a tues.
        if maintenant.duration_since(self.debut) >= FENETRE {
            tues = std::mem::take(&mut self.tues);
            self.debut = maintenant;
            self.relayees = 0;
        }
        if self.relayees < LIGNES_PAR_FENETRE {
            self.relayees += 1;
            Verdict::Relayer { tues }
        } else {
            self.tues += 1;
            Verdict::Taire
        }
    }

    /// Les lignes tues qui n'ont pas encore été dites (fin du flux).
    pub(super) fn reste(&mut self) -> u64 {
        std::mem::take(&mut self.tues)
    }
}

/// Vide `lecteur` jusqu'à sa fin et confie à `puits` chaque ligne à
/// journaliser, nettoyée, dans le budget.
pub(super) async fn relayer_vers<R: AsyncRead + Unpin>(lecteur: R, mut puits: impl FnMut(String)) {
    let mut lecteur = BufReader::new(lecteur);
    let mut debit = Debit::new(Instant::now());
    let mut ligne: Vec<u8> = Vec::with_capacity(256);
    loop {
        // Une ligne par morceaux : au-delà de `LONGUEUR_MAX`, la suite est
        // lue et jetée, jamais accumulée.
        ligne.clear();
        let mut fin_de_flux = false;
        loop {
            let tampon = match lecteur.fill_buf().await {
                Ok(t) => t,
                Err(_) => {
                    fin_de_flux = true;
                    break;
                }
            };
            if tampon.is_empty() {
                fin_de_flux = true;
                break;
            }
            let (morceau, complete) = match tampon.iter().position(|&o| o == b'\n') {
                Some(i) => (&tampon[..=i], true),
                None => (tampon, false),
            };
            let n = morceau.len();
            let place = LONGUEUR_MAX.saturating_sub(ligne.len());
            ligne.extend_from_slice(&morceau[..n.min(place)]);
            lecteur.consume(n);
            if complete {
                break;
            }
        }
        if !ligne.is_empty() {
            let texte = String::from_utf8_lossy(&ligne);
            if let Some(propre) = nettoyer_ligne(&texte) {
                match debit.ligne(Instant::now()) {
                    Verdict::Relayer { tues } => {
                        if tues > 0 {
                            puits(format!("{tues} lignes tues (débit borné)"));
                        }
                        puits(propre);
                    }
                    Verdict::Taire => {}
                }
            }
        }
        if fin_de_flux {
            break;
        }
    }
    let tues = debit.reste();
    if tues > 0 {
        puits(format!("{tues} lignes tues (débit borné)"));
    }
}

/// Lance la tâche qui relaie la sortie d'erreur du démon de `appareil`.
pub(super) fn relayer<R: AsyncRead + Unpin + Send + 'static>(stderr: R, appareil: String) {
    tokio::spawn(async move {
        relayer_vers(stderr, |ligne| {
            tracing::info!(device = %appareil, "{PREFIXE}: {ligne}");
        })
        .await;
    });
}

#[cfg(test)]
#[path = "stderr_du_demon_9015_tests.rs"]
mod tests;
