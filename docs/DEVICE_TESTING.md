# Testing ZiOS on the iPhone

A short checklist. Report back with the Device Status panel contents and the
exported logs (`Library/Logs/Zed` in the app's folder, visible in the Files app
under "On My iPhone › Zed").

## 1. Install
1. Open the latest successful **device** run on GitHub Actions
   (`.github/workflows/device.yml`) and download the `ZiOS-ipa` artifact.
2. Unzip it to get `ZiOS.ipa`, open it in SideStore, and install.
3. Launch "Zed" once. It opens with the Documents folder.

## 2. Enable JIT
1. Use StikDebug (or SideStore's JIT option) to enable JIT for Zed.
2. Open the Device Status panel: tap the server icon at the right of the
   status bar. Within 5 seconds "JIT" should read **Enabled** with method
   **TXM** (iOS 26+) or **dual-mapping** (older iOS).
3. Note **Increased Memory Limit**: Granted / Not granted, and "Available".

## 3. Edit
1. In the Files app, create a folder under On My iPhone › Zed, add a file.
2. In Zed: Cmd-O (or command palette › "open") shows the Files picker; pick
   the folder. Open the
   file, type, press Cmd-S on a hardware keyboard. Check the file in Files.
3. Quit Zed from the app switcher and relaunch: the folder and file reopen.

## 4. Keyboard
- With a hardware keyboard: Cmd-Shift-P opens the command palette; arrows and
  backspace repeat when held; Option-letter types accented characters.

## 5. Monitor
1. Connect a USB-C monitor (directly or through the hub).
2. The editor moves to the monitor; the phone shows the companion view
   (status, trackpad, quick keys, "Show editor on: Monitor / Phone").
3. Unplug the monitor: the editor returns to the phone with the same file and
   cursor position.

## 6. lan-mouse
1. In Device Status › lan-mouse, note the iPhone's Ethernet/Wi-Fi address and
   its fingerprint.
2. On the laptop's lan-mouse, add a client at that address, port 4242.
3. Move the laptop's cursor across the configured edge. The iPhone lists the
   laptop under "Laptop wants to connect"; tap **Authorize**, then move again.
4. Check: pointer moves and changes shape, hover, click, right-click, scroll,
   typing, and moving back across the edge returns control to the laptop.

## 7. Terminal
- Open a terminal (Ctrl-`). Run `ls`, `pwd`, `cd demo`, `echo hello`,
  `curl -I https://zed.dev`. Ctrl-C stops a running command.

## 8. Send results
- Copy the Device Status panel text (or take a screenshot), and send the
  `Library/Logs/Zed` folder (Zed.log, crashes/, strict-violation.txt if any).

Not yet available, so skip: language servers, git panel, extensions, Python,
Node, Device Self-Test, Export Logs button.
