A Tune 1.0.0-rc14 tartalmazza az rc13.90 és rc13.91 előzetes kiadások teljes tartalmát; ez a leírás az rc13.91 óta történt változásokat ismerteti. Köszönjük a tesztelőknek: e pontok közül sok az Önök visszajelzéseiből született.

## Újdonságok (New features)

- **TESS, Mentés és Újratöltés az előbeállításoknál**: amint egy beállítás eltér a kiválasztott előbeállítástól, a TESS „módosítva” jelzést mutat. A **Mentés** felülírja az eredeti előbeállítást, az **Újratöltés** visszaáll rá. A Tune-nal szállított előbeállítás, például a „Pure”, nem írható felül: a „Mentés másként…” másolatot készít.
- **TESS, azonnal alkalmazott DSD-előbeállítás**: a DSD-célt módosító előbeállítás (például DSD128 → DSD256) „azonnali alkalmazás” módban betöltve a számot a jelenlegi pozíciójától indítja újra, ahelyett hogy a következő számra várna. A felülmintavételezés Premium funkció.
- **Tartalék (headroom)**: Automatikus, vagy kézzel −12 és 0 dB között, 0,5 dB-es lépésekben. A panel a ténylegesen alkalmazott értéket mutatja, vagy megmondja, miért nem alkalmazza.
- **Tartalék DSD-re történő felülmintavételezéskor**: a kézi tartalék már nem érvényesül kétszer (a −12 dB korábban −24 dB-t adott, most −15 dB, ebből −3 dB a DSD-modulátoré, külön sorban megjelenítve).
- **75 dB(C)-re vezetett teremmérés**: kalibrált mikrofonnal a képernyő a mért szintet, a 75 dB(C)-től való eltérést és egy hangerő-utasítást mutat. A szabály óvatos, mert az átszámítást még nem ellenőrizték kalibrátorral: a Tune soha nem javasolja a becsült 80 dB(C) túllépését, és 90 dB SPL-nél lekapcsolja a jelet. A mikrofonnal mért korrekció Premium; a mikrofon nélkül számított ingyenes marad.
- **Hallásvédelem**: egy hangszínszabályzó kikapcsolása, átváltás PURE vagy Bypass all módra, illetve egy konvolúció, crossfeed vagy monó lekeverés eltávolítása már nem emeli meg hirtelen a szintet: az körülbelül fél másodperces átúsztatással tér vissza a korábbi értékre. Ha közben feljebb vette a hangerőt, kigyullad a „csökkentett szint” jelző, és a „Szint visszaállítása” feloldja a többletet. Ez a hálózati kimenetekre (UPnP, OAAT) is érvényes.
- **Hű jelút**:
  - hálózati kimeneten a folyam valós sorrendjét követi, egészen a lejátszó hangerejéig;
  - az impulzusválasz-szűrő lépésének neve „Konvolúció”;
  - a szűrő megtartja az importált .wav vagy .zip fájl nevét;
  - a DSD-cél módosítása után a ténylegesen játszott DSD-frekvenciát mutatja, és jelzi a következőt;
  - az ablak zöldre vált: a narancssárga a valódi figyelmeztetéseknek van fenntartva.
- **Konvolúció**: a panel a betöltött szűrő görbéjét mutatja, csatornánként, kérésre a fázist is.
- **Bypass all**: a lejátszósáv TESS-ikonja halványabb, ha a TESS meg van kerülve, és a TESS oldal „TESS megkerülve” jelzést mutat.
- **Egységesített TESS oldal**: TESS-zöld kiemelőszín, az aktuális zóna a jelvény mellett, hálózati kimeneten szürke Headroom panel, valamint „Mikrofon nélkül (becslés)” vagy „Mikrofonnal (ajánlott)” mérés.
- **Hálózati kimenet**: a Felülmintavételezés kártya közli, hogy a felülmintavételezés ott nem érvényes.
- **Egységesített felület**: beágyazott betűtípusok, offline is; a Gyűjtemény, a Live Radio, a Médiaszerverek és a Koncertek az új arculatot követi.

## Hibajavítások (Fixes)

- **Rádió Sonosra** (és minden korlátozott frekvenciájú hálózati lejátszóra): a 192 kHz-es rádióadót a lejátszó által elfogadott frekvencián kapja. Korábban a lejátszó azonnal leállt, újra és újra.
- **768 kHz**: egy zóna „Max. frekvencia” listája 768 kHz-et és 1536 kHz-et is kínál.
- **Gyűjtemény**: az albumrács már nem fagy le egy nagy gyűjtemény görgetése közben.
- **Lejátszás a böngészőlapon**: a tekerés valóban áthelyezi a lejátszást, másik eszközről is.
- **Súgó**: a soron kívüli támogatás sávja már nem jelenik meg Premium fióknál.
- **Licenc**: a lejárati dátum helyesen olvasható be, és a Beállítások › Licenc megmondja, miért ingyenes a szint.
- **Konvolúció**: az egyes frekvenciákat kiemelő szűrő már nem torzít.
- **Hosszú podcastok**: az az epizód, amelynek tárhelye rendszeresen bontja a kapcsolatot, már nem szakad meg.
- **Debian-csomag LXC-konténerben** (Proxmox): a szolgáltatás privát csatolási névterek nélkül is elindul.
- **Első indítás**: a varázsló már nem nevezi „Bekapcsolva” állapotúnak a fiók nélküli szolgáltatásokat, és elmenti a kiválasztott zenemappát. Az üres „Saját rádióim” a rádiókatalógust nyitja meg.

## Biztonság és fiókok

- **Megújuló munkamenet**: a „Maradjak bejelentkezve” beállítással (alapértelmezés szerint nincs bejelölve) a böngésző munkamenete addig újul meg, amíg használják, legfeljebb 90 napig. Egyébként 24 óra, mint korábban. Az avatar menüje „Kijelentkezés erről a szerverről” lehetőséget kínál.
- **Rendszergazdai hozzáférés visszaszerzése**: futtassa a `tune-server --reset-admin` (vagy `--reinitialiser-admin`) parancsot **a szerver gépén**. A kapott, 15 percig érvényes egyszer használatos kódot az „Elfelejtette a jelszót?” mezőbe kell beírni ezen a gépen; böngésző nélkül adja hozzá a `--password-stdin` kapcsolót. A nyitott munkamenetek bezáródnak. A telepítési útmutató (9. szakasz) minden rendszerhez megadja a parancsot.
- **`admin` profil**: az alapértelmezésként létrehozott rendszergazdai profil neve mostantól `admin`; az Ön által választott név megmarad, és a `default` továbbra is elfogadott.
- **Rendszergazdai jelszó már az első beállításkor**: új telepítésnél a varázsló létrehozatja.
- **Hitelesítés nélküli szerver**: a felügyelet a helyi hálózatról is elfogadott, nem csak a szerver gépéről. **Figyelem**: hitelesítés nélkül a helyi hálózat eszközei felügyelhetik a szervert; kapcsolja be a hitelesítést, ha a hálózata nem megbízható. A „Felügyelet csak erről a gépről” beállítás visszaállítja a korábbi szabályt.

## Távvezérlők

- Védett szerveren a még nem frissített alkalmazások 24 óránként újra kérik a jelszót, mint korábban.

## Ellenőrizze otthon

- **„Bypass all” egy előbeállításban**: az az előbeállítás, amely egyszerre változtatja meg a Bypass all-t és a DSD-célt, „azonnali alkalmazás” módban a számot a jelenlegi pozíciójától indítja újra. Csak tesztpadon ellenőriztük: jelezze nekünk, tiszta-e az újraindulás.
