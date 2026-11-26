Tune 1.0.0-rc13.90: **bản phát hành trước (pre-release) của rc14**, chỉ dành cho người thử nghiệm. Đây không phải là phiên bản đã phát hành: bản cập nhật tự động không đề xuất nó. Cảm ơn mọi người đã thử nghiệm.

## Tính năng mới (New features)

- **TESS, chuỗi xử lý 64 bit**: toàn bộ quá trình xử lý chạy bằng dấu phẩy động 64 bit từ đầu đến cuối, chỉ lượng tử hóa một lần ở cuối cùng.
- **Cùng một đường đi cho mọi ngõ ra**: ngõ ra cục bộ, UPnP/DLNA, AirPlay, Devialet AIR và OAAT đều đi qua TESS; ngõ ra nào không nhận được một bước xử lý sẽ nói rõ điều đó.
- **Bộ cân bằng tham số** với các dải tự do (tần số, độ lợi, Q), bên cạnh bộ cân bằng đồ họa và bộ cân bằng tai nghe (hồ sơ AutoEq).
- **Tích chập (convolution)**: bộ lọc dạng .wav, tệp nén .zip hoặc cấu hình Convolver (.cfg), mỗi tần số lấy mẫu một bộ lọc, chuyển đổi không gián đoạn và độ trễ được thông báo.
- **Hiệu chỉnh phòng bằng đo đạc**: bộ lọc FIR pha hỗn hợp được tính từ nhiều vị trí nghe.
- **Upsampling lên DSD** (DSD64, DSD128, DSD256) bằng bộ điều chế sigma-delta.
- **Cấu hình chuỗi đặt sẵn**: lưu một chuỗi hoàn chỉnh và gọi lại ngay hoặc ở lần phát tiếp theo.
- **Đường tín hiệu**: từ thanh phát, từng bước một, những gì thực sự được áp dụng và lý do một bước không được áp dụng.
- **Plugin gốc** trong chuỗi: bật khi đang chạy, định dạng 64 bit, giữ đúng thứ tự và số kênh.
- **Giao diện 20 ngôn ngữ**: thêm tiếng Hà Lan, tiếng Trung phồn thể, tiếng Nga và tiếng Ukraina.
- **Xác thực**: nút "Tắt xác thực" trong Cài đặt › Hệ thống › Xác thực.

## Cải tiến (Improvements)

- **ISO SACD**: DSD gốc được gửi đến các trình phát mạng dưới dạng DSF ảo; DSD truyền qua DoP được báo là DSD.
- **Nghe từ xa**: trên 5G, bản nhạc đầu tiên được gửi ở định dạng FLAC.
- **Thiết bị vắng mặt**: tự động bị quên sau 30 ngày (có thể điều chỉnh), không bao giờ xóa vùng.
- **Cover+**: ảnh bìa tốt hơn, kể cả cho radio internet (siêu dữ liệu ICY).
- **Phát theo nhóm**: độ lệch điều chỉnh được cho từng vùng; "Trước" trên vùng dẫn cũng khởi động lại các vùng theo sau.
- **Thư viện và streaming**: các lượt chạy nền không còn chặn giao diện, giữ thứ tự album theo bảng chữ cái, nghệ sĩ tương tự từ trang nghệ sĩ của một dịch vụ.
- **Thông báo từ chối** ngắn gọn hơn và chỉ đến đúng cài đặt.

## Sửa lỗi (Fixes)

- **Các bản sửa của rc13.1 và rc13.2** đã có: đăng nhập tài khoản quản trị, liên kết tài khoản mozaiklabs.fr từ mạng cục bộ, ảnh bìa khi bật xác thực, cập nhật trên Windows.
- **UPnP**: Tune không còn tự lập chỉ mục chính nó; một bản nhạc đã lập chỉ mục được phát từ địa chỉ hiện tại của máy chủ.
- **Phát nhạc**: tải trước 30 giây mà không bỏ bộ đệm; một lần tua bị nguồn từ chối không còn làm lệch luồng.
- **Chromecast** kết nối lại; việc công bố lại AirPlay không còn làm gián đoạn phát nhạc.
- **Podcast**: các tập không khả dụng được báo đúng cách.
- **Phản hồi từ diễn đàn**: lịch sử, Qobuz, Spotify, nút "Quay lại", tìm kiếm và iPad.

## Thay đổi về hoạt động

- **Bộ cân bằng tham số**: miễn phí đến rc13, nay thuộc Tune Premium, giống như tích chập, hiệu chỉnh phòng, declick thời gian thực và plugin của bên thứ ba. Bộ cân bằng đồ họa, bộ cân bằng tai nghe và crossfeed vẫn miễn phí.

## Hãy kiểm tra tại nhà

- **Một chuỗi TESS hoàn chỉnh** (bộ cân bằng, crossfeed, tích chập hoặc hiệu chỉnh phòng): đường tín hiệu có khớp với những gì bạn nghe không?
- **Nhập một bộ tích chập** và đổi bộ lọc trong khi phát.
- **Upsampling lên DSD** trên DAC tương thích: độ ổn định, mức âm, không có tiếng lách cách.
- **Cấu hình chuỗi đặt sẵn**, ở cả hai chế độ áp dụng.
- **Một ngõ ra mạng** (UPnP, AirPlay hoặc OAAT) với xử lý đang bật.
- **Cập nhật từ rc13.x** và nút "Tắt xác thực".

## Vấn đề đã biết

- **SACD nén DST**: các ảnh ISO này chưa được phát trực tiếp.
- **Đồng hồ VU** trên bản MP3, AAC hoặc Ogg được chuyển tiếp: sau khi tua, chúng tắt đến hết bản nhạc. Âm thanh không bị ảnh hưởng.
- **Nghe từ xa trên CarPlay**: bản sửa bộ chuyển tiếp được dời sang rc15.
- **Máy chủ không màn hình khi tắt xác thực**: để đổi tài khoản mozaiklabs.fr đã liên kết, hãy bật xác thực hoặc dùng đường hầm SSH đến localhost:8888.
