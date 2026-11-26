Tune 1.0.0-rc13.91: **a rc14 második előzetes kiadása (pre-release)**, csak tesztelőknek. Ez nem közzétett verzió: az automatikus frissítés nem ajánlja fel. A rc13.90 teljes tartalmát magában foglalja. Köszönjük mindenkinek, aki teszteli.

## Újdonságok (New features)

- **Sorba köthető hangszínszabályzók**: a grafikus, a parametrikus és a fejhallgató-hangszínszabályzó, valamint a teremkorrekció ugyanabban a TESS-láncban követi egymást; a teremkorrekció alkalmazása már nem írja felül a többi hangszínszabályzót. A parametrikus hangszínszabályzó erősítési tartománya bővült.
- **Teljes Bypass all**: egyetlen mozdulattal az egész TESS-lánc kiiktatható, majd pontosan ugyanúgy visszaállítható.
- **Fejhallgató-bővítmény** az OPRA adatbázisából: felismeri a fejhallgatója profilját, előerősítéssel és szinttel együtt.
- **Speaker Setup bővítmény** (hangszórók beállítása): távolság, erősítés és fázis, csatornánként.
- **DSD-re való felmintavételezés a helyi kimeneten**: saját szigma-delta modulátorral, és a kimenet DSD-képességei közzé vannak téve.
- **Választható felmintavételezési cél**: a TESS-ben egy képernyőn kiválasztható, mire történjen a felmintavételezés, PCM-re vagy DSD-re.
- **A mikrofon szintjének tesztje** a teremmérés előtt.
- **Mérés a mikrofon natív mintavételi frekvenciáján**, 44,1 és 192 kHz között.
- **TESS-beállítások menet közben**: egy fokozat kikapcsolása azonnal érvényes, és az előerősítés korlátok között marad.

## Fejlesztések (Improvements)

- **Teremkorrekció**: az ellenőrző mérés már a zóna korrekciójával szól; a nem korrigált eltéréseket megmagyarázza, és a felső határ 1 kHz-ig állítható.
- **Teremkorrekció**: a beírt adatok, az utolsó mérés és az alkalmazott korrekció megmarad, amikor visszatér a képernyőre; a mikrofon elhelyezésére vonatkozó útmutatás és egy „A mérés előtt” keret segíti a mérést.
- **Mérés PURE módban**: a hangerő a sweep idejére 100%-ra van kényszerítve, utána visszaáll az eredeti értékre.
- **Jelút**: egy kikapcsolt fokozat (felmintavételezés, konvolúció) megmondja, miért, a TESS-ben és a bővítmény oldalán is; Windowson megosztott módban a felmintavételezés a keverő frekvenciáját célozza.
- **Közreműködők**: a hangszer a „Zenész” helyett, a szövegíró külön a zeneszerzőtől, szerepenként egy név.
- **Beolvasás**: a címkékben található vonalkód kiegészíti az albumot, ha annak nincs.
- **Szerverállapot**: figyelmeztetés jelenik meg, ha a borítók gyorsítótára nem hozható létre.

## Hibajavítások (Fixes)

- **Kalibrálás**: a sweep már nem áll le a jelzett hossz előtt, és nem ragad be a sorba; a kimenet nélküli zónát egyértelműen elutasítja.
- **Kalibrálás**: a plafont elérő csúcs már nem blokkolja a következő sávokat.
- **RT60**: hamis érték helyett „nem mérhető (zaj)”; a hibaüzenetek legalább 10 másodpercig olvashatók maradnak.
- **Frekvencia-figyelmeztetés**: már csak az elfogadott frekvenciákon kívül jelenik meg.
- **Rádió**: az állomásváltást már nem írja felül a korábbi állomás.
- **Borítók**: friss telepítésnél a gyorsítótár az adatmappa alatt jön létre.
- **TESS**: egyetlen „következő szám” értesítés, és az „Alkalmaz” gomb súgója javítva.

## Működésbeli változások

- **A TESS-lánc módosítása lejátszás közben**: egy kikapcsolt fokozat azonnal leáll; egy hozzáadott vagy újra bekapcsolt fokozat a következő lejátszástól hallható.

## Ellenőrizze otthon

Ezeket a pontokat még nem tesztelték valódi hardveren:

- **DSD-re való felmintavételezés valódi DSD DAC-on**: stabilitás, szint, kattanások hiánya.
- **Teremmérés 44,1 kHz-en és 96 kHz-en** valódi hangkártyával, a mikrofon szintjének tesztjével együtt.
- **Windowson, megosztott WASAPI-val**: lejátszás és felmintavételezés.
- **Speaker Setup hálózati kimeneten** (UPnP, AirPlay vagy OAAT): távolság, erősítés és fázis.

És még:

- **Több sorba kötött hangszínszabályzó** Bypass all-lal: megfelel a jelút annak, amit hall?
- **A Fejhallgató-bővítmény** a saját fejhallgatójával.
- **Frissítés a rc13.90-ről.**

## Ismert problémák

- **DST-tömörítésű SACD-k**: ezeket az ISO-képeket még nem játssza le natívan.
- **VU-mérők** továbbított MP3, AAC vagy Ogg számon: ugrás után a szám végéig kikapcsolnak. A hangot ez nem érinti.
- **Távoli hallgatás CarPlay-en**: a továbbító javítása a rc15-re tolódik.
- **Képernyő nélküli szerver kikapcsolt hitelesítéssel**: az összekapcsolt mozaiklabs.fr-fiók cseréjéhez kapcsolja be a hitelesítést, vagy használjon SSH-alagutat a localhost:8888 felé.
