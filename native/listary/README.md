# ZSdesk integration for Listary

The plugin implements the official [Listary FileAppPlugin API](https://github.com/listary/Listary.FileAppPlugin).
It exposes the active ZSdesk folder to Quick Switch and Currently Opened Folders.
It has no background process, filesystem scan, injected code or Explorer identity emulation.

Build the `net48` project with the .NET SDK and pass `-p:ZSDeskBuildRoot=<absolute external cache directory>`.
Intermediate and output files are separated by project beneath that directory.
The runtime dependency is the pinned `Listary.FileAppPlugin` 1.0.0 NuGet package.
Copy the completed plugin directory into `FileAppPlugins/ZSDesk` under an existing Listary installation,
restart Listary, and enable ZSdesk in **Options → Integration**.
Listary manages switching the standard Open/Save dialog; ZSdesk never writes into another application's dialog.

Only a completed filesystem navigation is shared. Searches, virtual Shell locations, a newly selected tab
whose navigation has not completed, and a closed file window return an empty path.
The read-only endpoint is the hidden `ZSDesk.Listary.Folder.v1` child of `ZSDesk.Native.Explorer`.
The endpoint stores its UTF-16 caption through `DefWindowProc`. The plugin validates its class,
parent and process, then reads this system-maintained caption with cross-process `GetWindowTextW`.
It does not send synchronous messages to ZSdesk's UI thread or request elevation.
The active directory is held in the window, with no persistent path cache.

The `tests/Listary.ZSDesk.Probe.csproj` executable verifies the official interfaces against a running
ZSdesk test window through a cross-process caption read. Its arguments are the decimal
window handle and exact expected folder. The native regression pauses the target UI message pump
and requires the read to complete with the correct path within 1200 ms. This verifies the plugin contract; it does not replace
an end-to-end test with an installed Listary application and a standard Open/Save dialog.
