use std::{
    env,
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    process::{self, Command, Stdio},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use idevice::{
    IdeviceError, IdeviceService,
    afc::opcode::AfcFopenMode,
    house_arrest::HouseArrestClient,
    installation_proxy::InstallationProxyClient,
    provider::IdeviceProvider,
    usbmuxd::{Connection, UsbmuxdAddr, UsbmuxdConnection, UsbmuxdDevice},
    utils::installation,
};

const APP_BUNDLE_ID: &str = "com.woodypikmin.pikminpilot";
const APP_VERSION: &str = "11.5.4.17";
const PAIRING_NAME: &str = "rp_pairing_file.plist";
const EMBEDDED_IPA: &[u8] = include_bytes!("../assets/PikminPilot.ipa");
const EMBEDDED_PAIR_HELPER: &[u8] = include_bytes!("../assets/PikminPilotPairingHelper.exe");

#[derive(Default)]
struct Args {
    udid: Option<String>,
    no_pause: bool,
    show_udid_only: bool,
}

fn parse_args() -> Args {
    let mut out = Args::default();
    let args: Vec<String> = env::args().collect();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--udid" => {
                i += 1;
                if i >= args.len() {
                    eprintln!("--udid requires a value");
                    process::exit(2);
                }
                out.udid = Some(args[i].clone());
            }
            "--no-pause" => out.no_pause = true,
            "--show-udid" => out.show_udid_only = true,
            "-h" | "--help" => {
                println!("PikminPilotSetup.exe [--udid <UDID>] [--show-udid] [--no-pause]");
                process::exit(0);
            }
            other => {
                eprintln!("Unknown argument: {other}");
                process::exit(2);
            }
        }
        i += 1;
    }
    out
}

fn pause_if_needed(no_pause: bool) {
    if no_pause {
        return;
    }
    print!("\n按 Enter 關閉... ");
    let _ = io::stdout().flush();
    let mut s = String::new();
    let _ = io::stdin().read_line(&mut s);
}

fn fail(no_pause: bool, code: i32, message: impl AsRef<str>) -> ! {
    eprintln!("\n❌ {}", message.as_ref());
    pause_if_needed(no_pause);
    process::exit(code);
}

fn print_header() {
    println!("============================================================");
    println!(" Pikmin Pilot One-Click Setup  •  App {APP_VERSION}");
    println!("============================================================");
    println!("USB 接上 → 解鎖 → iPhone/iPad 若詢問請按『信任』。");
    println!("不需要 Python / pymobiledevice3 / Sideloadly / 手動 plist。\n");
}

fn setup_root() -> PathBuf {
    env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(env::temp_dir)
        .join("PikminPilotSetup")
}

fn write_udid_note(udid: &str) {
    let text = format!(
        "Pikmin Pilot device UDID\r\n\r\n{udid}\r\n\r\nIf install reports provisioning/profile failure, send this UDID to the Pikmin Pilot owner.\r\n"
    );
    let path = env::var_os("USERPROFILE")
        .map(PathBuf::from)
        .unwrap_or_else(env::temp_dir)
        .join("Desktop")
        .join("PikminPilot-UDID.txt");
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if fs::write(&path, text).is_ok() {
        println!("      UDID copy: {}", path.display());
    }
}

async fn usb_devices() -> Result<Vec<UsbmuxdDevice>, String> {
    let mut mux = UsbmuxdConnection::default().await.map_err(|e| format!("{e:?}"))?;
    Ok(mux
        .get_devices()
        .await
        .map_err(|e| format!("{e:?}"))?
        .into_iter()
        .filter(|d| d.connection_type == Connection::Usb)
        .collect())
}

