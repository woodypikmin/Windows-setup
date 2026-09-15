Pikmin Pilot Pairing Helper CLI RC1

Purpose
-------
Generate an idevice-native RPPairing plist over a trusted USB connection,
without launching the idevice_pair GUI. It mirrors idevice_pair's backend:
CoreDeviceProxy -> software tunnel -> RSD -> untrusted tunnelservice ->
RemotePairingClient -> RpPairingFile.

Build on GitHub Actions
-----------------------
1. Put this folder in a GitHub repository.
2. Actions -> Build Pikmin Pilot Pairing Helper (Windows) -> Run workflow.
3. Download artifact PikminPilotPairingHelper-Windows-x64.

Run on Windows
--------------
Prerequisite: Apple/iTunes device drivers must be installed and the iPhone/iPad
must be USB-connected, unlocked and trusted.

Example:
  .\PikminPilotPairingHelper.exe --udid 00008103-000A78D13684C01E --output C:\Users\woody\Downloads\rp_pairing_file.plist

If exactly one USB device is connected, --udid can be omitted.

Expected Pikmin Pilot schema
----------------------------
The output is idevice::RpPairingFile, i.e. the same family Pikmin Pilot reads
(identifier/public_key/private_key/alt_irk), not pymobiledevice3's three-field
lockdown-remotepairing record.

Do NOT reuse one device's pairing file on another device.


RC3 BUILD FIX
=============
RC1 accidentally used idevice 0.1.65 while its pairing code mirrors idevice_pair 0.1.14,
which was written against the idevice 0.1.61 API. idevice is pre-0.2 and point releases
can contain breaking API changes. RC3 pins idevice exactly to =0.1.61 and the GitHub
workflow asserts the resolved version before compiling.

RC3 compile fix:
- imports idevice::IdeviceService, required for CoreDeviceProxy::connect(provider)
- enables idevice pair feature to match upstream idevice_pair pairing build
