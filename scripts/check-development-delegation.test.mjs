import assert from "node:assert/strict";
import { test } from "node:test";
import { verifyDelegationHandshake } from "./check-development-delegation.mjs";

function response() {
  return [
    { jsonrpc: "2.0", id: 1, result: {
      protocolVersion: "2024-11-05",
      capabilities: { tools: { listChanged: false } },
      serverInfo: { name: "vibex-agent-delegation", version: "0.1.9" }
    } },
    { jsonrpc: "2.0", id: 2, result: {} }
  ];
}

function output(messages) {
  return { status: 0, stdout: messages.map((message) => JSON.stringify(message)).join("\n") + "\n" };
}

test("accepts an initialize and ping roundtrip", () => {
  assert.equal(verifyDelegationHandshake(output(response()), "0.1.9").status, "passed");
});

test("rejects the old CLI unknown-option failure", () => {
  assert.throws(() => verifyDelegationHandshake({ status: 1, stdout: "", stderr: "unknown option --agent-delegation-mcp" }, "0.1.9"), /exited/);
});

test("rejects timeout, help output and missing ping", () => {
  assert.throws(() => verifyDelegationHandshake({ error: new Error("timeout") }, "0.1.9"), /timeout/);
  assert.throws(() => verifyDelegationHandshake({ status: 0, stdout: "USAGE: vibex" }, "0.1.9"));
  assert.throws(() => verifyDelegationHandshake(output(response().slice(0, 1)), "0.1.9"), /exactly two/);
});

test("rejects wrong identity, version, capabilities and RPC errors", () => {
  for (const mutate of [
    (messages) => { messages[0].result.serverInfo.name = "other"; },
    (messages) => { messages[0].result.serverInfo.version = "0.0.0"; },
    (messages) => { messages[0].result.capabilities.tools.listChanged = true; },
    (messages) => { messages[1] = { jsonrpc: "2.0", id: 2, error: { code: -32000 } }; },
    (messages) => { messages[1].result = []; },
    (messages) => { messages[1].result = true; },
    (messages) => { messages[1].result = 1; },
    (messages) => { messages[1].result = "ok"; },
    (messages) => { messages[1].id = 1; }
  ]) {
    const messages = response();
    mutate(messages);
    assert.throws(() => verifyDelegationHandshake(output(messages), "0.1.9"));
  }
});
