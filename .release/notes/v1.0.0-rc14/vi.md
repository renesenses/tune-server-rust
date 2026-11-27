Tune 1.0.0-rc14 bao gồm toàn bộ nội dung của các bản phát hành trước rc13.90 và rc13.91; ghi chú này mô tả những gì đã thay đổi kể từ rc13.91. Cảm ơn những người thử nghiệm: nhiều điểm dưới đây đến từ phản hồi của bạn.

## Tính năng mới (New features)

- **TESS, lưu và tải lại cài đặt sẵn**: ngay khi một thiết lập khác với cài đặt sẵn đã chọn, TESS hiển thị "đã sửa". **Lưu** ghi đè cài đặt sẵn gốc, **Tải lại** quay về cài đặt sẵn đó. Cài đặt sẵn đi kèm Tune, như "Pure", không thể bị ghi đè: "Lưu thành…" tạo một bản sao.
- **TESS, cài đặt sẵn DSD áp dụng ngay**: một cài đặt sẵn thay đổi đích DSD (ví dụ DSD128 → DSD256), được tải ở chế độ "áp dụng ngay", sẽ phát lại bản nhạc từ vị trí hiện tại thay vì chờ bản tiếp theo. Upsampling là tính năng Premium.
- **Khoảng dự trữ (headroom)**: Tự động, hoặc thủ công từ −12 đến 0 dB, mỗi bước 0,5 dB. Bảng hiển thị giá trị thực sự được áp dụng, hoặc cho biết lý do không áp dụng.
- **Khoảng dự trữ khi upsampling lên DSD**: khoảng dự trữ thủ công không còn bị áp dụng hai lần (trước đây −12 dB cho ra −24 dB; giờ là −15 dB, trong đó −3 dB dành cho bộ điều chế DSD, hiển thị trên một dòng riêng).
- **Đo phòng có hướng dẫn, hướng tới 75 dB(C)**: với micro đã hiệu chuẩn, màn hình hiển thị mức đo được, độ chênh so với 75 dB(C) và chỉ dẫn chỉnh âm lượng. Quy tắc này thận trọng vì phép quy đổi chưa được kiểm tra bằng thiết bị hiệu chuẩn: Tune không bao giờ khuyên vượt quá 80 dB(C) ước tính và cắt tín hiệu ở 90 dB SPL. Hiệu chỉnh đo bằng micro là Premium; hiệu chỉnh tính toán không cần micro vẫn miễn phí.
- **An toàn thính giác**: bỏ chọn một bộ cân bằng, chuyển sang PURE hoặc Bypass all, hay gỡ bỏ một bộ tích chập, crossfeed hoặc gộp mono không còn làm mức âm tăng vọt: mức âm trở về mức trước đó bằng một đoạn chuyển dần khoảng nửa giây. Nếu bạn đã tăng âm lượng trong lúc đó, đèn báo "mức âm đã giảm" sáng lên và "Khôi phục mức âm" giải phóng phần dư. Điều này cũng áp dụng cho ngõ ra mạng (UPnP, OAAT).
- **Đường tín hiệu trung thực**:
  - trên ngõ ra mạng, đường tín hiệu theo đúng thứ tự thực của luồng, cho đến âm lượng của trình phát;
  - bước của bộ lọc đáp ứng xung được gọi là "Tích chập";
  - bộ lọc giữ tên của tệp .wav hoặc .zip đã nhập;
  - sau khi đổi đích DSD, đường tín hiệu hiển thị tần số DSD đang thực sự phát, và báo trước tần số tiếp theo;
  - cửa sổ chuyển sang màu xanh lá: màu cam chỉ dành cho các cảnh báo thực sự.
- **Tích chập**: bảng hiển thị đường cong của bộ lọc đã tải, theo từng kênh, và pha khi bạn yêu cầu.
- **Bypass all**: biểu tượng TESS trên thanh phát được làm mờ khi TESS bị bỏ qua, và trang TESS hiển thị "Đã bỏ qua TESS".
- **Trang TESS đồng bộ**: màu nhấn xanh lá TESS, vùng hiện tại nhắc lại cạnh huy hiệu, bảng Headroom chuyển xám trên ngõ ra mạng, và phép đo "Không cần micro (ước tính)" hoặc "Có micro (khuyến nghị)".
- **Ngõ ra mạng**: thẻ Upsampling cho biết upsampling không áp dụng ở đó.
- **Giao diện đồng bộ**: phông chữ tích hợp sẵn, hiển thị cả khi ngoại tuyến; Thư viện, Live Radio, Máy chủ đa phương tiện và Buổi hòa nhạc theo bộ nhận diện mới.

