# Native remote desktop

XunDuTerminal runs RDP sessions inside the desktop process through IronRDP. It does not require Docker, `guacd`, a gateway port, or a separately installed RDP client.

## Connect

1. Add a desktop widget to a workbench.
2. Enter the RDP host, port, username, password, and optional domain.
3. Choose Auto, NLA, or TLS security and connect. Auto tries regular NLA first and falls back to TLS only when the desktop cannot be initialized.

The desktop supports resizing, mouse and keyboard input, Unicode text, sending local clipboard text, view-only mode, and Ctrl+Alt+Delete. Existing workbenches created by the old preview keep their RDP target and credentials; obsolete gateway fields are ignored during migration.

VNC is shown as unavailable until a native VNC engine is integrated.
