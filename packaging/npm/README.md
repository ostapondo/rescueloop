# RescueLoop

Install the RescueLoop CLI:

```sh
npm install --global rescueloop
rescueloop
```

Running `rescueloop` starts the native background watcher when needed and opens the interactive
console. Closing the console leaves detection running. Manage the watcher with:

```sh
rescueloop stop
rescueloop status
rescueloop restart
rescueloop uninstall
```

`stop` preserves the background registration; `uninstall` removes the registration but does not
remove the npm package or delete incident history.

The watcher runs from a stable per-user binary copy. Updating or replacing the npm package while
the watcher is active does not leave the native service pointing into the old `node_modules` tree;
the next start refreshes the stable copy with a bounded native restart.

The package includes native binaries for macOS arm64, macOS x64, and Windows x64. See the
[RescueLoop repository](https://github.com/ostapondo/rescueloop) for full usage and security details.

## Supported platforms

| Operating system | Architecture |
| --- | --- |
| macOS | Apple silicon (`arm64`) and Intel (`x64`) |
| Windows | Intel/AMD 64-bit (`x64`) |

Node.js 18 or newer is required only for the npm launcher. RescueLoop itself is a native binary.
Linux and Windows on ARM are not included in this release.

Published packages include npm provenance. RescueLoop does not enable telemetry or send incident
data anywhere by default.
