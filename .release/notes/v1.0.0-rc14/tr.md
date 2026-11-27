Tune 1.0.0-rc14, rc13.90 ve rc13.91 ön sürümlerinin tüm içeriğini kapsar; bu notlar rc13.91'den bu yana değişenleri anlatır. Test kullanıcılarına teşekkür ederiz: bu maddelerin çoğu sizin geri bildirimlerinizden geliyor.

## Yenilikler (New features)

- **TESS, Kaydet ve Yeniden yükle ön ayarları**: bir ayar seçilen ön ayardan saptığı anda TESS “değiştirildi” gösterir. **Kaydet** özgün ön ayarın üzerine yazar, **Yeniden yükle** ona geri döner. Tune ile gelen bir ön ayarın, örneğin “Pure”un, üzerine yazılamaz: “Farklı kaydet…” bir kopya oluşturur.
- **TESS, hemen uygulanan DSD ön ayarı**: DSD hedefini değiştiren (örneğin DSD128 → DSD256) ve “hemen uygula” modunda yüklenen bir ön ayar, bir sonraki parçayı beklemek yerine parçayı bulunduğu konumdan yeniden başlatır. Upsampling bir Premium özelliğidir.
- **Pay (headroom)**: Otomatik ya da elle −12 ile 0 dB arasında, 0,5 dB'lik adımlarla. Panel gerçekten uygulanan değeri gösterir ya da neden uygulanmadığını söyler.
- **DSD'ye upsampling sırasında pay**: elle ayarlanan pay artık iki kez uygulanmıyor (−12 dB önceden −24 dB veriyordu; artık −15 dB, bunun −3 dB'si DSD modülatörü için ve ayrı bir satırda gösteriliyor).
- **75 dB(C)'ye yönlendirmeli oda ölçümü**: kalibre edilmiş bir mikrofonla ekran, ölçülen seviyeyi, 75 dB(C)'den farkını ve bir ses düzeyi talimatını gösterir. Dönüşüm henüz bir kalibratörle doğrulanmadığı için kural temkinlidir: Tune tahmini 80 dB(C)'nin aşılmasını asla önermez ve 90 dB SPL'de sinyali keser. Mikrofonla ölçülen düzeltme Premium'dur; mikrofonsuz hesaplanan düzeltme ücretsiz kalır.
- **İşitme güvenliği**: bir ekolayzerin işaretini kaldırmak, PURE veya Bypass all moduna geçmek ya da bir konvolüsyonu, crossfeed'i veya mono indirgemeyi kaldırmak artık seviyeyi bir anda yükseltmiyor: seviye, yaklaşık yarım saniyelik bir geçişle önceki düzeyine döner. Bu arada ses düzeyini yükselttiyseniz “seviye düşürüldü” göstergesi yanar ve “Seviyeyi geri getir” fazlasını serbest bırakır. Bu, ağ çıkışları (UPnP, OAAT) için de geçerlidir.
- **Gerçeğe uygun sinyal yolu**:
  - bir ağ çıkışında, oynatıcının ses düzeyine kadar akışın gerçek sırasını izler;
  - bir dürtü yanıtı filtresinin aşamasının adı “Konvolüsyon”dur;
  - filtre, içe aktarılan .wav veya .zip dosyasının adını korur;
  - DSD hedefi değiştikten sonra gerçekten çalınan DSD hızını gösterir ve bir sonrakini duyurur;
  - pencere yeşile döner: turuncu, gerçek uyarılara ayrılmıştır.
- **Konvolüsyon**: panel, yüklenen filtrenin eğrisini kanal kanal ve istek üzerine fazı gösterir.
- **Bypass all**: TESS atlandığında oynatma çubuğundaki TESS simgesi soluklaşır ve TESS sayfası “TESS atlandı” gösterir.
- **Uyumlu hale getirilmiş TESS sayfası**: TESS yeşili vurgu, rozetin yanında geçerli bölge, ağ çıkışında gri Headroom paneli ve “Mikrofonsuz (tahmin)” ya da “Mikrofonla (önerilen)” ölçüm.
- **Ağ çıkışı**: Upsampling kartı, upsampling'in orada uygulanmadığını söyler.
- **Uyumlu hale getirilmiş arayüz**: çevrimdışıyken de görüntülenen gömülü yazı tipleri; Kitaplık, Live Radio, Medya sunucuları ve Konserler yeni tasarımı izler.

