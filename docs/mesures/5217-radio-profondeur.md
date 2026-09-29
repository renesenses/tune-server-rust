# Radio locale : conservation du PCM 24 bits (#5217)

Bertrand / OpenAI Codex / support-20260927-5217, le 27 septembre 2026.
Base : `f136a8f2003b7a79d33158cff8c60c2e2404683b` (v0.9.166).
Lot : `batch/radio-profondeur-20260927` ; prochaine RC à assigner.
Compilation et exécution sur Shrek, worktree
`/srv/builds/bertrand/worktrees/codex-5217-20260927`, cible Cargo dédiée
`bertrand-codex-5217-20260927`, six jobs.

## Défaut et correction

Le décodeur radio quantifiait systématiquement en i16, y compris une source
FLAC 24 bits adressée à une sortie locale. Le contrôle strict de fréquence
ne détectait pas cette perte de profondeur.

Le résolveur distingue maintenant la sortie locale : une source sans perte
dont la profondeur connue dépasse 16 bits produit du PCM 24 bits. Les radios
16 bits restent en 16 bits ; le contrat réseau OAAT/DLNA reste en 16 bits.
En mode strict, toute réduction connue de profondeur dans ce décodeur est
refusée avant émission du PCM, avec une erreur dédiée exprimée en bits.

La profondeur détectée est publiée avant les données. Le serveur HTTP et les
vumètres utilisent ce format, et le pré-tampon radio compte les secondes au
débit réel. Les morceaux 24 bits se terminent sur une trame entière. Sans
format détecté après l'attente bornée, HTTP termine le flux en erreur au lieu
d'émettre un en-tête provisoire incompatible. Une reconnexion qui changerait
la profondeur de sortie termine la session, comme un changement de cadence.

## Banc et oracle

Le banc appelle le résolveur de production avec une station HTTP locale et
les fixtures FLAC existantes. La référence 24 bits / 96 kHz est vérifiée contre
l'empreinte libFLAC déjà publiée dans `flac_empreintes_reference.rs` :
`5647a1733e4ec46e7a1dd00e10feaf3c` sur les échantillons i32 little-endian.
Le PCM radio doit égaler la référence octet par octet, avec des bits faibles
non nuls. Les contrôles HTTP passent par le véritable handler, pour les deux
variantes WAV live (bornée et streaming).

```sh
cargo test -p tune-core -p tune-stream-http --lib \
  --no-default-features --features oaat --no-fail-fast 5217 \
  -- --nocapture --test-threads=2
```

Résultat corrigé : **7 tests core et 2 tests HTTP réussis**. Les deux témoins
de compatibilité couvrent OAAT 16 bits et les sources locales 16 bits mono et
stéréo. Les autres couvrent profondeur publiée, octets, strict, vumètres,
pré-tampon, profondeur WAV et refus de deviner le format.

## Contre-épreuve exécutée

Les tests sont conservés. Après sauvegarde des sources de production :
forcer la sélection à 16 bits, neutraliser seulement le refus de profondeur,
revenir au format provisoire pour le pré-tampon et l'en-tête HTTP, et rétablir
le repli HTTP sans format connu. La commande ci-dessus **compile**, puis
retourne 101 : **7 échecs attendus, 2 compatibilités réussies**.

| Témoin (suffixe `_5217`) | Message de l'échec |
|---|---|
| `radio_locale_publie_24_bits` | la radio locale 24 bits est annoncée en 16 bits |
| `radio_locale_conserve_les_octets_et_les_bits_faibles` | le décodeur radio a perdu les bits faibles du FLAC 24 bits |
| `radio_stricte_refuse_la_reduction_de_profondeur` | Bit-perfect strict laisse passer la réduction 24 vers 16 bits |
| `radio_locale_vumetres_observent_le_pcm_24_bits` | les vumètres interprètent le PCM 24 bits comme du 16 bits |
| `radio_prefill_compte_les_secondes_au_format_24_bits` | six blocs 24 bits ne sont pas une seconde à 48 kHz stéréo |
| `radio_http_annonce_la_profondeur_detectee` | l'en-tête HTTP annonce encore 16 bits pour du PCM 24 bits |
| `radio_http_ne_devine_pas_la_profondeur` | un en-tête 16 bits est servi avant de connaître le PCM |

Restauration par `cp` des trois sauvegardes, puis même commande : **9 réussites**.
Journaux Shrek : `/srv/builds/bertrand/codex-5217-preuves/contre.log` et
`restaure.log` ; script : `/srv/builds/bertrand/codex-5217-contre-epreuve.sh`.
La reproduction préalable sur la base avait déjà donné trois échecs nommés
et une compatibilité OAAT réussie (`codex-5217-rouge-final.log`). La première
tentative, qui échouait au nettoyage du banc, a été écartée des preuves.

## Limites

La station Sveriges Radio P2 réelle et le DAC Windows/WASAPI du testeur n'ont
pas été exercés. Aucun gain audible ni bit-perfect matériel n'est affirmé.
La garde ajoutée concerne le décodeur radio, pas tous les pilotes/mixeurs.
Une source de plus de 24 bits reste convertie à 24 bits localement, ou refusée
en mode strict. Aucun déploiement n'est inclus ; l'issue reste ouverte pour
la validation terrain et le suivi de livraison.
