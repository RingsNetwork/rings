import assert from "node:assert/strict";
import test from "node:test";

import {
  connectDidRequest,
  connectSeedRequest,
  createExampleHandler,
  hexToBytes,
  sendExampleMessageRequest,
  stringify,
} from "../app-core.mjs";

test("hexToBytes accepts prefixed even-length hex", () => {
  assert.deepEqual(Array.from(hexToBytes("0x000aff")), [0, 10, 255]);
});

test("hexToBytes rejects malformed signatures", () => {
  assert.throws(() => hexToBytes("0xabc"), /even-length/);
  assert.throws(() => hexToBytes("0xzz"), /even-length/);
});

test("connectSeedRequest targets connectPeerViaHttp", () => {
  assert.deepEqual(connectSeedRequest(" http://127.0.0.1:50001 "), {
    method: "connectPeerViaHttp",
    params: { url: "http://127.0.0.1:50001" },
  });
});

test("connectDidRequest targets connectWithDid", () => {
  assert.deepEqual(connectDidRequest(" 0xabc "), {
    method: "connectWithDid",
    params: { did: "0xabc" },
  });
});

test("sendExampleMessageRequest base64-encodes payload bytes", () => {
  const req = sendExampleMessageRequest(" 0xdef ", new TextEncoder().encode("hello"));

  assert.equal(req.method, "sendBackendMessage");
  assert.deepEqual(req.params, {
    destination_did: "0xdef",
    namespace: "example",
    data: "aGVsbG8=",
  });
});

test("createExampleHandler increments received count and emits no effects", () => {
  const seen = [];
  const handler = createExampleHandler((message, detail) => seen.push([message, detail]));
  const result = handler(
    { state: { received: 2 } },
    { from: "0xabc", payload: new TextEncoder().encode("payload") },
  );

  assert.deepEqual(result, { state: { received: 3 }, effects: [] });
  assert.deepEqual(seen, [["received example message from 0xabc", "payload"]]);
});

test("stringify keeps strings and serializes objects", () => {
  assert.equal(stringify("plain"), "plain");
  assert.equal(stringify({ ok: true }), '{\n  "ok": true\n}');
});
