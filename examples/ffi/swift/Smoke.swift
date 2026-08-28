import Foundation

@main
struct Smoke {
    static func main() throws {
        if CommandLine.arguments.count == 3, CommandLine.arguments[1] == "--load-only" {
            let runtime = try RingsRuntime(libraryPath: CommandLine.arguments[2])
            runtime.initializeLogging()
            print("Swift loaded and called the real Rings C ABI")
            return
        }
        guard CommandLine.arguments.count == 2 else {
            throw RingsFfiError.loadLibrary(
                "usage: rings-swift-smoke [--load-only] LIBRARY"
            )
        }
        let runtime = try RingsRuntime(libraryPath: CommandLine.arguments[1])
        runtime.initializeLogging()
        let first = try RingsProvider(runtime: runtime, account: "fixture-one") { message in
            Data(repeating: UInt8(message.count), count: ringsSignatureLength)
        }
        let second = try RingsProvider(runtime: runtime, account: "fixture-two") { _ in
            Data(repeating: 7, count: ringsSignatureLength)
        }
        first.listen()
        second.listen()
        let response = try first.request(method: "nodeInfo", json: "{}")
        guard response == "{\"ok\":true}" else {
            throw RingsFfiError.request("nodeInfo smoke assertion")
        }
        guard try first.request(method: "signerByte", json: "{}") == "{\"byte\":17}" else {
            throw RingsFfiError.request("first signer callback assertion")
        }
        guard try second.request(method: "signerByte", json: "{}") == "{\"byte\":7}" else {
            throw RingsFfiError.request("second signer callback assertion")
        }
        guard try first.nodeDid() == "fixture-one", try second.nodeDid() == "fixture-two" else {
            throw RingsFfiError.invalidResponse("fixture node identity")
        }
        try first.connect(to: second, timeoutSeconds: 0.25, pollSeconds: 0.01)
        guard try first.peerIsConnected("fixture-two"),
              try second.peerIsConnected("fixture-one") else {
            throw RingsFfiError.invalidResponse("fixture peer connection")
        }
        guard try first.sendE2eHandshake(destinationDid: "fixture-two")
            == "fixture-transaction" else {
            throw RingsFfiError.invalidResponse("fixture E2E handshake")
        }
        let stream = try first.sendE2eMessage(
            destinationDid: "fixture-two",
            recipientPublicKey: "fixture-public-key",
            plaintext: Data("fixture-body".utf8)
        )
        guard stream == "fixture-stream",
              try second.waitForE2eStream(
                  stream,
                  timeoutSeconds: 0.25,
                  pollSeconds: 0.01
              ).count == 1 else {
            throw RingsFfiError.invalidResponse("fixture E2E stream")
        }
        first.close()
        second.close()
        print("Swift Rings FFI smoke test passed")
    }
}
