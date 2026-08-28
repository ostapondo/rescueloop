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

The package includes native binaries for macOS arm64, macOS x64, and Windows x64. See the
[RescueLoop repository](https://github.com/ostapondo/rescueloop) for full usage and security details.
