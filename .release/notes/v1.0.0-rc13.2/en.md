Tune 1.0.0-rc13.2, a corrective release of rc13.1.

## Fixes

- Sign-in: the administrator account password is checked the same way on the server machine and on the local network; clearer error message (username: default).
- If you can no longer sign in, use the username default and the last password entered on the server machine.
- The mozaiklabs.fr account can now be linked from any browser on the local network (iPad, phone) when no account is linked yet.
- Windows: the built-in update no longer stops on “Access denied” while staging the bundled plugins (plugins.new\party). A plugin that cannot be replaced keeps its version, and the update goes ahead anyway.
- Windows on rc13 or rc13.1: the update button of those versions stays blocked. Install this version with the installer (setup.exe), or close Tune and extract the zip archive over the installation folder. Later updates will work from the button again.
