use super::cap_output_bit_depth;

#[test]
fn le_32_bits_est_ramene_a_24() {
    // Le cas de Jean Valjean : un FLAC annonce en 32 bits.
    assert_eq!(cap_output_bit_depth(32), 24);
}

#[test]
fn les_profondeurs_courantes_passent_intactes() {
    // Ne rien changer pour ceux que ca marchait deja.
    assert_eq!(cap_output_bit_depth(16), 16);
    assert_eq!(cap_output_bit_depth(24), 24);
}

#[test]
fn en_dessous_de_16_on_remonte() {
    // Plancher : sous 16 bits, plus rien ne lit le PCM de facon fiable.
    assert_eq!(cap_output_bit_depth(8), 16);
    assert_eq!(cap_output_bit_depth(1), 16);
    assert_eq!(cap_output_bit_depth(0), 16);
}

#[test]
fn une_valeur_aberrante_reste_jouable() {
    // Une metadonnee fantaisiste ne doit pas produire un flux injouable.
    assert_eq!(cap_output_bit_depth(64), 24);
    assert_eq!(cap_output_bit_depth(u16::MAX), 24);
}
