import { spawnSync } from "node:child_process";

export function verifyDelegationHandshake(result, version) {
  if (result.error) throw result.error;
  if (result.status !== 0) throw new Error(`Delegation sidecar exited ${result.status}`);
  const messages = result.stdout.trim().split(/\r?\n/).map((line) => JSON.parse(line));
  if (messages.length !== 2 || messages.some((message) => message.jsonrpc !== "2.0" || message.error)) {
    throw new Error("Expected exactly two successful delegation JSON-RPC responses");
  }
  const initialized = messages.find((message) => message.id === 1)?.result;
  const ping = messages.find((message) => message.id === 2)?.result;
  if (initialized?.protocolVersion !== "2024-11-05" ||
      initialized.serverInfo?.name !== "vibex-agent-delegation" ||
      initialized.serverInfo?.version !== version ||
      initialized.capabilities?.tools?.listChanged !== false ||
      !ping || typeof ping !== "object" || Array.isArray(ping) || Object.keys(ping).length !== 0) {
    throw new Error("Delegation handshake identity or capabilities mismatch");
  }
  return { status: "passed", initialize: true, ping: true, modelRequests: 0 };
}

export function checkDevelopmentDelegation(executable, version, options) {
  const input = [
    { jsonrpc: "2.0", id: 1, method: "initialize", params: {
      protocolVersion: "2024-11-05", capabilities: {},
      clientInfo: { name: "vibex-package-check", version: "1" }
    } },
    { jsonrpc: "2.0", method: "notifications/initialized" },
    { jsonrpc: "2.0", id: 2, method: "ping" }
  ].map((message) => JSON.stringify(message)).join("\n") + "\n";
  const result = spawnSync(executable, ["--agent-delegation-mcp"], {
    cwd: options.cwd, encoding: "utf8", windowsHide: true,
    timeout: 15_000, maxBuffer: 256 * 1024, input,
    env: { ...options.env,
      VIBEX_AGENT_DELEGATION_ENDPOINT: "127.0.0.1:1",
      VIBEX_AGENT_DELEGATION_TOKEN: "non-secret-package-probe",
      VIBEX_AGENT_DELEGATION_PARENT_SESSION: "session_package_probe",
      VIBEX_AGENT_DELEGATION_AUTHORITY: "local",
      VIBEX_AGENT_DELEGATION_ACTIVATION: "1"
    }
  });
  return verifyDelegationHandshake(result, version);
}
