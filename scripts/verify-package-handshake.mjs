import fs from "node:fs";
import path from "node:path";
import { spawn } from "node:child_process";

const [packageRoot, platform] = process.argv.slice(2);
if (!packageRoot || !platform) {
  throw new Error("usage: node scripts/verify-package-handshake.mjs <package-root> <platform>");
}

const manifestPath = path.join(packageRoot, "planeai-plugin.json");
const manifest = JSON.parse(fs.readFileSync(manifestPath, "utf8"));
const entrypoint = manifest.backend_entrypoints?.[platform];
if (typeof entrypoint !== "string") {
  throw new Error(`manifest does not declare a backend entrypoint for ${platform}`);
}

const binaryPath = path.join(packageRoot, entrypoint);
if (!fs.existsSync(binaryPath)) {
  throw new Error(`staged binary does not exist: ${binaryPath}`);
}

const expected = {
  plugin_id: manifest.id,
  plugin_name: manifest.name,
  plugin_version: manifest.version,
  host_api_version: manifest.host_api_version,
};
const request = {
  jsonrpc: "2.0",
  id: 1,
  method: "plugin.handshake",
  params: {
    host_api_version: manifest.host_api_version,
    host_capabilities: manifest.capabilities,
  },
};

const child = spawn(binaryPath, [], { stdio: ["pipe", "pipe", "pipe"] });
let stdout = "";
let stderr = "";
let settled = false;

const finish = (error) => {
  if (settled) return;
  settled = true;
  clearTimeout(timeout);
  child.kill();
  if (error) {
    console.error(error.message);
    if (stderr) console.error(stderr.trim());
    process.exitCode = 1;
    return;
  }
  console.log(`staged handshake matches manifest v${manifest.version} for ${platform}`);
};

const timeout = setTimeout(() => {
  finish(new Error(`timed out waiting for plugin.handshake from ${binaryPath}`));
}, 10_000);

child.on("error", finish);
child.stderr.on("data", (chunk) => {
  stderr += chunk;
});
child.stdout.on("data", (chunk) => {
  stdout += chunk;
  const newline = stdout.indexOf("\n");
  if (newline === -1) return;

  try {
    const response = JSON.parse(stdout.slice(0, newline));
    if (response.error) throw new Error(`plugin.handshake returned ${response.error.message}`);
    for (const [field, value] of Object.entries(expected)) {
      if (response.result?.[field] !== value) {
        throw new Error(`plugin.handshake ${field} (${response.result?.[field]}) does not match manifest (${value})`);
      }
    }
    finish();
  } catch (error) {
    finish(error);
  }
});
child.on("exit", (code, signal) => {
  if (!settled) {
    finish(new Error(`plugin exited before plugin.handshake completed (code ${code}, signal ${signal})`));
  }
});
child.stdin.write(`${JSON.stringify(request)}\n`);
