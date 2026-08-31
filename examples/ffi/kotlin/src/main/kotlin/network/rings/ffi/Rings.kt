package network.rings.ffi

import com.sun.jna.Callback
import com.sun.jna.Library
import com.sun.jna.Native
import com.sun.jna.NativeLibrary
import com.sun.jna.Pointer
import java.io.Closeable
import java.nio.charset.StandardCharsets
import java.util.Base64
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicReference
import kotlinx.serialization.json.Json
import kotlinx.serialization.json.JsonArray
import kotlinx.serialization.json.JsonElement
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.buildJsonObject
import kotlinx.serialization.json.jsonArray
import kotlinx.serialization.json.jsonObject
import kotlinx.serialization.json.jsonPrimitive
import kotlinx.serialization.json.put

const val RINGS_SIGNATURE_LENGTH: Int = 65

enum class RingsLogLevel(val abiValue: Int) {
    DEBUG(0),
    INFO(1),
    WARN(2),
    ERROR(3),
    TRACE(4),
}

fun interface RingsSigner {
    fun sign(message: ByteArray): ByteArray
}

internal fun interface SignerCallback : Callback {
    fun invoke(message: Pointer?, output: Pointer?)
}

internal interface NativeRings : Library {
    fun rings_node_init_logging(level: Int)
    fun rings_node_listen(provider: Pointer?)
    fun rings_node_request(provider: Pointer?, method: String, params: String): Pointer?
    fun rings_node_string_free(value: Pointer?)
    fun rings_node_provider_destroy(provider: Pointer?)
    fun rings_node_new_provider_with_callback(
        networkId: Int,
        iceServer: String,
        stabilizeInterval: Long,
        account: String,
        accountType: String,
        signer: SignerCallback,
    ): Pointer?
}

class RingsFfiException(message: String, cause: Throwable? = null) : RuntimeException(message, cause)

/** Dynamically loaded desktop Rings C ABI. */
class RingsRuntime private constructor(private val native: NativeRings) {
    companion object {
        private val requiredSymbols = listOf(
            "rings_node_init_logging",
            "rings_node_listen",
            "rings_node_request",
            "rings_node_string_free",
            "rings_node_provider_destroy",
            "rings_node_new_provider_with_callback",
        )

        fun load(libraryPath: String): RingsRuntime {
            val library = NativeLibrary.getInstance(libraryPath)
            requiredSymbols.forEach(library::getFunction)
            return RingsRuntime(Native.load(libraryPath, NativeRings::class.java))
        }
    }

    fun initializeLogging(level: RingsLogLevel = RingsLogLevel.ERROR) {
        native.rings_node_init_logging(level.abiValue)
    }

    fun createProvider(
        account: String,
        signer: RingsSigner,
        accountType: String = "eip191",
        networkId: Int = 0,
        iceServer: String = "stun://stun.l.google.com",
        stabilizeInterval: Long = 10,
    ): RingsProvider = RingsProvider(
        native,
        account,
        signer,
        accountType,
        networkId,
        iceServer,
        stabilizeInterval,
    )
}

