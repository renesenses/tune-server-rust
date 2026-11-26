Tune 1.0.0-rc13.91: **bản phát hành trước (pre-release) thứ hai của rc14**, chỉ dành cho người thử nghiệm. Đây không phải là phiên bản đã phát hành: bản cập nhật tự động không đề xuất nó. Bản này bao gồm toàn bộ nội dung của rc13.90. Cảm ơn mọi người đã thử nghiệm.

## Tính năng mới (New features)

- **Các bộ cân bằng ghép nối tiếp được**: bộ cân bằng đồ họa, bộ cân bằng tham số, bộ cân bằng tai nghe và hiệu chỉnh phòng nối tiếp nhau trong cùng một chuỗi TESS; áp dụng hiệu chỉnh phòng không còn ghi đè các bộ cân bằng khác. Dải độ lợi của bộ cân bằng tham số được mở rộng.
- **Bypass all trọn vẹn**: chỉ một thao tác là đưa toàn bộ chuỗi TESS ra khỏi mạch, rồi khôi phục y như cũ.
- **Plugin Tai nghe** lấy dữ liệu từ cơ sở dữ liệu OPRA: hồ sơ tai nghe của bạn được nhận diện, kèm tiền khuếch đại (preamp) và mức âm.
- **Plugin Speaker Setup** (thiết lập loa): khoảng cách, độ lợi và pha, cho từng kênh.
- **Upsampling lên DSD trên ngõ ra cục bộ**: một bộ điều chế sigma-delta riêng, và khả năng DSD của ngõ ra được công bố.
- **Chọn đích upsampling**: trong TESS, một màn hình cho phép chọn upsampling lên PCM hay lên DSD.
- **Kiểm tra mức micro** trước khi đo phòng.
- **Đo ở tần số lấy mẫu gốc của micro**, từ 44,1 đến 192 kHz.
- **Chỉnh TESS tức thì**: tắt một bước có hiệu lực ngay, và tiền khuếch đại luôn được giới hạn.

## Cải tiến (Improvements)

- **Hiệu chỉnh phòng**: lần đo kiểm tra nghe được hiệu chỉnh của vùng; các sai lệch chưa được hiệu chỉnh được giải thích, và giới hạn trên điều chỉnh được đến 1 kHz.
- **Hiệu chỉnh phòng**: dữ liệu đã nhập, lần đo gần nhất và hiệu chỉnh đang áp dụng được giữ lại khi quay lại màn hình; hướng dẫn đặt micro và khung "Trước khi đo" giúp bạn thực hiện phép đo.
- **Đo ở chế độ PURE**: âm lượng được đặt cố định ở 100 % trong lúc quét, rồi trả lại như cũ.
- **Đường tín hiệu**: một bước bị tắt (upsampling, tích chập) cho biết lý do, trong TESS và trên trang plugin; trên Windows ở chế độ chia sẻ, upsampling nhắm đến tần số của bộ trộn.
- **Thông tin ghi công**: hiển thị nhạc cụ thay vì "Nhạc công", tác giả lời tách riêng khỏi nhà soạn nhạc, mỗi vai trò một tên.
- **Quét**: mã vạch có trong thẻ sẽ bổ sung cho album nếu album chưa có.
- **Trạng thái máy chủ**: hiển thị cảnh báo nếu không tạo được bộ nhớ đệm ảnh bìa.

## Sửa lỗi (Fixes)

- **Hiệu chuẩn**: lần quét không còn dừng trước độ dài đã báo và không còn bị kẹt trong hàng đợi; một vùng không có ngõ ra bị từ chối rõ ràng.
- **Hiệu chuẩn**: một đỉnh chạm trần không còn chặn các dải tiếp theo.
- **RT60**: hiển thị "không đo được (nhiễu)" thay vì một giá trị sai; thông báo lỗi hiển thị đủ để đọc ít nhất 10 giây.
- **Cảnh báo tần số lấy mẫu**: chỉ còn xuất hiện khi nằm ngoài các tần số được chấp nhận.
- **Radio**: việc đổi đài không còn bị đài cũ ghi đè.
- **Ảnh bìa**: trên bản cài đặt mới, bộ nhớ đệm được tạo trong thư mục dữ liệu.
- **TESS**: chỉ còn một thông báo "bản tiếp theo", và phần trợ giúp của nút "Áp dụng" đã được sửa.

## Thay đổi về hoạt động

- **Thay đổi chuỗi TESS trong khi phát**: một bước bị tắt sẽ ngừng ngay lập tức; một bước được thêm hoặc bật lại sẽ nghe được từ lần phát tiếp theo.

## Hãy kiểm tra tại nhà

Các điểm này chưa được thử nghiệm trên phần cứng thực:

- **Upsampling lên DSD trên một DAC DSD thực**: độ ổn định, mức âm, không có tiếng lách cách.
- **Đo phòng ở 44,1 kHz và 96 kHz** trên một giao diện âm thanh thực, cùng với kiểm tra mức micro.
- **Trên Windows, với WASAPI chia sẻ**: phát nhạc và upsampling.
- **Speaker Setup trên một ngõ ra mạng** (UPnP, AirPlay hoặc OAAT): khoảng cách, độ lợi và pha.

Ngoài ra:

- **Nhiều bộ cân bằng nối tiếp** cùng Bypass all: đường tín hiệu có khớp với những gì bạn nghe không?
- **Plugin Tai nghe** với chính tai nghe của bạn.
- **Cập nhật từ rc13.90.**

## Vấn đề đã biết

- **SACD nén DST**: các ảnh ISO này chưa được phát trực tiếp.
- **Đồng hồ VU** trên bản MP3, AAC hoặc Ogg được chuyển tiếp: sau khi tua, chúng tắt đến hết bản nhạc. Âm thanh không bị ảnh hưởng.
- **Nghe từ xa trên CarPlay**: bản sửa bộ chuyển tiếp được dời sang rc15.
- **Máy chủ không màn hình khi tắt xác thực**: để đổi tài khoản mozaiklabs.fr đã liên kết, hãy bật xác thực hoặc dùng đường hầm SSH đến localhost:8888.
