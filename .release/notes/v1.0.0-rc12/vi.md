Tune 1.0.0-rc12, bản ứng viên phát hành thứ mười hai của Tune 1.0. Nhiều màn hình có giao diện mới do Levente, một nhà thiết kế trong cộng đồng Tune, vẽ.

- **Giao diện mới**: Tune Circle được thiết kế lại (Khám phá, Tin nhắn, Danh sách phát chia sẻ, Vòng tròn của tôi); trang album có mô tả, người tham gia thực hiện, chi tiết và thẻ Phiên bản kèm số danh mục; trang nghệ sĩ có ảnh chân dung lớn, các con số và các thẻ; Cài đặt trong một cột dễ đọc hơn; Trạng thái máy chủ và bảng điều khiển lượt nghe, có chọn khoảng thời gian.
- **Trang chủ**: widget mới “Tổng quan lượt nghe (bố cục mới)”, thêm từ Trang chủ › Sửa, với một bảng Vùng; ô lớn hơn kèm năm và thể loại.
- **Thư viện**: ghim các bộ lọc bạn chọn bằng nút Bộ lọc; nhạc cổ điển với Nhạc trưởng, Hòa tấu / dàn nhạc và Tác phẩm; số kênh và bộ lọc Stereo; bản đồ và bộ lọc theo quốc gia của nghệ sĩ; “Thay đường dẫn” cho một thư mục nhạc; phát hiện Hi-Res giả, tắt theo mặc định.
- **Thông số kỹ thuật của bài**: phổ tần và dạng sóng theo từng kênh, cả cho các bài Qobuz và TIDAL trong thư viện.
- **Radio khi hết hàng đợi**: khi hàng đợi kết thúc, Tune tiếp tục với các bài tương tự, được đánh dấu “Radio”; điều chỉnh được theo từng vùng.
- **Hẹn giờ tắt**: nhạc dừng vào giờ đã chọn; trên Tune OS, máy cũng có thể tắt.
- **Danh sách phát**: số lượt nghe và lần nghe gần nhất trên mỗi dòng; dấu X Gỡ bỏ và “Xóa khỏi danh sách phát này”; gỡ một bài khỏi danh sách phát Qobuz của chính bạn.
- **Radio**: các bộ lọc Lossless, Chất lượng cao và Hi-Res trên một dòng; một đài trong danh bạ có thể được chỉnh sửa riêng cho bạn; một đài radio có thể được thêm vào danh sách phát.
- **Phát nhạc**: DSD gốc trên đầu ra cục bộ Linux (ALSA); tần số PCM cố định theo vùng; chế độ độc quyền thiết lập theo từng vùng; vùng đã xóa có thể khôi phục; kiểm tra AccurateRip thực sự cho CD.
- **DLNA**: luồng liên tục giữa các bài cho những renderer nối hai bài kém, tắt theo mặc định; âm lượng chính của một nhóm điều chỉnh được từ bất kỳ thành viên nào.
- **Streaming**: chọn quốc gia của danh mục TIDAL; mục mới phát hành của Qobuz giống hệt trình phát Qobuz; Spotify có trợ lý kết nối, và Spotify Connect phát trong vùng đã chọn.
- **Đường tín hiệu**: nhấp vào một khâu xử lý để mở thiết lập của nó; phát theo nhóm hiển thị từng đầu ra.
- **Cài đặt và giao diện**: tìm theo thiết lập, cỡ chữ, thanh tiến trình dạng sóng, một ô tìm kiếm duy nhất trên mọi màn hình, nhập bằng giọng nói trong trợ lý.
- **Tốc độ**: bộ sưu tập thông minh và nhảy theo chữ cái nhanh hơn nhiều trên thư viện lớn; danh sách lớn không còn làm chậm máy chủ.
- **Bộ nhớ**: Tune trả lại bộ nhớ đã giải phóng trong lúc nghe lâu; bộ nhớ dành cho chuyển mã được giới hạn.
- **Bắt đầu phát**: YouTube bắt đầu nhanh hơn; TIDAL không còn chờ tải xong toàn bộ bài trên đầu ra cục bộ hoặc OAAT.
- **Thống kê**: một tổng số bài và album duy nhất trên mọi màn hình, một mục cho mỗi codec, tìm kiếm trong toàn bộ bảng xếp hạng.
- **Tune Circle**: emoji trong Tin nhắn; khi việc nghe tại một liên hệ bị đóng, Tune tìm cùng bài đó trong các nguồn của bạn.
- **Ứng dụng di động**: CarPlay phát trực tiếp trên điện thoại, với Bộ sưu tập và một thẻ Radio; Android có thêm danh mục TIDAL, lịch sử radio và podcast, và widget Radio yêu thích.
- **Windows**: một biểu ngữ cảnh báo khi tường lửa chặn Tune; huy hiệu WASAPI hoặc ASIO trên các vùng cục bộ.
- **Docker**: trợ lý chia sẻ SMB hoạt động trong image; ổ CD dùng được trong container.
- **Bản dịch**: số nhiều chính xác trong cả 15 ngôn ngữ.
- **Các thành phần phụ thuộc** được cập nhật, trong đó có SQLite 3.54 đi kèm.
- **Sửa lỗi giao diện web**: không còn màn hình đen sau khi cập nhật giao diện.
- **Sửa lỗi tắt tiếng**: một vùng đang tắt tiếng vẫn im lặng khi một bài mới bắt đầu; báo thức hoặc giới hạn âm lượng không còn bỏ tắt tiếng.
- **Sửa lỗi phát nhạc**: một tệp bị hỏng không còn làm đơ vùng, Tune chuyển sang phần tiếp theo; một tập podcast đã nghe xong không còn mở lại ở cuối.
- **Sửa lỗi AirPlay 2**: phát nhạc tiếp tục từ chỗ đã dừng sau khi tạm dừng, và bắt đầu ở âm lượng của vùng, không bao giờ ở mức tối đa.
- **Sửa lỗi DLNA và nhóm**: phát nhạc tiếp tục sau lỗi 701; bài tiếp theo được chuẩn bị trước; một thiết bị không phản hồi không còn chặn các vùng khác; Sonos mang tên model của nó.
- **Sửa lỗi HQPlayer, Devialet và Diretta**: theo dõi vị trí và trạng thái, tự động kết nối lại; ampli Devialet phản hồi trở lại; không còn thanh trượt âm lượng giả trên Diretta.
- **Sửa lỗi OAAT**: DSD gốc được hiển thị đúng và bit-perfect; nhận diện DSD1024, chuyển về PCM trên bộ thu có giới hạn.
- **Sửa lỗi radio**: không còn đài trùng lặp; một đài bị từ chối ở quốc gia của bạn chuyển sang luồng tiếp theo.
- **Sửa lỗi Spotify, Qobuz và TIDAL**: danh sách phát và mục yêu thích Spotify theo API mới; việc Qobuz từ chối không còn bị hiển thị là sai thông tin đăng nhập; tải trước TIDAL không còn làm chậm việc phát.
- **Sửa lỗi thư viện**: lượt nghe được tính theo bài; dấu gạch nối không còn tạo nghệ sĩ giả; các đĩa trong một bộ hộp được nhận diện tốt hơn; một bài không có năm lấy năm của album.
- **Sửa lỗi Windows**: DAC vắng mặt không còn đưa âm thanh ra loa của PC; việc theo dõi thư mục bao gồm cả các thư mục chia sẻ mạng.
- **Sửa lỗi PostgreSQL 18**: mục yêu thích UPnP hoạt động trở lại.

