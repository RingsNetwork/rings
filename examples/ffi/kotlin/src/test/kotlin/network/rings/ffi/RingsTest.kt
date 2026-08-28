package network.rings.ffi

import kotlin.test.Test
import kotlin.test.assertEquals

class RingsTest {
    @Test
    fun actualRustAbiLoadsResolvesSymbolsAndInitializesLogging() {
        val library = System.getProperty("rings.actual.library")
        if (library == null) {
            if (System.getProperty("rings.require.actual.library") == "1") {
                error("RINGS_FFI_REQUIRE_LIBRARY=1 but RINGS_FFI_LIBRARY is not set")
            }
            return
        }

        RingsRuntime.load(library).initializeLogging()
    }

    @Test
    fun twoProvidersRetainDistinctCallbacksAndFreeResponses() {
        val library = requireNotNull(System.getProperty("rings.fake.library"))
        val runtime = RingsRuntime.load(library)
        runtime.initializeLogging()
        runtime.createProvider("fixture-one", RingsSigner { ByteArray(RINGS_SIGNATURE_LENGTH) { 1 } })
            .use { first ->
                runtime.createProvider(
                    "fixture-two",
                    RingsSigner { ByteArray(RINGS_SIGNATURE_LENGTH) { 2 } },
                ).use { second ->
                    first.listen()
                    second.listen()
                    assertEquals("{\"ok\":true}", first.request("nodeInfo", "{}"))
                    assertEquals("{\"byte\":1}", first.request("signerByte", "{}"))
                    assertEquals("{\"byte\":2}", second.request("signerByte", "{}"))
                    assertEquals("fixture-one", first.nodeDid())
                    assertEquals("fixture-two", second.nodeDid())
                    first.connectTo(second, timeoutMillis = 250, pollMillis = 10)
                    assertEquals(true, first.peerIsConnected("fixture-two"))
                    assertEquals(true, second.peerIsConnected("fixture-one"))
                    assertEquals(
                        "fixture-transaction",
                        first.sendE2eHandshake("fixture-two"),
                    )
                    val stream = first.sendE2eMessage(
                        "fixture-two",
                        "fixture-public-key",
                        "fixture-body".encodeToByteArray(),
                    )
                    assertEquals("fixture-stream", stream)
                    assertEquals(
                        1,
                        second.waitForE2eStream(
                            stream,
                            timeoutMillis = 250,
                            pollMillis = 10,
                        ).size,
                    )
                }
            }
    }
}
