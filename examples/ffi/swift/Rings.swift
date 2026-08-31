import Foundation

#if canImport(Darwin)
import Darwin
#elseif canImport(Glibc)
import Glibc
#endif

public let ringsSignatureLength = 65

public enum RingsLogLevel: Int32 {
    case debug = 0
    case info = 1
    case warn = 2
    case error = 3
    case trace = 4
}

public enum RingsFfiError: Error, CustomStringConvertible {
    case loadLibrary(String)
    case loadSymbol(String)
    case signerCapacity
    case signerLength(Int)
    case providerCreation
    case request(String)
    case invalidUtf8
    case invalidResponse(String)
    case timeout(String)

    public var description: String {
        switch self {
        case .loadLibrary(let message): return "failed to load Rings library: \(message)"
        case .loadSymbol(let symbol): return "Rings library does not export \(symbol)"
        case .signerCapacity: return "all Swift FFI signer callback slots are in use"
        case .signerLength(let length): return "signer returned \(length) bytes; expected 65"
        case .providerCreation: return "rings_node_new_provider_with_callback returned NULL"
        case .request(let method): return "Rings request \(method) returned NULL"
        case .invalidUtf8: return "Rings returned a non-UTF-8 response"
        case .invalidResponse(let message): return "invalid Rings response: \(message)"
        case .timeout(let message): return "Rings operation timed out: \(message)"
        }
    }
}

public typealias RingsSigner = @Sendable (Data) throws -> Data

private typealias CSigner = @convention(c) (
    UnsafePointer<CChar>?,
    UnsafeMutablePointer<CChar>?
) -> Void
private typealias InitLogging = @convention(c) (Int32) -> Void
private typealias Listen = @convention(c) (UnsafeRawPointer?) -> Void
private typealias Request = @convention(c) (
    UnsafeRawPointer?,
    UnsafePointer<CChar>?,
    UnsafePointer<CChar>?
) -> UnsafeMutablePointer<CChar>?
private typealias StringFree = @convention(c) (UnsafeMutablePointer<CChar>?) -> Void
private typealias ProviderDestroy = @convention(c) (UnsafeMutableRawPointer?) -> Void
private typealias NewProvider = @convention(c) (
    UInt32,
    UnsafePointer<CChar>?,
    UInt64,
    UnsafePointer<CChar>?,
    UnsafePointer<CChar>?,
    CSigner?
) -> UnsafeMutableRawPointer?

/// Dynamically loaded native Rings C ABI.
///
/// Dynamic loading keeps the example independent of Xcode and SwiftPM linker configuration. The
/// path normally points to `target/debug/librings_node.dylib` on macOS or the corresponding `.so`
/// on Linux.
public final class RingsRuntime {
    fileprivate let initLoggingFunction: InitLogging
    fileprivate let listenFunction: Listen
    fileprivate let requestFunction: Request
    fileprivate let stringFreeFunction: StringFree
    fileprivate let providerDestroyFunction: ProviderDestroy
    fileprivate let newProviderFunction: NewProvider
    private let library: UnsafeMutableRawPointer

    public init(libraryPath: String) throws {
        guard let library = dlopen(libraryPath, RTLD_NOW | RTLD_LOCAL) else {
            let message = dlerror().map { String(cString: $0) } ?? "unknown dlopen error"
            throw RingsFfiError.loadLibrary(message)
        }
        do {
            self.initLoggingFunction = try loadSymbol(
                library,
                "rings_node_init_logging",
                as: InitLogging.self
            )
            self.listenFunction = try loadSymbol(
                library,
                "rings_node_listen",
                as: Listen.self
            )
            self.requestFunction = try loadSymbol(
                library,
                "rings_node_request",
                as: Request.self
            )
            self.stringFreeFunction = try loadSymbol(
                library,
                "rings_node_string_free",
                as: StringFree.self
            )
            self.providerDestroyFunction = try loadSymbol(
                library,
                "rings_node_provider_destroy",
                as: ProviderDestroy.self
            )
            self.newProviderFunction = try loadSymbol(
                library,
                "rings_node_new_provider_with_callback",
                as: NewProvider.self
            )
            self.library = library
        } catch {
            dlclose(library)
            throw error
        }
    }