async fn wait_for_usb_device(requested: Option<&str>) -> Result<UsbmuxdDevice, String> {
    println!("[1/5] 等待 USB iPhone/iPad...");
    let mut last_error = String::new();
    for _ in 0..45 {
        match usb_devices().await {
            Ok(devices) => {
                let matches: Vec<_> = if let Some(udid) = requested {
                    devices.into_iter().filter(|d| d.udid == udid).collect()
                } else {
                    devices
                };
                match matches.len() {
                    1 => return Ok(matches.into_iter().next().unwrap()),
                    n if n > 1 && requested.is_none() => {
                        return Err("偵測到超過一台 USB iPhone/iPad；請只保留一台連接。".into());
                    }
                    _ => {}
                }
            }
            Err(e) => last_error = e,
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    if last_error.is_empty() {
        Err("90 秒內沒有偵測到 USB iPhone/iPad。請確認 USB 線與 Apple 裝置驅動。".into())
    } else {
        Err(format!(
            "Apple USB device service unavailable: {last_error}\n請先安裝 Apple Devices / Apple Mobile Device Support，確認 Windows 能看到 iPhone/iPad。"
        ))
    }
}

fn prepare_pair_helper() -> Result<PathBuf, String> {
    if EMBEDDED_PAIR_HELPER.len() < 1_000_000 {
        return Err("內嵌 Pairing Helper 無效。".into());
    }
    let dir = setup_root().join("Tools");
    fs::create_dir_all(&dir).map_err(|e| format!("create tools dir: {e}"))?;
    let path = dir.join("PikminPilotPairingHelper.exe");
    let needs_write = match fs::metadata(&path) {
        Ok(m) => m.len() != EMBEDDED_PAIR_HELPER.len() as u64,
        Err(_) => true,
    };
    if needs_write {
        fs::write(&path, EMBEDDED_PAIR_HELPER).map_err(|e| format!("extract Pairing Helper: {e}"))?;
    }
    Ok(path)
}

fn pairing_output_path(udid: &str) -> Result<PathBuf, String> {
    let dir = setup_root().join("PairingBackup");
    fs::create_dir_all(&dir).map_err(|e| format!("create pairing dir: {e}"))?;
    Ok(dir.join(format!("rp_pairing_file_{udid}.plist")))
}

async fn run_known_good_pair_helper(udid: &str) -> Result<Vec<u8>, String> {
    println!("[2/5] 建立這台裝置專屬 RPPairing...");
    println!("      保持裝置解鎖；若出現 Trust/Pairing 提示請允許。");
    let helper = prepare_pair_helper()?;
    let output = pairing_output_path(udid)?;

    // The uploaded, hardware-validated helper is the authoritative pairing engine.
    // Retry only for trust/USB timing; each successful run commits the record twice itself.
    let mut last = String::new();
    for attempt in 1..=12 {
        let status = Command::new(&helper)
            .arg("--udid")
            .arg(udid)
            .arg("--output")
            .arg(&output)
            .arg("--hostname")
            .arg("Pikmin Pilot Windows Setup")
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status();

        match status {
            Ok(s) if s.success() => {
                let bytes = fs::read(&output).map_err(|e| format!("read generated pairing: {e}"))?;
                if bytes.len() < 64 {
                    return Err(format!("Pairing Helper returned success but file is too small: {} bytes", bytes.len()));
                }
                println!("      ✅ Pairing ready ({} bytes)", bytes.len());
                println!("      backup: {}", output.display());
                return Ok(bytes);
            }
            Ok(s) => last = format!("Pairing Helper exit code {:?}", s.code()),
            Err(e) => last = format!("Pairing Helper could not start: {e}"),
        }
        if attempt == 1 {
            println!("      尚未完成 pairing；請確認 iPhone/iPad 已解鎖並按『信任』。Setup 會自動重試。");
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    Err(format!("Pairing failed after retries: {last}"))
}

async fn app_is_installed(provider: &dyn IdeviceProvider) -> Result<bool, IdeviceError> {
    let mut client = InstallationProxyClient::connect(provider).await?;
    let apps = client
        .get_apps(Some("User"), Some(vec![APP_BUNDLE_ID.to_string()]))
        .await?;
    Ok(apps.contains_key(APP_BUNDLE_ID))
}

async fn install_or_upgrade(provider: &dyn IdeviceProvider, udid: &str) -> Result<(), String> {
    if EMBEDDED_IPA.len() < 1024 * 1024 {
        return Err("Setup 內沒有有效的 Pikmin Pilot IPA。".into());
    }

    println!("[3/5] 安裝 Pikmin Pilot {APP_VERSION}...");
    let installed = app_is_installed(provider).await.unwrap_or(false);
    let result = if installed {
        println!("      已存在 Pikmin Pilot，執行 upgrade（保留 App container）...");
        installation::upgrade_bytes_with_callback(
            provider,
            EMBEDDED_IPA,
            None,
            |(percent, ())| async move { println!("      install progress: {percent}%"); },
            (),
        )
        .await
    } else {
        installation::install_bytes_with_callback(
            provider,
            EMBEDDED_IPA,
            None,
            |(percent, ())| async move { println!("      install progress: {percent}%"); },
            (),
        )
        .await
    };

    match result {
        Ok(()) => {
            println!("      ✅ Pikmin Pilot installed");
            Ok(())
        }
        Err(e) => {
            write_udid_note(udid);
            let text = format!("{e:?}");
            let lower = text.to_ascii_lowercase();
            if lower.contains("e8008015")
                || lower.contains("provision")
                || lower.contains("applicationverificationfailed")
            {
                Err(format!(
                    "iOS 拒絕安裝：這台裝置 UDID 很可能尚未包含在目前 Development profiles。\nUDID={udid}\n請把桌面的 PikminPilot-UDID.txt 給發佈者，更新 App / Tunnel / Runner profiles 後換新的 Setup.exe 再跑一次。\n原始錯誤：{text}"
                ))
            } else {
                Err(format!("IPA install failed: {text}"))
            }
        }
    }
}

async fn wait_for_app_registration(provider: &dyn IdeviceProvider) -> Result<(), String> {
    // InstallationProxy may report install completion slightly before all services see
    // the refreshed app registration. Poll the authoritative installed-app lookup with
    // a fresh client each time before opening House Arrest.
    for attempt in 1..=20 {
        match app_is_installed(provider).await {
            Ok(true) => {
                if attempt > 1 {
                    println!("      ✅ app registration ready after {attempt} checks");
                }
                return Ok(());
            }
            Ok(false) => {}
            Err(e) => {
                if attempt == 20 {
                    return Err(format!("InstallationProxy lookup failed: {e:?}"));
                }
            }
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    Err(format!("{APP_BUNDLE_ID} did not appear in InstallationProxy after install"))
}

async fn inject_pairing(provider: &dyn IdeviceProvider, pairing: &[u8]) -> Result<(), String> {
    println!("[4/5] 把 pairing 自動寫進 Pikmin Pilot...");

    // IMPORTANT: the signed 11.5.4.17 IPA intentionally does not advertise
    // UIFileSharingEnabled. On modern iOS, House Arrest VendDocuments can therefore
    // return InstallationLookupFailed even though the app is correctly installed.
    // VendContainer does not depend on Files/iTunes document-sharing opt-in. It gives
    // us the app container, from which /Documents is writable by AFC.
    wait_for_app_registration(provider).await?;
    println!("      House Arrest mode: VendContainer → /Documents (no UIFileSharingEnabled required)");

    let mut last_error = String::new();
    for attempt in 1..=12 {
        let house = match HouseArrestClient::connect(provider).await {
            Ok(v) => v,
            Err(e) => {
                last_error = format!("HouseArrest connect: {e:?}");
                println!("      ⚠ injection attempt {attempt}/12: {last_error}");
                tokio::time::sleep(Duration::from_millis(1200)).await;
                continue;
            }
        };

        let mut afc = match house.vend_container(APP_BUNDLE_ID.to_string()).await {
            Ok(v) => v,
            Err(e) => {
                last_error = format!("VendContainer: {e:?}");
                println!("      ⚠ injection attempt {attempt}/12: {last_error}");
                // Reconnect House Arrest on the next pass; do not reuse a vend session.
                tokio::time::sleep(Duration::from_millis(1200)).await;
                continue;
            }
        };

        let path = format!("/Documents/{PAIRING_NAME}");
        let write_result = async {
            let mut file = afc
                .open(&path, AfcFopenMode::Wr)
                .await
                .map_err(|e| format!("AFC open write: {e:?}"))?;
            file.write_entire(pairing)
                .await
                .map_err(|e| format!("AFC write: {e:?}"))?;
            file.close()
                .await
                .map_err(|e| format!("AFC close/commit: {e:?}"))?;

            let mut check = afc
                .open(&path, AfcFopenMode::RdOnly)
                .await
                .map_err(|e| format!("AFC verify open: {e:?}"))?;
            let got = check
                .read_entire()
                .await
                .map_err(|e| format!("AFC verify read: {e:?}"))?;
            check.close()
                .await
                .map_err(|e| format!("AFC verify close: {e:?}"))?;
            if got.as_slice() != pairing {
                return Err(format!(
                    "pairing read-back mismatch: wrote {} bytes, read {} bytes",
                    pairing.len(),
                    got.len()
                ));
            }
            Ok::<(), String>(())
        }
        .await;

        match write_result {
            Ok(()) => {
                println!("      ✅ {path} verified ({} bytes)", pairing.len());
                return Ok(());
            }
            Err(e) => {
                last_error = e;
                println!("      ⚠ injection attempt {attempt}/12 failed: {last_error}");
            }
        }
        tokio::time::sleep(Duration::from_millis(1200)).await;
    }
    Err(format!("pairing injection failed after retries: {last_error}"))
}

fn print_success(udid: &str) {
    println!("[5/5] 完成\n");
    println!("✅ Pikmin Pilot {APP_VERSION} 已安裝");
    println!("✅ Device UDID: {udid}");
    println!("✅ 此裝置專屬 RPPairing 已建立");
    println!("✅ {PAIRING_NAME} 已寫入 App Documents 並 read-back 驗證");
    println!("\n現在直接在 iPhone/iPad 打開 Pikmin Pilot 即可。");
    println!("第一次啟動若 iOS 詢問 VPN / Local Network 權限，照畫面允許。\n");
}

#[tokio::main]
async fn main() {
    let args = parse_args();
    print_header();

    let device = match wait_for_usb_device(args.udid.as_deref()).await {
        Ok(d) => d,
        Err(e) => fail(args.no_pause, 10, e),
    };
    println!("      Device UDID: {}", device.udid);
    write_udid_note(&device.udid);

    if args.show_udid_only {
        println!("\nUDID={}", device.udid);
        pause_if_needed(args.no_pause);
        return;
    }

    let pairing_bytes = match run_known_good_pair_helper(&device.udid).await {
        Ok(v) => v,
        Err(e) => fail(
            args.no_pause,
            20,
            format!("{e}\n請保持 USB 連接、裝置解鎖並已按『信任』，然後重新執行 Setup。"),
        ),
    };

    let provider = device.to_provider(UsbmuxdAddr::default(), "PikminPilotSetup");

    if let Err(e) = install_or_upgrade(&provider, &device.udid).await {
        fail(args.no_pause, 30, e);
    }

    // App-registration readiness is polled inside inject_pairing(); no blind fixed delay.

    if let Err(e) = inject_pairing(&provider, &pairing_bytes).await {
        fail(
            args.no_pause,
            40,
            format!("{e}\nApp 已安裝、pairing 也已備份；請保持 USB 連接後直接重跑 Setup。"),
        );
    }

    print_success(&device.udid);
    pause_if_needed(args.no_pause);
}