## Bảo mật

- Quản trị Tune từ một máy khác trong mạng nay yêu cầu đăng nhập (Tune OS: từ ảnh hệ thống mới).
- Webhook không còn có thể nhắm tới chính máy chạy Tune.
- Cấu hình Home Assistant và Hue cần quyền quản trị hoặc phải thực hiện ngay trên máy chạy Tune; Home Assistant cài trên cùng máy với Tune không còn được hỗ trợ.

## Vấn đề đã biết

- **DSD qua DoP trên đầu ra cục bộ**: có thể phát ở mức toàn thang sau PCM đã được giảm mức, bỏ qua âm lượng. Hãy giữ âm lượng ampli ở mức thấp khi chuyển từ một bài PCM sang một bài DSD qua DoP (phiên bản tiếp theo).
- **Kiểm tra toàn vẹn**: nhiều tệp FLAC có thể bị báo nhầm là hỏng; đừng xóa tệp nào chỉ dựa vào kết luận này.
- **Windows ASIO do một máy chủ khác điều khiển**: mất tiếng sau khi Tạm dừng rồi Phát; hãy chuyển sang bài tiếp theo.
- **Sau khi cập nhật**: nếu giao diện không thay đổi, hãy buộc tải lại (Cmd+Shift+R hoặc Ctrl+Shift+R). Lần khởi động đầu tiên lâu hơn, trong lúc tạo hai chỉ mục.
