# ZSDesk file-location routing

The controller and source-built adapter route documented Shell file-location requests to ZSDesk. The complete UTF-16 packet preserves directory opening and same-directory single/multiple selection. Default configuration is disabled and has no application or target bindings.

## Building and packaging

Run `build.ps1 -OutputDir <absolute-directory> -BuildDir <absolute-build-cache>` with Visual Studio 2022 C++ tools and the Windows SDK installed. CMake is an alternative build entry. Build products and temporary files stay outside the source tree. The build keeps assertions and rejects compiled products containing detected private build paths; PDBs are not package members.

Ship these files under the application's `routing` directory:

- `ShellRoutingControl.exe`
- `ZSDeskRevealRouter.dll`
- `manifest.json`
- `MinHook-LICENSE.txt`

The manifest records relative names and binary hashes. The adapter is built from the included source and the repository's BSD-2-Clause-licensed MinHook source. Product binaries from Tencent, Microsoft or other applications are not package inputs.

## Local bindings

An explicit settings action creates `shell-routing.json`, `package.ini` and `clients.ini` in the user's configuration area. They contain private executable/module paths, selected applications and SHA-256 identities and must never be included in public source exports or default installation data.

Compatibility contracts cover native x64 executables calling documented Shell APIs, Chromium/Electron `chrome.dll`, and WeChat `Weixin.dll`. Discovery checks executable format and relevant imports; executable and caller-module hashes are verified before attachment. This does not claim control over custom file browsers or every application's internal navigation implementation. Changed identities and unsupported modules fail closed.

Supported calls include `SHOpenFolderAndSelectItems`, Explorer directory/select commands, and `ShellExecuteW`/`ShellExecuteExW` opening an existing filesystem directory with the default, `open`, or `explore` verb. Directory PIDLs and common asynchronous/process-handle flags are supported. A reused window does not return a new process handle. Ordinary file opening, program launches, custom verbs, hidden/minimized requests, and unsupported flags keep their original Shell behavior.

The controller verifies the target image, user and session and selects a window by its stable routing-profile property. `TargetProfileDir` supplies the same profile when launching the target. The originating application delegates foreground permission to the controller, which delegates it to the verified destination after navigation is accepted. The receiving UI thread handles the separate `ZSDesk.Activate.v1` activation contract; `ZSDesk.PreferredShow` preserves saved maximized startup state. Windows foreground restrictions remain in effect. `--forward-test` never sends activation requests or shows a window.

Requests are bounded to 1 MiB and 128 selected items, with a 16-request queue and bounded duplicate-ID history. Timeouts are indeterminate and are not replayed. `--restore` disables configured adapters without unloading pinned modules and restores only the owned startup value. A different loaded adapter location is rejected. Compatible legacy modules can be retained through explicit local legacy bindings; they are not distributed with this package.
