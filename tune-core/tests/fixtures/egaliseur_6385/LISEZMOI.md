# Fichiers d'égaliseur réels — #6385

Quatre fichiers publiés, repris tels quels (aucun octet modifié), pour éprouver
l'import sur autre chose que du texte écrit à la main. Les profils AutoEq
ParametricEQ sont dans `../autoeq/`.

| Fichier | Format | Origine | Licence |
|---|---|---|---|
| `apo_rew_stereo_lg_tv.txt` | Equalizer APO `config.txt` (filtres générés par REW, sections `Channel: L` / `Channel: R`, ligne `Device:` en tête) | <https://github.com/giuliojiang/EqualizerAPO-configurations>, `config.txt`, commit `c8a30b261e4cf89dc08f71a64d1c41beb9591962` | GPL-2.0 (licence du dépôt d'origine) |
| `apo_philips_shp9500.txt` | Equalizer APO `config.txt` (`Preamp:`, plateaux `LSC`/`HSC`, filtres `Filter:` sans numéro, fins de ligne CRLF, pas de saut de ligne final) | <https://github.com/nuudlman/equalizer-apo-txts>, `shp9500.txt`, commit `97dabafba7ebb474b802bde874eb084d8f9050a2` | CC0-1.0 (licence du dépôt d'origine) |
| `apo_hyperx_cloud_ii_device.txt` | Equalizer APO `config.txt` (ligne `Device:` en tête, commentaires `#`, `Preamp:`, plateaux `LSC`/`HSC`) | <https://github.com/maseaaao/Equalizer-APO-Configs>, `OUT---HyperX-Cloud-II.txt`, commit `f8b382e79f435a8e5aa88ee5dc95e28b124f3068` | Apache-2.0 (licence du dépôt d'origine) |
| `opra_sennheiser_hd650_oratory1990.json` | OPRA `info.json` (schéma `schemas/eq_info.json`) | <https://github.com/opra-project/OPRA>, `database/vendors/sennheiser/products/hd650/eq/oratory1990_harman_target/info.json`, commit `d2d2c334752ec69db117a7e083deb9b6b2405ce2` | CC BY-SA 4.0 (jeu de données OPRA), auteur de la correction : oratory1990 |

Ces fichiers ne servent qu'aux tests : ils ne sont ni compilés dans Tune ni
distribués avec lui.