## Sửa lỗi (Fixes)

- **Radio đến loa Sonos** (và mọi trình phát mạng có giới hạn tần số): một đài radio 192 kHz được phát ở tần số mà trình phát chấp nhận. Trước đây, trình phát dừng ngay lập tức, lặp đi lặp lại.
- **768 kHz**: danh sách "Tần số tối đa" của một vùng có 768 kHz và 1.536 kHz.
- **Thư viện**: lưới album không còn bị đứng khi cuộn qua một bộ sưu tập lớn.
- **Phát trong thẻ trình duyệt**: việc tua thực sự di chuyển vị trí phát, kể cả từ một thiết bị khác.
- **Trợ giúp**: biểu ngữ hỗ trợ ưu tiên không còn hiển thị với tài khoản Premium.
- **Giấy phép**: ngày hết hạn được đọc đúng, và Cài đặt › Giấy phép cho biết vì sao bạn đang ở cấp miễn phí.
- **Tích chập**: một bộ lọc khuếch đại một số tần số không còn gây méo do quá tải.
- **Podcast dài**: một tập mà máy chủ lưu trữ thường xuyên ngắt kết nối không còn bị gián đoạn.
- **Gói Debian trong container LXC** (Proxmox): dịch vụ vẫn khởi động được ngay cả khi không có không gian tên gắn kết riêng.
- **Lần khởi chạy đầu tiên**: trình hướng dẫn không còn gọi là "đã bật" các dịch vụ chưa có tài khoản, và lưu thư mục nhạc đã chọn. Khi "Đài của tôi" trống, danh bạ đài sẽ được mở.

## Bảo mật và tài khoản

- **Phiên được gia hạn**: với "Duy trì đăng nhập" (mặc định không được chọn), phiên trình duyệt tự gia hạn miễn là còn được dùng, tối đa 90 ngày. Nếu không, vẫn là 24 giờ như trước. Menu ảnh đại diện có mục "Đăng xuất khỏi máy chủ này".
- **Khôi phục quyền quản trị**: hãy chạy `tune-server --reset-admin` (hoặc `--reinitialiser-admin`) **trên chính máy chủ**. Mã dùng một lần mà lệnh cấp, có hiệu lực trong 15 phút, được nhập vào "Quên mật khẩu?" từ máy đó; nếu không có trình duyệt, hãy thêm `--password-stdin`. Các phiên đang mở sẽ bị đóng. Hướng dẫn cài đặt (mục 9) cung cấp lệnh cho từng hệ thống.
- **Hồ sơ `admin`**: hồ sơ quản trị được tạo mặc định giờ có tên `admin`; tên do bạn tự chọn vẫn được giữ, và `default` vẫn được chấp nhận.
- **Mật khẩu quản trị ngay từ lần thiết lập đầu tiên**: trên bản cài đặt mới, trình hướng dẫn yêu cầu bạn tạo mật khẩu này.
- **Máy chủ không bật xác thực**: việc quản trị được chấp nhận từ mạng cục bộ, chứ không còn chỉ từ chính máy chủ. **Cảnh báo**: khi không có xác thực, các thiết bị trong mạng cục bộ có thể quản trị máy chủ; hãy bật xác thực nếu mạng của bạn không đáng tin cậy. Thiết lập "Chỉ quản trị từ máy này" khôi phục quy tắc cũ.

## Ứng dụng điều khiển từ xa

- Trên máy chủ được bảo vệ, các ứng dụng chưa được cập nhật vẫn yêu cầu nhập lại mật khẩu sau mỗi 24 giờ, như trước đây.

## Hãy kiểm tra tại nhà

- **"Bypass all" trong một cài đặt sẵn**: một cài đặt sẵn thay đổi cùng lúc Bypass all và đích DSD, ở chế độ "áp dụng ngay", sẽ phát lại bản nhạc từ vị trí hiện tại. Mới chỉ được kiểm tra trên bàn thử: hãy cho chúng tôi biết việc phát lại có sạch không.
