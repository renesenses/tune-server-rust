Tune 1.0.0-rc13.2, az rc13.1 javító kiadása.

## Hibajavítások (Fixes)

- Bejelentkezés: a rendszergazdai fiók jelszavát a szervergépen és a helyi hálózaton ugyanúgy ellenőrzi a rendszer; érthetőbb hibaüzenet (felhasználónév: default).
- Ha nem tud bejelentkezni, használja a default felhasználónevet és a szervergépen legutóbb beírt jelszót.
- A mozaiklabs.fr-fiók mostantól a helyi hálózat bármely böngészőjéből (iPad, telefon) összekapcsolható, ha még nincs összekapcsolt fiók.
- Windows: a beépített frissítés már nem áll le „Hozzáférés megtagadva” hibával a mellékelt bővítmények előkészítésekor (plugins.new\party). Az a bővítmény, amelyet nem lehet lecserélni, megtartja a verzióját, a frissítés pedig ettől függetlenül lefut.
- Windows rc13 vagy rc13.1 verzión: ezeknek a verzióknak a frissítési gombja továbbra is elakad. Telepítse ezt a verziót a telepítővel (setup.exe), vagy zárja be a Tune-t, és csomagolja ki a zip-archívumot a telepítési mappára. A további frissítések ismét a gombbal működnek.