/** Owned provider handle aligned with `examples/ffi/rings.py`. */
class RingsProvider internal constructor(
    private val native: NativeRings,
    account: String,
    signer: RingsSigner,
    accountType: String,
    networkId: Int,
    iceServer: String,
    stabilizeInterval: Long,
) : Closeable {
    private val signerFailure = AtomicReference<Throwable?>()
    private val callback = SignerCallback { message, output ->
        try {
            require(message != null && output != null) { "Rings passed a NULL signer pointer" }
            val signature = signer.sign(message.getString(0).toByteArray(StandardCharsets.UTF_8))
            require(signature.size == RINGS_SIGNATURE_LENGTH) {
                "signer returned ${signature.size} bytes; expected $RINGS_SIGNATURE_LENGTH"
            }
            output.write(0, signature, 0, signature.size)
        } catch (error: Throwable) {
            output?.clear(RINGS_SIGNATURE_LENGTH.toLong())
            signerFailure.set(error)
        }
    }
    private var handle: Pointer? = native.rings_node_new_provider_with_callback(
        networkId,
        iceServer,
        stabilizeInterval,
        account,
        accountType,
        callback,
    ) ?: run {
        val signerError = signerFailure.getAndSet(null)
        throw RingsFfiException(
            "rings_node_new_provider_with_callback returned NULL",
            signerError,
        )
    }

    fun listen() {
        native.rings_node_listen(requireHandle())
    }

    fun request(method: String, json: String): String {
        val response = native.rings_node_request(requireHandle(), method, json)
        if (response == null) {
            val signerError = signerFailure.getAndSet(null)
            throw RingsFfiException("Rings request $method returned NULL", signerError)
        }
        return try {
            response.getString(0)
        } finally {
            native.rings_node_string_free(response)
        }
    }

    fun requestJson(method: String, json: String): JsonElement = JSON.parseToJsonElement(
        request(method, json),
    )

    fun nodeDid(): String = requestJson("nodeDid", "{}").jsonObject.string("did")

    fun createOffer(remoteDid: String): String = requestJson(
        "createOffer",
        buildJsonObject { put("did", remoteDid) }.toString(),
    ).jsonObject.string("offer")

    fun answerOffer(offer: String): String = requestJson(
        "answerOffer",
        buildJsonObject { put("offer", offer) }.toString(),
    ).jsonObject.string("answer")

    fun acceptAnswer(answer: String): JsonElement = requestJson(
        "acceptAnswer",
        buildJsonObject { put("answer", answer) }.toString(),
    )

    fun listPeers(): List<JsonObject> = requestJson("listPeers", "{}")
        .jsonObject.array("peers")
        .map { it.jsonObject }

    fun peerIsConnected(remoteDid: String): Boolean = listPeers().any { peer ->
        peer.stringOrNull("did") == remoteDid && peer.stringOrNull("state") == "Connected"
    }

    fun waitForConnectedPeer(
        remoteDid: String,
        timeoutMillis: Long = 15_000,
        pollMillis: Long = 250,
    ) {
        poll(timeoutMillis, pollMillis, "peer $remoteDid did not reach Connected") {
            peerIsConnected(remoteDid)
        }
    }

    fun connectTo(
        responder: RingsProvider,
        timeoutMillis: Long = 15_000,
        pollMillis: Long = 250,
    ) {
        val initiatorDid = nodeDid()
        val responderDid = responder.nodeDid()
        val offer = createOffer(responderDid)
        val answer = responder.answerOffer(offer)
        acceptAnswer(answer)
        waitForConnectedPeer(responderDid, timeoutMillis, pollMillis)
        responder.waitForConnectedPeer(initiatorDid, timeoutMillis, pollMillis)
    }

    fun takeE2eEvents(): List<JsonObject> = requestJson("takeE2eEvents", "{}")
        .jsonObject.array("events")
        .map { it.jsonObject }

    fun sendE2eHandshake(destinationDid: String): String = requestJson(
        "sendE2eHandshake",
        buildJsonObject { put("destination_did", destinationDid) }.toString(),
    ).jsonObject.string("tx_id")

    fun sendE2eMessage(
        destinationDid: String,
        recipientPublicKey: String,
        plaintext: ByteArray,
        maxPlaintextFrameLength: Int = 0,
    ): String = requestJson(
        "sendE2eMessage",
        buildJsonObject {
            put("destination_did", destinationDid)
            put("recipient_public_key", recipientPublicKey)
            put("data", Base64.getEncoder().encodeToString(plaintext))
            put("max_plaintext_frame_len", maxPlaintextFrameLength)
        }.toString(),
    ).jsonObject.string("stream_id")

    fun waitForE2eEvent(
        timeoutMillis: Long = 15_000,
        pollMillis: Long = 250,
        predicate: (JsonObject) -> Boolean,
    ): JsonObject {
        var match: JsonObject? = null
        poll(timeoutMillis, pollMillis, "matching E2E event was not observed") {
            match = takeE2eEvents().firstOrNull(predicate)
            match != null
        }
        return match ?: throw RingsFfiException("matching E2E event disappeared")
    }

    fun waitForE2eStream(
        streamId: String,
        timeoutMillis: Long = 15_000,
        pollMillis: Long = 250,
    ): List<JsonObject> {
        val frames = mutableListOf<JsonObject>()
        poll(timeoutMillis, pollMillis, "E2E stream $streamId did not reach its final frame") {
            frames += takeE2eEvents().filter { event ->
                event.stringOrNull("kind") == "streamFrame"
                    && event.stringOrNull("stream_id") == streamId
            }
            frames.any { event -> event["is_final"]?.jsonPrimitive?.content == "true" }
        }
        return frames
    }

    override fun close() {
        val provider = handle ?: return
        handle = null
        native.rings_node_provider_destroy(provider)
    }

    private fun requireHandle(): Pointer = handle ?: throw RingsFfiException("provider is closed")
}

private val JSON = Json {
    ignoreUnknownKeys = true
}

private fun JsonObject.string(field: String): String = this[field]?.jsonPrimitive?.content
    ?: throw RingsFfiException("response does not contain string field $field")

private fun JsonObject.stringOrNull(field: String): String? = this[field]?.jsonPrimitive?.content

private fun JsonObject.array(field: String): JsonArray = this[field]?.jsonArray
    ?: throw RingsFfiException("response does not contain array field $field")

private fun poll(
    timeoutMillis: Long,
    pollMillis: Long,
    timeoutMessage: String,
    complete: () -> Boolean,
) {
    if (timeoutMillis <= 0 || pollMillis <= 0) {
        throw RingsFfiException("poll timeout and interval must be positive")
    }
    val deadline = System.nanoTime() + TimeUnit.MILLISECONDS.toNanos(timeoutMillis)
    while (System.nanoTime() < deadline) {
        if (complete()) return
        try {
            Thread.sleep(pollMillis)
        } catch (error: InterruptedException) {
            Thread.currentThread().interrupt()
            throw RingsFfiException("polling was interrupted", error)
        }
    }
    throw RingsFfiException(timeoutMessage)
}
