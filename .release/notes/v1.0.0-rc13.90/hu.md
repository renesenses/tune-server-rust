Tune 1.0.0-rc13.90: **a rc14 előzetes kiadása (pre-release)**, csak tesztelőknek. Ez nem közzétett verzió: az automatikus frissítés nem ajánlja fel. Köszönjük mindenkinek, aki teszteli.

## Újdonságok (New features)

- **TESS, a 64 bites feldolgozási lánc**: minden feldolgozás végig 64 bites lebegőpontos számítással történik, egyetlen kvantálással a legvégén.
- **Ugyanaz az út minden kimenetnek**: a helyi kimenet, az UPnP/DLNA, az AirPlay, a Devialet AIR és az OAAT is a TESS-en halad át; ha egy kimenet nem fogadhat egy feldolgozást, ezt egyértelműen jelzi.
- **Parametrikus hangszínszabályzó** szabad sávokkal (frekvencia, erősítés, Q), a grafikus és a fejhallgató-hangszínszabályzó (AutoEq-profilok) mellett.
- **Konvolúció**: szűrők .wav, .zip archívum vagy Convolver-konfiguráció (.cfg) formában, mintavételi frekvenciánként egy szűrő, megszakítás nélküli váltás és közölt késleltetés.
- **Mérésen alapuló teremkorrekció**: több hallgatási pozícióból számított, vegyes fázisú FIR-szűrő.
- **DSD-re való felmintavételezés** (DSD64, DSD128, DSD256) szigma-delta modulátorral.
- **Lánc-előbeállítások**: mentsen el egy teljes láncot, és hívja elő azonnal vagy a következő lejátszáskor.
- **Jelút**: a lejátszósávból, fokozatonként, mi érvényesül valójában, és miért nem egy adott fokozat.
- **Natív bővítmények** a láncban: menet közbeni bekapcsolás, 64 bites formátum, a csatornák sorrendje és száma megmarad.
- **Felület 20 nyelven**: megérkezik a holland, a hagyományos kínai, az orosz és az ukrán.
- **Hitelesítés**: „Hitelesítés kikapcsolása” gomb a Beállítások › Rendszer › Hitelesítés alatt.

## Fejlesztések (Improvements)

- **SACD ISO**: a natív DSD virtuális DSF-ként jut el a hálózati lejátszókhoz; a DoP-ben szállított DSD DSD-ként jelentkezik.
- **Távoli hallgatás**: 5G-n az első szám FLAC-ként indul.
- **Távol lévő eszközök**: 30 nap után automatikusan elfelejtődnek (állítható), zónát soha nem törölve.
- **Cover+**: jobb borítók, a webrádiókhoz is (ICY-metaadatok).
- **Csoportos lejátszás**: zónánként állítható eltolás; a vezető zónában az „Előző” a követő zónákat is újraindítja.
- **Könyvtár és streaming**: a háttérfolyamatok már nem akasztják meg a felületet, az albumok betűrendje megmarad, hasonló előadók egy szolgáltatás előadói oldaláról.
- **Elutasító üzenetek**: rövidebbek, és a megfelelő beállításra mutatnak.

## Hibajavítások (Fixes)

- **A rc13.1 és rc13.2 javításai** benne vannak: a rendszergazdai fiók bejelentkezése, a mozaiklabs.fr-fiók összekapcsolása a helyi hálózatról, borítók bekapcsolt hitelesítéssel, frissítés Windowson.
- **UPnP**: a Tune már nem indexeli önmagát; egy indexelt szám a szervere jelenlegi címéről szól.
- **Lejátszás**: 30 mp-es előtöltés eldobott puffer nélkül; a forrás által elutasított ugrás már nem tolja el a folyamot.
- **A Chromecast** újracsatlakozik; az AirPlay újraközzététele már nem szakítja meg a lejátszást.
- **Podcastok**: a nem elérhető epizódokat rendesen jelzi.
- **Fórumbejelentések**: előzmények, Qobuz, Spotify, „Vissza” gomb, keresés és iPad.

## Működésbeli változások

- **Parametrikus hangszínszabályzó**: a rc13-ig ingyenes volt, mostantól a Tune Premium része, akárcsak a konvolúció, a teremkorrekció, a valós idejű declick és a külső bővítmények. A grafikus hangszínszabályzó, a fejhallgató-hangszínszabályzó és a crossfeed ingyenes marad.

## Ellenőrizze otthon

- **Egy teljes TESS-lánc** (hangszínszabályzó, crossfeed, konvolúció vagy teremkorrekció): megfelel a jelút annak, amit hall?
- **Egy konvolúciós készlet importálása** és szűrőváltás lejátszás közben.
- **DSD-re való felmintavételezés** kompatibilis DAC-on: stabilitás, szint, kattanások hiánya.
- **Lánc-előbeállítások**, mindkét alkalmazási módban.
- **Egy hálózati kimenet** (UPnP, AirPlay vagy OAAT) aktív feldolgozással.
- **Frissítés rc13.x-ről** és a „Hitelesítés kikapcsolása” gomb.

## Ismert problémák

- **DST-tömörítésű SACD-k**: ezeket az ISO-képeket még nem játssza le natívan.
- **VU-mérők** továbbított MP3, AAC vagy Ogg számon: ugrás után a szám végéig kikapcsolnak. A hangot ez nem érinti.
- **Távoli hallgatás CarPlay-en**: a továbbító javítása a rc15-re tolódik.
- **Képernyő nélküli szerver kikapcsolt hitelesítéssel**: az összekapcsolt mozaiklabs.fr-fiók cseréjéhez kapcsolja be a hitelesítést, vagy használjon SSH-alagutat a localhost:8888 felé.
