import Foundation

// Compiled only with -D JSON_FRAMING_TESTS alongside the actual worker sources.
// The parent test parses emitted stdout, so this also exercises production emit.
func jsonFramingSelfTest() {
    let body = Data(repeating: 0xff, count: 32768)
    let encoded = body.base64EncodedString()
    let request: [String: Any] = ["id": "boundary", "method": "file_upload_write",
        "owner": String(repeating: "a", count: 64), "token": String(repeating: "b", count: 32),
        "offset": 8355840, "data": encoded]
    guard let line = guestRequestData(request) else { fatalError("32 KiB request rejected") }
    precondition(line.count < 65536 && line.last == 10)
    let decoded = try! JSONSerialization.jsonObject(with: line) as! [String: Any]
    precondition(Data(base64Encoded: decoded["data"] as! String) == body)
    for size in [65534, 65535, 65536] {
        let value: [String: Any] = ["data": String(repeating: "x", count: size - 11)]
        precondition(try! JSONSerialization.data(withJSONObject: value).count == size)
        let output = guestRequestData(value)
        if size < 65536 { precondition(output?.count == size + 1) }
        else { precondition(output == nil) }
    }
    emit(["event": "framing-test", "payload_bytes": body.count,
          "request_line_bytes": line.count, "data": encoded])
}