    deinit {
        dlclose(library)
    }

    public func initializeLogging(_ level: RingsLogLevel = .error) {
        initLoggingFunction(level.rawValue)
    }
}

/// Owned provider handle mirroring the Python example's `ProviderHandle`.
///
/// The wrapper retains both the native library and signer callback until `close()` or `deinit`.
/// Up to eight providers may be alive at once because the current C callback has no context
/// pointer; fixed C trampolines keep per-provider Swift closures distinct.
public final class RingsProvider {
    private let runtime: RingsRuntime
    private var handle: UnsafeMutableRawPointer?
    private let signerSlot: Int

    public init(
        runtime: RingsRuntime,
        account: String,
        accountType: String = "eip191",
        networkId: UInt32 = 0,
        iceServer: String = "stun://stun.l.google.com",
        stabilizeInterval: UInt64 = 10,
        signer: @escaping RingsSigner
    ) throws {
        let slot = try signerRegistry.allocate(signer)
        let callback = callbackForSlot(slot)
        let provider = iceServer.withCString { ice in
            account.withCString { account in
                accountType.withCString { accountType in
                    runtime.newProviderFunction(
                        networkId,
                        ice,
                        stabilizeInterval,
                        account,
                        accountType,
                        callback
                    )
                }
            }
        }
        guard let provider else {
            let signerFailure = signerRegistry.takeFailure(slot)
            signerRegistry.release(slot)
            throw signerFailure ?? RingsFfiError.providerCreation
        }
        self.runtime = runtime
        self.handle = provider
        self.signerSlot = slot
    }

    deinit {
        close()
    }

    public func listen() {
        runtime.listenFunction(handle)
    }

    public func request(method: String, json: String) throws -> String {
        let response = method.withCString { method in
            json.withCString { json in
                runtime.requestFunction(handle, method, json)
            }
        }
        guard let response else {
            throw signerRegistry.takeFailure(signerSlot) ?? RingsFfiError.request(method)
        }
        defer { runtime.stringFreeFunction(response) }
        guard let value = String(validatingCString: response) else {
            throw RingsFfiError.invalidUtf8
        }
        return value
    }

    public func requestJson(method: String, object: Any) throws -> Any {
        let data = try JSONSerialization.data(withJSONObject: object)
        guard let json = String(data: data, encoding: .utf8) else {
            throw RingsFfiError.invalidUtf8
        }
        let response = try request(method: method, json: json)
        guard let responseData = response.data(using: .utf8) else {
            throw RingsFfiError.invalidUtf8
        }
        return try JSONSerialization.jsonObject(with: responseData)
    }

    public func nodeDid() throws -> String {
        let result = try requestJson(method: "nodeDid", object: [:])
        guard let object = result as? [String: Any], let did = object["did"] as? String else {
            throw RingsFfiError.invalidResponse("nodeDid has no string did")
        }
        return did
    }

    public func createOffer(remoteDid: String) throws -> String {
        let result = try requestJson(method: "createOffer", object: ["did": remoteDid])
        guard let object = result as? [String: Any], let offer = object["offer"] as? String else {
            throw RingsFfiError.invalidResponse("createOffer has no string offer")
        }
        return offer
    }

    public func answerOffer(_ offer: String) throws -> String {
        let result = try requestJson(method: "answerOffer", object: ["offer": offer])
        guard let object = result as? [String: Any], let answer = object["answer"] as? String else {
            throw RingsFfiError.invalidResponse("answerOffer has no string answer")
        }
        return answer
    }

    public func acceptAnswer(_ answer: String) throws -> Any {
        try requestJson(method: "acceptAnswer", object: ["answer": answer])
    }

    public func listPeers() throws -> [[String: Any]] {
        let result = try requestJson(method: "listPeers", object: [:])
        guard let object = result as? [String: Any],
              let peers = object["peers"] as? [[String: Any]] else {
            throw RingsFfiError.invalidResponse("listPeers has no peer array")
        }
        return peers
    }

    public func peerIsConnected(_ remoteDid: String) throws -> Bool {
        try listPeers().contains { peer in
            peer["did"] as? String == remoteDid && peer["state"] as? String == "Connected"
        }
    }

