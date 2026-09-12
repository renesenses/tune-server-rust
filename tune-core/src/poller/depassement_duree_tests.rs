use super::decisions::{END_MARGIN_MS, position_au_dela_de_la_duree};

/// Duree reelle du morceau du signalement : 1 min 46 s.
const TURAPIN_MS: u64 = 106_000;
/// Dix minutes : ce que le testeur a vu defiler.
const DIX_MINUTES_MS: u64 = 600_000;

/// La forme du signalement : duree connue, position tres au-dela.
#[test]
fn position_tres_au_dela_de_la_duree_est_constatee() {
    assert!(
        position_au_dela_de_la_duree(false, TURAPIN_MS, DIX_MINUTES_MS),
        "une position de dix minutes sur une piste de 1'46 doit etre constatee"
    );
}

/// La forme que l'ecran MONTRE reellement : la position rendue au client est
/// plafonnee a la duree (voir le `min(dur)` du sondeur), donc le testeur lit
/// « 1:46 / 1:46, en lecture » indefiniment. Le constat doit tenir sur cette
/// forme-la aussi, sinon il ne couvre pas le cas observe.
#[test]
fn position_collee_a_la_duree_est_constatee() {
    assert!(
        position_au_dela_de_la_duree(false, TURAPIN_MS, TURAPIN_MS),
        "position figee EXACTEMENT a la duree : c'est ce que le testeur voit"
    );
}

/// Une radio n'a pas de fin : sa position ne depasse rien, quelle qu'elle
/// soit et quelle que soit la duree que quelqu'un aurait cru y lire.
#[test]
fn une_radio_ne_declenche_rien() {
    assert!(
        !position_au_dela_de_la_duree(true, 0, DIX_MINUTES_MS),
        "radio sans duree : aucun depassement possible"
    );
    assert!(
        !position_au_dela_de_la_duree(true, TURAPIN_MS, DIX_MINUTES_MS),
        "meme si une duree traine dans les metadonnees, une radio reste hors sujet"
    );
    // Contre-epreuve de la garde `source_radio` : memes chiffres, source
    // ordinaire — la, et la seulement, le constat tombe.
    assert!(
        position_au_dela_de_la_duree(false, TURAPIN_MS, DIX_MINUTES_MS),
        "la garde ne doit tenir QUE sur la radio, sinon elle ne prouve rien"
    );
}

/// Duree nulle ou derisoire : il n'y a rien a depasser. Le seuil est le meme
/// que celui de `past_end_reached` — les deux doivent parler de la meme
/// « fin », sans quoi Tune constaterait un depassement la ou son propre
/// detecteur de fin de piste ne voit meme pas de piste.
#[test]
fn une_duree_nulle_ou_absente_ne_declenche_rien() {
    assert!(
        !position_au_dela_de_la_duree(false, 0, DIX_MINUTES_MS),
        "duree inconnue (0) : aucun depassement possible"
    );
    assert!(
        !position_au_dela_de_la_duree(false, END_MARGIN_MS, DIX_MINUTES_MS),
        "duree egale a la marge de fin : sous le seuil de `past_end_reached`"
    );
    // Contre-epreuve : une milliseconde de plus que la marge, et la duree
    // devient exploitable.
    assert!(
        position_au_dela_de_la_duree(false, END_MARGIN_MS + 1, DIX_MINUTES_MS),
        "juste au-dessus de la marge, la duree redevient une duree"
    );
}

/// Une lecture ordinaire en plein milieu de piste n'est jamais constatee —
/// c'est la garde qui empeche ce chemin de parler a tort une fois par
/// seconde sur toutes les zones du parc.
#[test]
fn une_lecture_en_cours_de_piste_ne_declenche_rien() {
    assert!(
        !position_au_dela_de_la_duree(false, TURAPIN_MS, TURAPIN_MS / 2),
        "a mi-piste, rien a signaler"
    );
    // Bord exact : la zone de fin commence a duree - END_MARGIN_MS.
    assert!(
        !position_au_dela_de_la_duree(false, TURAPIN_MS, TURAPIN_MS - END_MARGIN_MS - 1),
        "une milliseconde AVANT la zone de fin : encore une lecture ordinaire"
    );
    assert!(
        position_au_dela_de_la_duree(false, TURAPIN_MS, TURAPIN_MS - END_MARGIN_MS),
        "au bord exact de la zone de fin, le constat est possible"
    );
}
