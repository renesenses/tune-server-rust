Tune 1.0.0-rc13.2, az rc13.1 javító kiadása.

## Hibajavítások (Fixes)

- Bejelentkezés: a rendszergazdai fiók jelszavát a szervergépen és a helyi hálózaton ugyanúgy ellenőrzi a rendszer; érthetőbb hibaüzenet (felhasználónév: default).
- Ha nem tud bejelentkezni, használja a default felhasználónevet és a szervergépen legutóbb beírt jelszót.
- A mozaiklabs.fr-fiók mostantól a helyi hálózat bármely böngészőjéből (iPad, telefon) összekapcsolható, ha még nincs összekapcsolt fiók.
- A helyi hálózatról ugyanaz a fiók újra is csatlakoztatható. Másik fiókra cserélni vagy a kapcsolatot megszüntetni továbbra is csak a szervergépen vagy rendszergazdaként lehet. Az összekapcsolt fiók neve és e-mail-címe a helyi hálózatról látható.
- Windows: a Tune-ból indított frissítés ismét működik. Az rc13 és az rc13.1 óta a telepítőt kellett használni.
- Rc12-es Tune OS, amely már nem frissül: a „Débloquer Tune” eszköz (Windows, macOS) parancsok nélkül újraindítja a frissítést. Külön tölthető le: https://github.com/renesenses/tune-server-rust/releases/tag/debloquer-tune-v1