    public func waitForConnectedPeer(
        _ remoteDid: String,
        timeoutSeconds: TimeInterval = 15,
        pollSeconds: TimeInterval = 0.25
    ) throws {
        try validatePolling(timeoutSeconds: timeoutSeconds, pollSeconds: pollSeconds)
        let deadline = Date().addingTimeInterval(timeoutSeconds)
        while Date() < deadline {
            if try peerIsConnected(remoteDid) {
                return
            }
            Thread.sleep(forTimeInterval: pollSeconds)
        }
        throw RingsFfiError.timeout("peer \(remoteDid) did not reach Connected")
    }

    public func connect(
        to responder: RingsProvider,
        timeoutSeconds: TimeInterval = 15,
        pollSeconds: TimeInterval = 0.25
    ) throws {
        let initiatorDid = try nodeDid()
        let responderDid = try responder.nodeDid()
        let offer = try createOffer(remoteDid: responderDid)
        let answer = try responder.answerOffer(offer)
        _ = try acceptAnswer(answer)
        try waitForConnectedPeer(
            responderDid,
            timeoutSeconds: timeoutSeconds,
            pollSeconds: pollSeconds
        )
        try responder.waitForConnectedPeer(
            initiatorDid,
            timeoutSeconds: timeoutSeconds,
            pollSeconds: pollSeconds
        )
    }

    public func takeE2eEvents() throws -> [[String: Any]] {
        let result = try requestJson(method: "takeE2eEvents", object: [:])
        guard let object = result as? [String: Any],
              let events = object["events"] as? [[String: Any]] else {
            throw RingsFfiError.invalidResponse("takeE2eEvents has no event array")
        }
        return events
    }

    public func sendE2eHandshake(destinationDid: String) throws -> String {
        let result = try requestJson(
            method: "sendE2eHandshake",
            object: ["destination_did": destinationDid]
        )
        guard let object = result as? [String: Any], let txId = object["tx_id"] as? String else {
            throw RingsFfiError.invalidResponse("sendE2eHandshake has no string tx_id")
        }
        return txId
    }

    public func sendE2eMessage(
        destinationDid: String,
        recipientPublicKey: String,
        plaintext: Data,
        maxPlaintextFrameLength: Int = 0
    ) throws -> String {
        let result = try requestJson(
            method: "sendE2eMessage",
            object: [
                "destination_did": destinationDid,
                "recipient_public_key": recipientPublicKey,
                "data": plaintext.base64EncodedString(),
                "max_plaintext_frame_len": maxPlaintextFrameLength,
            ]
        )
        guard let object = result as? [String: Any],
              let streamId = object["stream_id"] as? String else {
            throw RingsFfiError.invalidResponse("sendE2eMessage has no string stream_id")
        }
        return streamId
    }

    public func waitForE2eEvent(
        timeoutSeconds: TimeInterval = 15,
        pollSeconds: TimeInterval = 0.25,
        matching predicate: ([String: Any]) -> Bool
    ) throws -> [String: Any] {
        try validatePolling(timeoutSeconds: timeoutSeconds, pollSeconds: pollSeconds)
        let deadline = Date().addingTimeInterval(timeoutSeconds)
        while Date() < deadline {
            if let event = try takeE2eEvents().first(where: predicate) {
                return event
            }
            Thread.sleep(forTimeInterval: pollSeconds)
        }
        throw RingsFfiError.timeout("matching E2E event was not observed")
    }

    public func waitForE2eStream(
        _ streamId: String,
        timeoutSeconds: TimeInterval = 15,
        pollSeconds: TimeInterval = 0.25
    ) throws -> [[String: Any]] {
        try validatePolling(timeoutSeconds: timeoutSeconds, pollSeconds: pollSeconds)
        var frames: [[String: Any]] = []
        let deadline = Date().addingTimeInterval(timeoutSeconds)
        while Date() < deadline {
            frames.append(contentsOf: try takeE2eEvents().filter { event in
                event["kind"] as? String == "streamFrame"
                    && event["stream_id"] as? String == streamId
            })
            if frames.contains(where: { $0["is_final"] as? Bool == true }) {
                return frames
            }
            Thread.sleep(forTimeInterval: pollSeconds)
        }
        throw RingsFfiError.timeout("E2E stream \(streamId) did not reach its final frame")
    }

