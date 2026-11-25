Tune 1.0.0-rc13.90: **rc14'ün ön sürümü (pre-release)**, yalnızca test edenler için. Bu yayımlanmış bir sürüm değildir: otomatik güncelleme bunu önermez. Test eden herkese teşekkürler.

## Yenilikler (New features)

- **TESS, 64 bit işleme zinciri**: tüm işleme baştan sona 64 bit kayan noktalı olarak yapılır, en sonda tek bir nicemleme ile.
- **Tüm çıkışlar için aynı yol**: yerel çıkış, UPnP/DLNA, AirPlay, Devialet AIR ve OAAT TESS'ten geçer; bir işlemi alamayan çıkış bunu açıkça söyler.
- **Parametrik ekolayzır**: serbest bantlarla (frekans, kazanç, Q), grafik ekolayzırın ve kulaklık ekolayzırının (AutoEq profilleri) yanında.
- **Konvolüsyon**: .wav, .zip arşivi veya Convolver yapılandırması (.cfg) olarak filtreler, örnekleme hızı başına bir filtre, kesintisiz geçiş ve bildirilen gecikme.
- **Ölçümle oda düzeltmesi**: birden çok dinleme konumundan hesaplanan karma fazlı FIR filtresi.
- **DSD'ye yukarı örnekleme** (DSD64, DSD128, DSD256), sigma-delta modülatörüyle.
- **Zincir ön ayarları**: tam bir zinciri kaydedin ve hemen ya da bir sonraki çalmada geri çağırın.
- **Sinyal yolu**: çalma çubuğundan, aşama aşama, gerçekte neyin uygulandığı ve bir aşamanın neden uygulanmadığı.
- **Yerel eklentiler** zincirde: çalışırken etkinleştirme, 64 bit biçim, kanal sırası ve sayısı korunur.
- **20 dilde arayüz**: Felemenkçe, Geleneksel Çince, Rusça ve Ukraynaca geliyor.
- **Kimlik doğrulama**: Ayarlar › Sistem › Kimlik doğrulama altında "Kimlik doğrulamayı kapat" düğmesi.

## İyileştirmeler (Improvements)

- **SACD ISO**: yerel DSD, ağ oynatıcılarına sanal DSF olarak sunulur; DoP ile taşınan DSD, DSD olarak bildirilir.
- **Uzaktan dinleme**: 5G'de ilk parça FLAC olarak gönderilir.
- **Bulunmayan cihazlar**: 30 gün sonra otomatik olarak unutulur (ayarlanabilir), hiçbir zaman bir bölge silinmez.
- **Cover+**: web radyoları dahil daha iyi kapaklar (ICY meta verileri).
- **Grup çalma**: bölge başına ayarlanabilir kaydırma; öncü bölgede "Önceki", takip eden bölgeleri de yeniden başlatır.
- **Kitaplık ve akış**: arka plan işlemleri artık arayüzü kilitlemiyor, albümlerin alfabetik sıralaması korunuyor, bir hizmetin sanatçı sayfasından benzer sanatçılar.
- **Ret mesajları** daha kısa ve doğru ayara yönlendiriyor.

## Düzeltmeler (Fixes)

- **rc13.1 ve rc13.2 düzeltmeleri** dahildir: yönetici hesabıyla giriş, mozaiklabs.fr hesabının yerel ağdan bağlanması, kimlik doğrulama açıkken kapaklar, Windows'ta güncelleme.
- **UPnP**: Tune artık kendini dizinlemiyor; dizinlenmiş bir parça, sunucusunun güncel adresinden çalınır.
- **Çalma**: atılan arabellek olmadan 30 sn ön yükleme; kaynağın reddettiği bir atlama artık akışı kaydırmıyor.
- **Chromecast** yeniden bağlanıyor; AirPlay yeniden yayını artık çalmayı kesmiyor.
- **Podcast'ler**: kullanılamayan bölümler düzgün bildiriliyor.
- **Forum bildirimleri**: geçmiş, Qobuz, Spotify, "Geri" düğmesi, arama ve iPad.

## Davranış değişiklikleri

- **Parametrik ekolayzır**: rc13'e kadar ücretsizdi, artık konvolüsyon, oda düzeltmesi, gerçek zamanlı declick ve üçüncü taraf eklentiler gibi Tune Premium'un parçası. Grafik ekolayzır, kulaklık ekolayzırı ve crossfeed ücretsiz kalıyor.

## Lütfen kendi sisteminizde deneyin

- **Tam bir TESS zinciri** (ekolayzır, crossfeed, konvolüsyon veya oda düzeltmesi): sinyal yolu duyduğunuzla örtüşüyor mu?
- **Bir konvolüsyon setinin içe aktarılması** ve çalma sırasında filtre değiştirme.
- **DSD'ye yukarı örnekleme**, uyumlu bir DAC'ta: kararlılık, seviye, tıklama olmaması.
- **Zincir ön ayarları**, iki uygulama modunda da.
- **Bir ağ çıkışı** (UPnP, AirPlay veya OAAT), etkin bir işlemle.
- **rc13.x'ten güncelleme** ve "Kimlik doğrulamayı kapat" düğmesi.

## Bilinen sorunlar

- **DST ile sıkıştırılmış SACD'ler**: bu ISO görüntüleri henüz yerel olarak çalınmıyor.
- **VU metreler**, aktarılan bir MP3, AAC veya Ogg parçasında: atlamadan sonra parçanın sonuna kadar kapanır. Ses etkilenmez.
- **CarPlay'de uzaktan dinleme**: aktarıcı düzeltmesi rc15'e ertelendi.
- **Ekransız sunucu, kimlik doğrulama kapalı**: bağlı mozaiklabs.fr hesabını değiştirmek için kimlik doğrulamayı açın veya localhost:8888'e bir SSH tüneli kullanın.
