PIKMIN PILOT 11.5.4.17 — FRIEND ONE-CLICK SETUP BUILDER RC2
==============================================================

這版的 GitHub Repository 不再存放 31 MB IPA。

Repo 只需要小檔案：
- WindowsSetup source
- PikminPilotPairingHelper.exe (~5 MB)
- GitHub Actions workflow

31 MB 的：
  PikminPilot-11.5.4.17.ipa
請放到 GitHub Release：
  tag = pikminpilot-11.5.4.17

Actions 執行時會：
1. 從該 Release 自動下載 IPA
2. 驗證固定 SHA256
3. 驗證 pairing helper SHA256
4. 編譯單一 PikminPilotSetup.exe
5. 上傳成 GitHub Actions artifact

朋友端最後仍然只需要 PikminPilotSetup.exe。

請直接看 OWNER_STEP_BY_STEP.txt。
