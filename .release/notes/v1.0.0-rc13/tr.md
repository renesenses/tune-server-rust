Tune 1.0.0-rc13, Tune 1.0'ın on üçüncü sürüm adayı. Bildirimleri ve denemeleri için Gros Bidon, r5yk5, Tades, cpadiou77, Pierre, Daniel LEVY, Lio, sashasha, ajm, Jean Valjean, Semakawa, Jon Rotaetxe, hifi25nl, aqs, Christopher J, FabienM, Begastor ve Régis'e, Smart Upsampler için alec_eiffel'e, Qobuz katkıları için cometdom'a ve tasarım için Levente'ye teşekkür ederiz.

## Yenilikler (New features)

- **Upsampling**: Rémi'nin (alec_eiffel) Smart Upsampler'ı kapalıyken olduğu seviyede çalıyor ve bir ayar değişikliği hemen duyulur.
- **Çalma ve çıkışlar**: Sendspin ve Devialet AIR işlenmiş sesi (ekolayzır, eklentiler, upsampling) alır. Zamanlanmış durdurma bilgisayarı kapatabilir; Linux'ta masaüstünün medya denetimleri.
- **Crossfeed**: Ayarları ve hazır ayarlarıyla birlikte ücretsiz.
- **Evrişim**: Her örnekleme hızı için bir dürtü yanıtı dosyası.
- **DSD**: Bir UPnP sunucusundan gelen DSD, seçici ağ oynatıcılarında doğal olarak çalar ve iki DSD parçası boşluksuz art arda çalar.
- **Kitaplık**: Kişisel etiketleriniz Tune etiketlerine dönüşür; toplu bir işlem için birden fazla albüm seçilebilir; enstrümana göre filtre.
- **Değiştirilmiş kaynak**, "Şüpheli Hi-Res" ibaresinin yerini alır; tüm biçimler için geçerlidir ve ölçülen kesim frekansını belirtir.
- **Dalga biçimi** daha ince; CUE, Qobuz ve TIDAL parçalarında da (isteğe bağlı).
- **Çalma sırası**: Albümler daraltılabilir ve tek bir hareketle taşınabilir.
- **Ana sayfa**: 5 "Dinlediğiniz için…" çalma listesi ve En çok çalınan sanatçılar, Sık çalınanlar, Sonra dinle ve Dinlemeye devam et widget'ları.
- **Hizmetler**: TIDAL'ın "Sizin için" sayfası; tür başına 200 yeni Qobuz albümü.
- **Tune Circle**: Bir çevrenin üyeleri, kabul ederlerse birbirlerini görebilir ve Dünya görünümü tüm çevreleri gösterir.
- **Web arayüzü**: Türkçe, arayüzden etkinleştirilen kimlik doğrulama ve parça ya da dinleme başına geçmiş.
- **Tune Client (masaüstü uygulaması)**: Ağdaki sunucuları kendiliğinden bulur ve yerel sunucuyu yönetir.
- **Windows**: Yükleyicinin önerdiği, Tune'u Windows ile birlikte başlatma seçeneği.
- **iOS uygulaması**: 5G'de daha eksiksiz CarPlay, iPad'de kenar çubuğunun geri dönüşü.

## İyileştirmeler (Improvements)

- **Geçişler**: "Otomatik" ReplayGain, aynı albümün iki parçası arasında albüm kazancını korur; AAC, Opus ve CUE parçaları boşluksuz art arda çalar.
- **Çalma ve çıkışlar**: AirPlay 1 ve 2'de aynı ses eğrisi; bir radyo ağ oynatıcısında daha hızlı başlar.
- **Windows**: WASAPI özel modunda Tune, parçayı durdurmak yerine diğer hız ailesine geçer.
- **Devialet AIR**: 1 dB'lik adımlarla ses seviyesi, 192 kHz'e kadar.
- **DSD**: Doğal DSD algılaması daha temkinli ve yeniden değerlendiriliyor.
- **Büyük kitaplıklar**: Klasörler ekranı beklemeden açılır, arka plan görevleri artık geri kalanını engellemez.
- **Hizmetler**: YouTube için Tune'un kendi Google oturum açma istemcisi; Spotify ve TIDAL çalma sırasında daha sağlam.
- **Web arayüzü**: Daha hassas parametrik ekolayzır ve silmeden önce onay.
- **iOS uygulaması**: Kilit ekranı, bölge çalmıyorken bile bölgeyi kontrol eder; DAC yokken kaydırıcı iOS ses seviyesini ayarlar.
- **Güncelleme ve tanılama**: Güncelleme, Tune ile birlikte gelen programları da değiştirir; hata raporları adları gizler.

## Düzeltmeler (Fixes)

