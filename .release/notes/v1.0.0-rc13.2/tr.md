Tune 1.0.0-rc13.2, rc13.1 için düzeltme sürümü.

## Düzeltmeler (Fixes)

- Oturum açma: yönetici hesabının parolası sunucu makinesinde ve yerel ağda aynı şekilde denetleniyor; daha anlaşılır hata mesajı (kullanıcı adı: default).
- Artık oturum açamıyorsanız, default kullanıcı adını ve sunucu makinesinde en son girilen parolayı kullanın.
- Henüz bağlı bir hesap yokken mozaiklabs.fr hesabı artık yerel ağdaki herhangi bir tarayıcıdan (iPad, telefon) bağlanabiliyor.
- Yerel ağdan aynı hesap yeniden de bağlanabilir. Onu başka bir hesapla değiştirmek veya bağlantısını kaldırmak, sunucu makinesine ya da bir yöneticiye ayrılmış olmaya devam ediyor. Bağlı hesabın adı ve e-posta adresi yerel ağdan görülebilir.
- Windows: Tune içinden güncelleme yeniden çalışıyor. rc13 ve rc13.1'den beri yükleyiciyi kullanmak gerekiyordu.
- rc12'de kalmış ve artık güncellenmeyen Tune OS sunucuları: yeni "Débloquer Tune" aracı (Windows, Mac) sorunu hiçbir komut gerektirmeden birkaç tıklamayla çözer. Veritabanı başka bir bilgisayardan kopyalandıysa ve default hesabı yoksa, yönetici hesabınızın kullanıcı adını ve parolasını sorar. İndirme: https://github.com/renesenses/tune-server-rust/releases/tag/debloquer-tune-v1