## Düzeltmeler (Fixes)

- **Sonos'a radyo** (ve hız sınırı olan her ağ oynatıcısına): 192 kHz'lik bir radyo istasyonu oynatıcının kabul ettiği hızda sunuluyor. Önceden oynatıcı hemen, tekrar tekrar duruyordu.
- **768 kHz**: bir bölgenin “Maks. frekans” listesi 768 kHz ve 1.536 kHz sunuyor.
- **Kitaplık**: albüm ızgarası büyük bir koleksiyonda kaydırma sırasında artık donmuyor.
- **Tarayıcı sekmesinde oynatma**: parça içinde ileri geri gitmek, başka bir cihazdan bile, oynatmayı gerçekten taşıyor.
- **Yardım**: öncelikli destek bandı artık bir Premium hesapta görünmüyor.
- **Lisans**: son kullanma tarihi doğru okunuyor ve Ayarlar › Lisans katmanın neden ücretsiz olduğunu söylüyor.
- **Konvolüsyon**: bazı frekansları yükselten bir filtre artık kırpılmaya (clipping) yol açmıyor.
- **Uzun podcast'ler**: barındırma hizmeti bağlantıyı düzenli olarak kesen bir bölüm artık yarıda kalmıyor.
- **LXC kapsayıcısında Debian paketi** (Proxmox): hizmet, özel bağlama ad alanları olmadan da başlıyor.
- **İlk başlatma**: sihirbaz, hesabı olmayan hizmetleri artık “Etkin” olarak adlandırmıyor ve seçilen müzik klasörünü kaydediyor. Boş “Radyolarım” radyo dizinini açıyor.

## Güvenlik ve hesaplar

- **Yenilenen oturum**: “Oturumu açık tut” ile (varsayılan olarak işaretsiz) tarayıcı oturumu kullanıldığı sürece, 90 güne kadar yenilenir. Aksi halde, eskisi gibi 24 saat. Avatar menüsü “Bu sunucudan çıkış yap” seçeneğini sunar.
- **Yönetici erişimini geri kazanma**: `tune-server --reset-admin` (veya `--reinitialiser-admin`) komutunu **sunucu makinesinde** çalıştırın. Verdiği 15 dakika geçerli tek kullanımlık kod, bu makineden “Parolanızı mı unuttunuz?” bölümüne girilir; tarayıcı yoksa `--password-stdin` ekleyin. Açık oturumlar kapatılır. Kurulum kılavuzu (bölüm 9) her sistem için komutu verir.
- **`admin` profili**: varsayılan olarak oluşturulan yönetici profilinin adı artık `admin`; sizin seçtiğiniz bir ad korunur ve `default` kabul edilmeye devam eder.
- **İlk yapılandırmadan itibaren yönetici parolası**: yeni bir kurulumda sihirbaz bu parolayı oluşturmanızı ister.
- **Kimlik doğrulamasız sunucu**: yönetim artık yalnızca sunucu makinesinden değil, yerel ağdan da kabul edilir. **Uyarı**: kimlik doğrulama olmadan yerel ağdaki cihazlar sunucuyu yönetebilir; ağınız güvenilir değilse kimlik doğrulamayı etkinleştirin. “Yalnızca bu makineden yönetim” ayarı eski kuralı geri getirir.

## Uzaktan kumandalar

- Korumalı bir sunucuda, henüz güncellenmemiş uygulamalar eskisi gibi her 24 saatte bir parolayı yeniden ister.

## Lütfen kendi sisteminizde deneyin

- **Bir ön ayarda “Bypass all”**: hem Bypass all'u hem DSD hedefini değiştiren ve “hemen uygula” modunda olan bir ön ayar, parçayı bulunduğu konumdan yeniden başlatır. Yalnızca test tezgâhında doğrulandı: yeniden başlamanın temiz olup olmadığını bize bildirin.
