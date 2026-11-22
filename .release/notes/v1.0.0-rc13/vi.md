Tune 1.0.0-rc13, bản ứng viên phát hành thứ mười ba của Tune 1.0. Cảm ơn Gros Bidon, r5yk5, Tades, cpadiou77, Pierre, Daniel LEVY, Lio, sashasha, ajm, Jean Valjean, Semakawa, Jon Rotaetxe, hifi25nl, aqs, Christopher J, FabienM, Begastor và Régis vì những báo cáo và thử nghiệm, cảm ơn alec_eiffel vì Smart Upsampler, cometdom vì những đóng góp cho Qobuz, và Levente vì phần thiết kế.

## Tính năng mới (New features)

- **Bộ tăng tần số lấy mẫu**: Smart Upsampler của Rémi (alec_eiffel) phát ở cùng mức như khi không dùng nó, và thay đổi thiết lập được nghe ngay.
- **Phát nhạc và đầu ra**: Sendspin và Devialet AIR nhận âm thanh đã qua xử lý (bộ cân bằng, plugin, bộ tăng tần số lấy mẫu). Hẹn giờ tắt có thể tắt máy tính; trên Linux, Tune xuất hiện trong các điều khiển đa phương tiện của màn hình nền.
- **Crossfeed**: miễn phí, cùng với các thiết lập và cài đặt sẵn của nó.
- **Tích chập**: một tệp đáp ứng xung cho mỗi tần số lấy mẫu.
- **DSD**: DSD từ một máy chủ UPnP được phát gốc trên các trình phát mạng khó tính, và hai bài DSD nối tiếp nhau không có khoảng lặng.
- **Thư viện**: các thẻ cá nhân trong tệp trở thành thẻ Tune; chọn nhiều album cho một thao tác hàng loạt; bộ lọc theo nhạc cụ.
- **Nguồn bị biến đổi** thay cho “Hi-Res đáng ngờ”, cho mọi định dạng, kèm tần số cắt đo được.
- **Dạng sóng** mảnh hơn, cũng có cho các bài CUE, Qobuz và TIDAL (tùy chọn).
- **Hàng đợi**: các album có thể thu gọn và di chuyển bằng một thao tác.
- **Trang chủ**: 5 danh sách phát “Vì bạn thích…” và các widget Nghệ sĩ phát nhiều nhất, Nghe thường xuyên, Nghe sau và Tiếp tục nghe.
- **Dịch vụ**: trang “Dành cho bạn” của TIDAL; Qobuz đề xuất 200 bản phát hành mới cho mỗi thể loại.
- **Tune Circle**: các thành viên của một vòng nhìn thấy nhau nếu họ đồng ý, và chế độ xem Thế giới hiển thị mọi vòng Tune.
- **Giao diện web**: tiếng Thổ Nhĩ Kỳ, xác thực bật được từ giao diện, và lịch sử theo bài hoặc theo lượt nghe.
- **Tune Client (ứng dụng máy tính)**: tự tìm các máy chủ Tune trong mạng và điều khiển máy chủ cục bộ.
- **Windows**: trình cài đặt đề xuất khởi động Tune cùng Windows.
- **Ứng dụng iOS**: CarPlay qua 5G đầy đủ hơn, thanh bên trở lại trên iPad.

## Cải tiến (Improvements)

- **Chuyển bài**: ReplayGain “Tự động” giữ mức khuếch đại album giữa hai bài của cùng một album; AAC, Opus và các bài CUE nối tiếp không khoảng lặng.
- **Phát nhạc và đầu ra**: AirPlay 1 và 2 dùng chung một đường cong âm lượng; một đài radio khởi động nhanh hơn trên trình phát mạng.
- **Windows**: ở chế độ độc quyền WASAPI, Tune chuyển sang họ tần số còn lại thay vì dừng bài.
- **Devialet AIR**: âm lượng theo bước 1 dB, lên đến 192 kHz.
- **DSD**: việc nhận diện DSD gốc thận trọng hơn và được đánh giá lại.
- **Thư viện lớn**: màn hình Thư mục mở ra không cần chờ, các tác vụ nền không còn chặn phần còn lại.
- **Dịch vụ**: ứng dụng đăng nhập Google riêng của Tune cho YouTube; Spotify và TIDAL ổn định hơn khi đang phát.
- **Giao diện web**: bộ cân bằng tham số chính xác hơn và có xác nhận trước khi xóa.
- **Ứng dụng iOS**: màn hình khóa điều khiển được vùng ngay cả khi vùng không phát; khi không có DAC, thanh trượt điều chỉnh âm lượng của iOS.
- **Cập nhật và chẩn đoán**: bản cập nhật cũng thay thế các chương trình đi kèm Tune; báo cáo lỗi ẩn các tên.

## Sửa lỗi (Fixes)

