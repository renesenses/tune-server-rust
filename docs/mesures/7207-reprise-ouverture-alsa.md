# Reprendre l'ouverture après un changement de fréquence

Source : [rapport automatique de cpadiou77, rc8.1 sous Linux](https://github.com/renesenses/tune-server-rust/issues/7207#issuecomment-6463009529), ajouté le 28 octobre 2026. Lecture des journaux avant modification.

## Ce que les journaux établissent

Le 27 octobre, zone 4, sortie locale « Zone 7 », endpoint `alsa:hw:CARD=1,DEV=0` :

| Heure locale | Événement | Conséquence |
| --- | --- | --- |
| 11:27:04.015–016 | Repli `i32` à 44 100 Hz, puis `local_audio_playing_after_prefill` | La lecture fonctionne. |
| 11:27:39.490 | `local_audio_stopped` | La piste précédente est arrêtée. |
| 11:27:39.548–557 | En-tête 176 400 Hz / 24 bits, fréquence annoncée supportée | Le nouveau flux est identifié ; la décision de cadence conserve la source. |
| 11:27:39.566 | `audio_stream_build_failed_all_formats` | L'ouverture échoue avant le préchargement et le démarrage. Les deux erreurs publiées concernent seulement `f32`. |
| 11:27:39.942–943 | `output_reported_failure_stopping_zone`, suppression de session | Le sondeur arrête la zone sur l'échec remonté par la sortie. |
| 11:27:42.552–628 | Nouvelle demande de la même piste, mêmes fréquence/profondeur/canaux ; ouverture `i32`, puis préchargement réussi | Une relance environ trois secondes plus tard suffit. |

Un autre échec à 44 100 Hz à 11:26:58 est suivi d'une ouverture réussie à 11:27:04. Le problème ne se limite donc pas à une fréquence réellement non supportée. La disparition antérieure du périphérique à 11:10 et la recréation de la sortie à 11:26 sont visibles, mais ne prouvent pas la cause du refus à 11:27:39.

**Cause logicielle de l'arrêt établie :** une seule cascade d'ouverture est tentée ; son échec devient immédiatement une panne définitive de zone. La relance manuelle recommence cette ouverture et réussit. Ce n'est pas un arrêt déclenché par le décodage ni par un manque d'échantillons dans l'anneau.

**Limite du diagnostic :** les erreurs `i32`/`i16` étaient jetées. Les journaux ne permettent pas d'affirmer si le pilote refusait les paramètres, restait momentanément occupé ou subissait une autre indisponibilité. Le correctif ne prétend pas résoudre une cause matérielle identifiée.

## Changement

Après les refus flottants, la cascade entière `i32` puis `i16` peut être reprise sur un endpoint matériel ALSA. Elle conserve le même objet périphérique et les configurations déjà calculées. Cinq attentes (100, 200, 400, 800, 1 500 ms), soit trois secondes de sommeil au maximum, séparent six tentatives au plus. Les appels au pilote eux-mêmes ne sont pas soumis à ce budget.

Pendant cette attente, la session et le bloc PCM initial restent dans la boucle de lecture. Aucun flux réussi n'est recommencé ; le préchargement et le démarrage ordinaires suivent la première ouverture réussie. Un arrêt ou remplacement de piste interrompt l'attente sans publier de panne. Un refus persistant conserve la remontée d'erreur existante.

Les erreurs entières sont désormais journalisées avec leur format et leur fréquence. Le refus final conserve aussi la dernière erreur entière au lieu de ne montrer que les deux essais flottants.

Aucun changement de convertisseur, de volume, de format candidat ou de règle bit-perfect. Le refus strict d'un rééchantillonnage reste placé avant cette cascade. Aucun autre périphérique n'est recherché pendant la reprise.

## Essais et limites

Les tests `open_retry_7207` utilisent une sortie mémoire et une horloge factice. Ils exercent la fonction de reprise appelée par la cascade réelle : 44,1 → 96 kHz, changements successifs de fréquence, mots d'un porteur DoP à 176,4 kHz, refus permanent, annulation, et périmètre ALSA matériel. Aucun périphérique sonore n'est ouvert.

Ils prouvent la reprise bornée et la conservation des données confiées à la sortie factice. Ils ne reproduisent pas le refus interne du pilote du demandeur et ne constituent pas une mesure bit-perfect sur matériel. La vérification sur son appareil reste à faire après intégration.

Les chemins natifs exclusifs, les sorties réseau, les PCM virtuels et les indisponibilités de plus de trois secondes ne sont pas modifiés.

## Preuves de la réalisation

- `cargo test -p tune-core --lib open_retry_7207` : 8 réussites.
- `cargo test -p tune-core --lib gapless_changement_de_cadence_4953` : 11 réussites.
- `cargo test -p tune-core --lib bitperfect_strict_3973` : 7 réussites.
- `cargo clippy -p tune-core --lib -- -D clippy::correctness` : succès (avertissements présents dans la caisse).
- Formatage contrôlé avec `rustfmt --edition 2024 --config skip_children=true --check` sur les trois fichiers Rust modifiés uniquement.

Contre-épreuve : reprises désactivées dans `retry_open`, test inchangé. La commande `cargo test -p tune-core --lib sample_rate_44100_to_96000_resumes_automatically_7207` compile et échoue sur ce test : `le changement de fréquence doit reprendre seul, sans relance manuelle: "hardware temporarily unavailable"`. Restauration par copie de la sauvegarde, puis retour au vert des huit tests de reprise.