    private func validatePolling(timeoutSeconds: TimeInterval, pollSeconds: TimeInterval) throws {
        guard timeoutSeconds > 0, pollSeconds > 0 else {
            throw RingsFfiError.invalidResponse("poll timeout and interval must be positive")
        }
    }

    public func close() {
        guard let handle else { return }
        self.handle = nil
        runtime.providerDestroyFunction(handle)
        signerRegistry.release(signerSlot)
    }
}

private func loadSymbol<T>(
    _ library: UnsafeMutableRawPointer,
    _ name: String,
    as type: T.Type
) throws -> T {
    guard let symbol = dlsym(library, name) else {
        throw RingsFfiError.loadSymbol(name)
    }
    return unsafeBitCast(symbol, to: type)
}

private final class SignerRegistry: @unchecked Sendable {
    private let lock = NSLock()
    private var signers: [Int: RingsSigner] = [:]
    private var failures: [Int: Error] = [:]

    func allocate(_ signer: @escaping RingsSigner) throws -> Int {
        lock.lock()
        defer { lock.unlock() }
        for slot in 0..<8 where signers[slot] == nil {
            signers[slot] = signer
            return slot
        }
        throw RingsFfiError.signerCapacity
    }

    func release(_ slot: Int) {
        lock.lock()
        signers.removeValue(forKey: slot)
        failures.removeValue(forKey: slot)
        lock.unlock()
    }

    func sign(_ slot: Int, message: Data) -> Result<Data, Error> {
        lock.lock()
        let signer = signers[slot]
        lock.unlock()
        guard let signer else { return .failure(RingsFfiError.signerCapacity) }
        do {
            let signature = try signer(message)
            guard signature.count == ringsSignatureLength else {
                return .failure(RingsFfiError.signerLength(signature.count))
            }
            return .success(signature)
        } catch {
            return .failure(error)
        }
    }

    func recordFailure(_ slot: Int, _ error: Error) {
        lock.lock()
        failures[slot] = error
        lock.unlock()
    }

    func takeFailure(_ slot: Int) -> Error? {
        lock.lock()
        defer { lock.unlock() }
        return failures.removeValue(forKey: slot)
    }
}

private let signerRegistry = SignerRegistry()

private func dispatchSigner(
    slot: Int,
    message: UnsafePointer<CChar>?,
    output: UnsafeMutablePointer<CChar>?
) {
    guard let message, let output else { return }
    let input = Data(bytes: message, count: strlen(message))
    switch signerRegistry.sign(slot, message: input) {
    case .success(let signature):
        signature.withUnsafeBytes { bytes in
            if let source = bytes.baseAddress {
                memcpy(output, source, ringsSignatureLength)
            }
        }
    case .failure(let error):
        memset(output, 0, ringsSignatureLength)
        signerRegistry.recordFailure(slot, error)
    }
}

private let signerCallback0: CSigner = { dispatchSigner(slot: 0, message: $0, output: $1) }
private let signerCallback1: CSigner = { dispatchSigner(slot: 1, message: $0, output: $1) }
private let signerCallback2: CSigner = { dispatchSigner(slot: 2, message: $0, output: $1) }
private let signerCallback3: CSigner = { dispatchSigner(slot: 3, message: $0, output: $1) }
private let signerCallback4: CSigner = { dispatchSigner(slot: 4, message: $0, output: $1) }
private let signerCallback5: CSigner = { dispatchSigner(slot: 5, message: $0, output: $1) }
private let signerCallback6: CSigner = { dispatchSigner(slot: 6, message: $0, output: $1) }
private let signerCallback7: CSigner = { dispatchSigner(slot: 7, message: $0, output: $1) }

private func callbackForSlot(_ slot: Int) -> CSigner {
    switch slot {
    case 0: return signerCallback0
    case 1: return signerCallback1
    case 2: return signerCallback2
    case 3: return signerCallback3
    case 4: return signerCallback4
    case 5: return signerCallback5
    case 6: return signerCallback6
    default: return signerCallback7
    }
}
