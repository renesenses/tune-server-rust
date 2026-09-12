# Banc de mesure audio orienté AES17

Le banc SRC de Tune utilise la méthode versionnée
`tune-aes17-oriented-residual-v1`. AES17-2020 est sa référence normative et
le stimulus est un sinus de 997 Hz à −6,0206 dBFS crête pendant une seconde.

Cette méthode est une régression déterministe, pas une déclaration de
conformité AES17 complète. Elle retire le fondamental par un ajustement exact
`a sin(wt) + b cos(wt)`, analyse les 80 % centraux du signal et mesure le
résidu non pondéré de 0 Hz à Nyquist. Elle ne prétend pas reproduire tous les
filtres d'analyse prescrits par la norme. Cette limite figure aussi dans
l'artefact avec `full_aes17_conformance_claimed: false`.

La contre-épreuve emploie la même fréquence non entière de 997 Hz et injecte
un second harmonique à −60 dB. Elle doit retrouver ce défaut à ±0,05 dB. Ce
cas rougit avec le raccourci historique `2 * dot / N`, qui n'est valable que
pour une fenêtre contenant un nombre entier de périodes.

## Reproduction locale

La cible est unitaire et reste compilée malgré `autotests = false`, car elle
est exécutée explicitement avec `--lib` :

```sh
cargo test -p tune-core --lib \
  audio::resample::tests::residual_meter_recovers_a_known_injected_distortion
cargo test -p tune-core --lib \
  audio::resample::tests::audio_conformance_artifact_is_stable_and_explicit
```

Le second test écrit un JSON stable dans :

```text
${CARGO_TARGET_DIR:-target}/audio-conformance/aes17-oriented-residual-v1.json
```

Le document contient l'identifiant de méthode, la référence, le stimulus, la
bande d'analyse, les seuils et les mesures arrondies au milli-décibel. Il peut
être publié tel quel par une future étape CI sans modifier le protocole de
mesure.

Référence officielle :
<https://www.aes.org/publications/standards/preview.cfm?ID=21>.