- **Çalma ve çıkışlar**: Bir duraklatma DAC'ı serbest bıraktığında devam etme çalışıyor; "Tümünü tekrarla" artık kısa bir durmayı parçanın sonu sanmıyor; Windows'ta Tune'dan sonra açılan bir DAC bulunur.
- **Bölgeler**: Zaten bağlı bir cihaz bölgesinin adını verir; OAAT'de devam etmek, sessiz kalmak yerine oturumu yeniden açar.
- **DLNA, UPnP ve OpenHome**: Musicbook Source'ta gereksiz Durdur sona erdi, sessize alma kararlı, Linn Klimax DS/2 tanınıyor.
- **AirPlay**: AirPlay 1 çıkışı, yeniden başlatmadan sonra artık en yüksek seviyede başlamaz.
- **Sinyal yolu**: Artık gerçekleşmeyen bir işlemeyi göstermez.
- **Kitaplık**: Aynı albümün DSD ve CD baskıları artık birleştirilmiyor; DR yazımı artık sunucuyu dondurmuyor.
- **Hizmetler**: Doğru süreleriyle Qobuz ve TIDAL favorileri; Spotify bölümleri artık iki kez görünmüyor.
- **Radyolar**: FIP istasyonları logolarına yeniden kavuşuyor.
- **Eklentiler ve crossfeed**: Yüklenen bir eklenti yeniden başlatmadan etkili olur; crossfeed artık OAAT'de ve tarayıcıda kilitli değil.
- **Web arayüzü**: Izgara sonuna kadar kayar; "sunucu yanıt vermiyor" başlığı, sunucu yanıt verir vermez kaybolur.

## Güvenlik

- Çeşitli güvenlik sağlamlaştırmaları.
- Günlükler ve hata raporları artık bazı akış adreslerinin içerdiği kimlik bilgilerini gizliyor.
- Yolu değiştir (Ayarlar › müzik klasörleri) artık klasörleri doğruluyor ve kimlik doğrulama olmadan başka bir cihazdan erişilemiyor.

## Davranış değişiklikleri

- **Tune Circle**: Kimlik doğrulama etkinken yalnızca yöneticilere açık; uzaktan dinlemede salt okunur.
- **YouTube**: Bu sürümden önce bağlanan hesapların bir kez yeniden bağlanması gerekir.
- **Çalma sırası bitince radyo**: Varsayılan olarak yeniden kapalı; seçmişseniz bölgede yeniden işaretleyin.
- **Ekolayzır**: Ekolayzır kapatıldığında ön yükselticisi "Seviyeyi geri yükle" komutuna kadar uygulanmaya devam eder.
- **Çoklu oda**: 3 bölgeye kadar ücretsiz grup; üzerinde Premium gerekir. Stereo çiftler ücretsiz kalır.
- **INDEX 02 içeren CUE**: Parça başlangıçları bir sonraki taramada düzeltilir, geçmiş korunur.
- **Tune OS**: Geçici parola artık oturum açma sayfasında değil, makinenin ekranında veya önyükleme kartında gösterilir; değiştirilene kadar uzaktan erişim kapalı kalır.
- **Profiller ve geçmiş**: Kimlik doğrulama etkinken diğer profilleri yalnızca yönetici yönetir.
- **Yolu değiştir**: Kimlik doğrulama olmadan, Tune'un çalıştığı makineden.

## Lütfen kendi sisteminizde deneyin

Bu değişiklikler otomatik testlerden geçti, ancak gerçek donanımda denenmedi. Gözlemlerinizi bize bildirin:

- **Sendspin ve Devialet AIR**, bir işleme etkinken.
- **Windows**: mini oynatıcı, WASAPI özel modu, Windows ile birlikte başlatma.
- **AirPlay 1**: başlangıçta ve parçalar arasındaki seviye.
- **Ağ oynatıcısına DSD** (Atoll ST200, Eversolo).
- **Linn Klimax DS/2** OpenHome olarak, **macOS'ta Tune Client** ve **5G ile CarPlay**.

## Bilinen sorunlar

- **Yerel çıkışta DoP ile DSD**: Kısılmış bir PCM'den sonra tam ölçekte çalabilir. Amplifikatörün ses seviyesini düşük tutun.
- **Adres değişikliğinden sonra UPnP**: Bir parça çalmayabilir; o sunucuyu yeniden dizinleyin.
- **Kitaplık bütünlüğü denetimi**: Bazı dosyalar yanlışlıkla bildirilebilir; yalnızca bu sonuca dayanarak hiçbir şey silmeyin.
- **Başka bir sunucu tarafından sürülen Windows ASIO**: Duraklat ve ardından Çal komutundan sonra ses gelmez; sonraki parçaya geçin.
- **Güncellemeden sonra**: Arayüz değişmezse yeniden yüklemeyi zorlayın (Cmd+Shift+R veya Ctrl+Shift+R).