- **Phát nhạc và đầu ra**: tiếp tục phát hoạt động khi một lần tạm dừng đã giải phóng DAC; “Lặp lại tất cả” không còn nhầm một lần dừng thoáng qua với kết thúc bài; trên Windows, DAC được bật sau Tune được tìm thấy.
- **Vùng**: một thiết bị đã liên kết sẽ nêu tên vùng của nó; với OAAT, tiếp tục phát sẽ mở lại phiên thay vì im lặng.
- **DLNA, UPnP và OpenHome**: không còn lệnh Dừng thừa trên Musicbook Source, tắt tiếng ổn định, Linn Klimax DS/2 được nhận diện.
- **AirPlay**: đầu ra AirPlay 1 không còn khởi động ở mức tối đa sau khi Tune khởi động lại.
- **Đường tín hiệu**: không còn hiển thị một xử lý không diễn ra.
- **Thư viện**: các ấn bản DSD và CD của cùng một album không còn bị gộp lại; việc ghi DR không còn làm đơ máy chủ.
- **Dịch vụ**: mục yêu thích Qobuz và TIDAL hiển thị đúng thời lượng; các mục Spotify không còn bị lặp.
- **Radio**: các đài FIP có lại logo.
- **Plugin và crossfeed**: một plugin được cài có tác dụng không cần khởi động lại; crossfeed không còn bị khóa trên OAAT và trong trình duyệt.
- **Giao diện web**: lưới cuộn được đến cuối; biểu ngữ “máy chủ không phản hồi” biến mất ngay khi máy chủ phản hồi.

## Bảo mật

- Nhiều biện pháp tăng cường bảo mật.
- Nhật ký và báo cáo lỗi giờ đây ẩn thông tin đăng nhập có trong một số địa chỉ luồng.
- Thay đường dẫn (Cài đặt › thư mục nhạc) giờ kiểm tra các thư mục và không còn truy cập được từ thiết bị khác khi chưa xác thực.

## Thay đổi về hoạt động

- **Tune Circle**: khi bật xác thực, chỉ dành cho quản trị viên; chỉ đọc khi nghe từ xa.
- **YouTube**: các tài khoản đã kết nối trước phiên bản này cần được kết nối lại một lần.
- **Radio khi hết hàng đợi**: lại tắt theo mặc định; nếu bạn đã chọn nó, hãy đánh dấu lại trong vùng.
- **Bộ cân bằng**: khi tắt bộ cân bằng, bộ tiền khuếch đại vẫn được áp dụng cho đến khi nhấn “Khôi phục mức”.
- **Multiroom**: một nhóm miễn phí đến 3 vùng; trên 3 cần có Premium. Các cặp stereo vẫn miễn phí.
- **CUE có INDEX 02**: điểm bắt đầu của các bài được sửa ở lần quét tiếp theo, lịch sử được giữ nguyên.
- **Tune OS**: mật khẩu tạm thời hiển thị trên màn hình của máy hoặc trên thẻ khởi động, không còn trên trang đăng nhập; truy cập từ xa vẫn đóng cho đến khi mật khẩu được đổi.
- **Hồ sơ và lịch sử**: khi bật xác thực, chỉ quản trị viên quản lý các hồ sơ khác.
- **Thay đường dẫn**: khi không có xác thực, thực hiện từ máy đang chạy Tune.

## Hãy kiểm tra tại nhà

Những thay đổi này đã qua các bài kiểm thử tự động nhưng chưa được thử trên phần cứng thật. Hãy cho chúng tôi biết những gì bạn thấy:

- **Sendspin và Devialet AIR** khi có một xử lý đang bật.
- **Windows**: trình phát mini, chế độ độc quyền WASAPI, khởi động cùng Windows.
- **AirPlay 1**: mức khi khởi động và giữa các bài.
- **DSD tới trình phát mạng** (Atoll ST200, Eversolo).
- **Linn Klimax DS/2** ở chế độ OpenHome, **Tune Client trên macOS** và **CarPlay qua 5G**.

## Vấn đề đã biết

- **DSD qua DoP trên đầu ra cục bộ**: sau PCM đã được giảm mức, có thể phát ở mức toàn thang. Hãy giữ âm lượng ampli ở mức thấp.
- **UPnP sau khi đổi địa chỉ**: một bài có thể không phát được; hãy lập chỉ mục lại máy chủ đó.
- **Kiểm tra toàn vẹn**: một vài tệp vẫn có thể bị báo nhầm; đừng xóa gì chỉ dựa vào kết luận này.
- **Windows ASIO do một máy chủ khác điều khiển**: sau khi Tạm dừng rồi Phát, không còn tiếng; hãy chuyển sang bài tiếp theo.
- **Sau khi cập nhật**: nếu giao diện không thay đổi, hãy buộc tải lại (Cmd+Shift+R hoặc Ctrl+Shift+R).
