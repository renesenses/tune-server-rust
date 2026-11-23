Tune 1.0.0-rc13.2, a corrective release of rc13.1.

## Fixes

- Sign-in: the administrator account password is checked the same way on the server machine and on the local network; clearer error message (username: default).
- If you can no longer sign in, use the username default and the last password entered on the server machine.
- The mozaiklabs.fr account can now be linked from any browser on the local network (iPad, phone) when no account is linked yet.
- From the local network you can also reconnect the same account. Replacing it with another account, or unlinking it, remains reserved to the server machine or to an administrator. The name and email address of the linked account are visible from the local network.
- Windows: updating from within Tune works again. Since rc13 and rc13.1, you had to use the installer.
- Tune OS on rc12 that no longer updates: the "Débloquer Tune" tool (Windows, macOS) restarts the update without any command. It is a separate download: https://github.com/renesenses/tune-server-rust/releases/tag/debloquer-tune-v1
