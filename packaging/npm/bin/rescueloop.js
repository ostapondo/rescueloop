#!/usr/bin/env node

const { existsSync } = require("node:fs");
const { spawnSync } = require("node:child_process");
const { join } = require("node:path");

const binaries = {
  "darwin-arm64": "rescueloop-darwin-arm64",
  "darwin-x64": "rescueloop-darwin-x64",
  "win32-x64": "rescueloop-win32-x64.exe",
};

const platform = `${process.platform}-${process.arch}`;
const binaryName = binaries[platform];
if (!binaryName) {
  console.error(
    `RescueLoop does not provide an npm binary for ${platform}. Supported platforms: macOS arm64, macOS x64, and Windows x64.`,
  );
  process.exit(1);
}

const binary = join(__dirname, "..", "native", binaryName);
if (!existsSync(binary)) {
  console.error(`The RescueLoop npm package is incomplete: ${binaryName} is missing.`);
  process.exit(1);
}

const result = spawnSync(binary, process.argv.slice(2), {
  argv0: "rescueloop",
  stdio: "inherit",
});
if (result.error) {
  console.error(`Could not start RescueLoop: ${result.error.message}`);
  process.exit(1);
}
if (result.signal) {
  console.error(`RescueLoop stopped after receiving ${result.signal}.`);
  process.exit(1);
}
process.exit(result.status ?? 1);
