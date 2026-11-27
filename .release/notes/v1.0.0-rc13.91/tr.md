Tune 1.0.0-rc13.91: **rc14'ün ikinci ön sürümü (pre-release)**, yalnızca test edenler için. Bu yayımlanmış bir sürüm değildir: otomatik güncelleme bunu önermez. Bu sürüm rc13.90'ın tüm içeriğini kapsar. Test eden herkese teşekkürler.

## Yenilikler (New features)

- **Seri bağlanabilen ekolayzırlar**: grafik, parametrik ve kulaklık ekolayzırı ile oda düzeltmesi aynı TESS zincirinde art arda çalışır; oda düzeltmesi uygulamak artık diğer ekolayzırların üzerine yazmıyor. Parametrik ekolayzırın kazanç aralığı genişletildi.
- **Tam Bypass all**: tek bir hareket tüm TESS zincirini devre dışı bırakır ve aynen geri getirir.
- **Kulaklık eklentisi**, OPRA veritabanından: kulaklığınızın profili ön yükseltme ve seviyeyle birlikte tanınır.
- **Speaker Setup eklentisi** (hoparlör ayarı): mesafe, kazanç ve faz, kanal kanal.
- **Yerel çıkışta DSD'ye yukarı örnekleme**: özel bir sigma-delta modülatörü; çıkışın DSD yetenekleri de yayımlanır.
- **Seçilebilir yukarı örnekleme hedefi**: TESS'te bir ekran, yukarı örneklemenin PCM'e mi yoksa DSD'ye mi yapılacağını seçmenizi sağlar.
- **Mikrofon seviye testi**, oda ölçümünden önce.
- **Mikrofonun kendi frekansında ölçüm**, 44,1 ile 192 kHz arasında.
- **Anında TESS ayarları**: bir aşamayı devre dışı bırakmak hemen etkili olur ve ön yükseltme sınırlı kalır.

## İyileştirmeler (Improvements)

- **Oda düzeltmesi**: doğrulama ölçümü bölgenin düzeltmesini duyar; düzeltilmeyen sapmalar açıklanır ve üst sınır 1 kHz'e kadar ayarlanabilir.
- **Oda düzeltmesi**: girilen bilgiler, son ölçüm ve uygulanan düzeltme ekrana geri dönüldüğünde yerinde bulunur; mikrofon yerleştirme talimatları ve bir "Ölçümden önce" kutusu ölçüme rehberlik eder.
- **PURE modunda ölçüm**: tarama süresince ses seviyesi %100'e zorlanır, ardından önceki haline döndürülür.
- **Sinyal yolu**: kapalı bir aşama (yukarı örnekleme, konvolüsyon) nedenini TESS'te ve eklenti sayfasında söyler; Windows'ta paylaşılan modda yukarı örnekleme mikserin frekansını hedefler.
- **Katkıda bulunanlar**: "Müzisyen" yerine enstrüman, besteciden ayrı olarak yazar, rol başına bir ad.
- **Tarama**: etiketlerde bulunan barkod, albümde yoksa albümü tamamlar.
- **Sunucu durumu**: kapak önbelleği oluşturulamazsa bir uyarı görüntülenir.

## Düzeltmeler (Fixes)

- **Kalibrasyon**: tarama artık bildirilen süreden önce durmuyor ve kuyrukta takılı kalmıyor; çıkışı olmayan bir bölge açıkça reddediliyor.
- **Kalibrasyon**: tavana ulaşan bir tepe artık sonraki bantları engellemiyor.
- **RT60**: yanlış bir değer yerine "ölçülemez (gürültü)"; hata mesajları en az 10 sn okunabilir kalıyor.
- **Frekans uyarısı**: artık yalnızca kabul edilen frekansların dışında görünüyor.
- **Radyo**: istasyon değişikliği artık eski istasyon tarafından ezilmiyor.
- **Kapaklar**: yeni bir kurulumda önbellek veri klasörünün altında oluşturuluyor.
- **TESS**: tek bir "sonraki parça" bildirimi; "Uygula" düğmesinin yardım metni düzeltildi.

## Davranış değişiklikleri

- **Çalma sırasında TESS zincirini değiştirme**: devre dışı bırakılan bir aşama hemen kesilir; eklenen veya yeniden etkinleştirilen bir aşama bir sonraki çalmadan itibaren duyulur.

## Lütfen kendi sisteminizde deneyin

Bu noktalar henüz gerçek donanımda test edilmedi:

- **Gerçek bir DSD DAC'ta DSD'ye yukarı örnekleme**: kararlılık, seviye, tıklama olmaması.
- **44,1 kHz ve 96 kHz'de oda ölçümü**, gerçek bir ses arayüzünde, mikrofon seviye testiyle.
- **Windows'ta, paylaşılan WASAPI modunda**: çalma ve yukarı örnekleme.
- **Bir ağ çıkışında Speaker Setup** (UPnP, AirPlay veya OAAT): mesafe, kazanç ve faz.

Ayrıca:

- **Seri bağlı birden çok ekolayzır**, Bypass all ile: sinyal yolu duyduğunuzla örtüşüyor mu?
- **Kulaklık eklentisi**, kendi kulaklığınızla.
- **rc13.90'dan güncelleme.**

## Bilinen sorunlar

- **DST ile sıkıştırılmış SACD'ler**: bu ISO görüntüleri henüz yerel olarak çalınmıyor.
- **VU metreler**, aktarılan bir MP3, AAC veya Ogg parçasında: atlamadan sonra parçanın sonuna kadar kapanır. Ses etkilenmez.
- **CarPlay'de uzaktan dinleme**: aktarıcı düzeltmesi rc15'e ertelendi.
- **Ekransız sunucu, kimlik doğrulama kapalı**: bağlı mozaiklabs.fr hesabını değiştirmek için kimlik doğrulamayı açın veya localhost:8888'e bir SSH tüneli kullanın.
