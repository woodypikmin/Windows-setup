use std::{
    env,
    fs,
    io::{self, Write},
    path::PathBuf,
    process::{self, Command, Stdio},
    time::Duration,
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
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const APP_BUNDLE_ID: &str = "com.woodypikmin.pikminpilot";
const UNIVERSAL_SETUP_REVISION: &str = "RC6_RENDER_STABLE_V1";
const SETUP_PROTOCOL: u8 = 2;
const PAIRING_NAME: &str = "rp_pairing_file.plist";
const EMBEDDED_PAIR_HELPER: &[u8] = include_bytes!("../assets/PikminPilotPairingHelper.exe");
const COMPILED_BACKEND_URL: Option<&str> = option_env!("PIKMIN_BACKEND_URL");

#[derive(Default)]
struct Args {
    udid: Option<String>,
    no_pause: bool,
    show_udid_only: bool,
}

#[derive(Serialize)]
struct InstallRequest<'a> {
    udid: &'a str,
    platform: &'a str,
    setup_protocol: u8,
}

#[derive(Deserialize)]
struct InstallCreated {
    job_id: String,
    status: String,
    app_version: String,
}

#[derive(Deserialize, Debug)]
struct InstallStatus {
    status: String,
    message: Option<String>,
    app_version: Option<String>,
    download_url: Option<String>,
    sha256: Option<String>,
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
    if no_pause { return; }
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
    println!(" Pikmin Pilot One-Click Setup  •  Universal Release Client");
    println!(" Client revision: {UNIVERSAL_SETUP_REVISION}");
    println!("============================================================");
    println!("USB 接上 → 解鎖 → iPhone/iPad 若詢問請按『信任』。");
    println!("目前 App 版本由 Pikmin Pilot backend 動態決定；Setup.exe 不需隨 App 升版重發。\n");
}

fn setup_root() -> PathBuf {
    env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(env::temp_dir)
        .join("PikminPilotSetup")
}

fn backend_url() -> Result<String, String> {
    let raw = env::var("PIKMIN_BACKEND_URL")
        .ok()
        .or_else(|| COMPILED_BACKEND_URL.map(ToOwned::to_owned))
        .unwrap_or_default();
    let url = raw.trim().trim_end_matches('/').to_string();
    if url.is_empty() || url.contains("CHANGE-ME") {
        return Err("Universal Setup 尚未設定 PIKMIN_BACKEND_URL。請由 owner build 正式 Setup.exe。".into());
    }
    if !url.starts_with("https://")
        && !url.starts_with("http://127.0.0.1")
        && !url.starts_with("http://localhost")
    {
        return Err("Backend 必須使用 HTTPS；只有 localhost 測試允許 HTTP。".into());
    }
    Ok(url)
}

fn write_udid_note(udid: &str) {
    let text = format!(
        "Pikmin Pilot device UDID\r\n\r\n{udid}\r\n\r\nUniversal cloud provisioning uses this UDID automatically.\r\n"
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
    println!("[1/7] 等待 USB iPhone/iPad...");
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

async fn request_cloud_job(client: &reqwest::Client, base: &str, udid: &str) -> Result<InstallCreated, String> {
    println!("[2/7] 取得目前版本並準備 provisioning / signed IPA...");
    let endpoint = format!("{base}/api/v1/install");
    let mut last_error = String::new();
    for attempt in 1..=12 {
        match client
            .post(&endpoint)
            .json(&InstallRequest { udid, platform: "IOS", setup_protocol: SETUP_PROTOCOL })
            .send()
            .await
        {
            Ok(response) if response.status().is_success() => {
                let created: InstallCreated = response
                    .json()
                    .await
                    .map_err(|e| format!("invalid backend response: {e}"))?;
                if created.app_version.trim().is_empty() {
                    return Err("backend response missing app_version".into());
                }
                println!("      target version: {}", created.app_version);
                println!("      cloud job: {} ({})", created.job_id, created.status);
                return Ok(created);
            }
            Ok(response) => {
                let code = response.status();
                let retryable = code.as_u16() == 429 || code.as_u16() == 502 || code.as_u16() == 503 || code.as_u16() == 504;
                let body = response.text().await.unwrap_or_default();
                last_error = format!("HTTP {code} {body}");
                if !retryable {
                    return Err(format!("backend rejected install request: {last_error}"));
                }
            }
            Err(e) => {
                last_error = format!("{e}");
            }
        }
        if attempt < 12 {
            println!("      backend 正在喚醒/重試 ({attempt}/12)...");
            tokio::time::sleep(Duration::from_secs(8)).await;
        }
    }
    Err(format!("backend 目前無法連線；已自動重試。最後錯誤：{last_error}"))
}

async fn wait_cloud_ipa(client: &reqwest::Client, base: &str, job_id: &str, expected_version: &str) -> Result<Vec<u8>, String> {
    println!("[4/7] 等待 signed IPA...");
    let endpoint = format!("{base}/api/v1/install/{job_id}");
    let mut last_status = String::new();
    for _ in 0..900 {
        let response = client.get(&endpoint).send().await.map_err(|e| format!("backend status failed: {e}"))?;
        if !response.status().is_success() {
            let code = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(format!("backend status HTTP {code}: {body}"));
        }
        let status: InstallStatus = response.json().await.map_err(|e| format!("invalid backend status: {e}"))?;
        if let Some(ref status_version) = status.app_version {
            if status_version != expected_version {
                return Err(format!("backend changed app version during job: expected {expected_version}, got {status_version}"));
            }
        }
        if status.status != last_status {
            println!("      status: {}{}", status.status,
                status.message.as_deref().map(|m| format!(" — {m}")).unwrap_or_default());
            last_status = status.status.clone();
        }
        match status.status.as_str() {
            "ready" => {
                let url = status.download_url.ok_or("ready response missing download_url")?;
                let expected = status.sha256.ok_or("ready response missing sha256")?.to_ascii_lowercase();
                let resp = client.get(url).send().await.map_err(|e| format!("IPA download failed: {e}"))?;
                if !resp.status().is_success() { return Err(format!("IPA download HTTP {}", resp.status())); }
                let bytes = resp.bytes().await.map_err(|e| format!("read IPA download: {e}"))?.to_vec();
                if bytes.len() < 1024 * 1024 { return Err(format!("downloaded IPA too small: {} bytes", bytes.len())); }
                let got = format!("{:x}", Sha256::digest(&bytes));
                if got != expected { return Err(format!("IPA SHA256 mismatch: expected {expected}, got {got}")); }
                println!("      ✅ IPA ready ({} bytes, SHA256 verified)", bytes.len());
                return Ok(bytes);
            }
            "failed" => return Err(status.message.unwrap_or_else(|| "cloud provisioning failed".into())),
            "apple_pending" => {
                return Err(status.message.unwrap_or_else(|| "Apple 尚未允許此裝置進入 provisioning；稍後直接重跑 Setup。".into()));
            }
            _ => tokio::time::sleep(Duration::from_secs(4)).await,
        }
    }
    Err("等待 signed IPA 超時；請稍後直接重新執行 Setup。".into())
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
    println!("[3/7] 建立這台裝置專屬 RPPairing...");
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

async fn install_or_upgrade(provider: &dyn IdeviceProvider, udid: &str, ipa: &[u8], app_version: &str) -> Result<(), String> {
    if ipa.len() < 1024 * 1024 {
        return Err("Backend 回傳的 Pikmin Pilot IPA 無效。".into());
    }

    println!("[5/7] 安裝 Pikmin Pilot {app_version}...");
    let installed = app_is_installed(provider).await.unwrap_or(false);
    let result = if installed {
        println!("      已存在 Pikmin Pilot，執行 upgrade（保留 App container）...");
        installation::upgrade_bytes_with_callback(
            provider,
            ipa,
            None,
            |(percent, ())| async move { println!("      install progress: {percent}%"); },
            (),
        )
        .await
    } else {
        installation::install_bytes_with_callback(
            provider,
            ipa,
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
                    "iOS 拒絕安裝：這台裝置 UDID 很可能尚未包含在目前 Development profiles。\nUDID={udid}\nUniversal backend 已嘗試自動 provisioning；若仍失敗請保留錯誤訊息供 owner 檢查。\n原始錯誤：{text}"
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
    println!("[6/7] 把 pairing 自動寫進 Pikmin Pilot...");

    // IMPORTANT: the known-good Pikmin Pilot IPA intentionally does not advertise
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

fn print_success(udid: &str, app_version: &str) {
    println!("[7/7] 完成\n");
    println!("✅ Pikmin Pilot {app_version} 已安裝");
    println!("✅ Device UDID: {udid}");
    println!("✅ signed IPA SHA256 已驗證");
    println!("✅ 此裝置專屬 RPPairing 已建立");
    println!("✅ {PAIRING_NAME} 已寫入 App Documents 並 read-back 驗證");
    println!("\n現在直接在 iPhone/iPad 打開 Pikmin Pilot 即可。\n");
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

    let base = backend_url().unwrap_or_else(|e| fail(args.no_pause, 11, e));
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(60))
        .timeout(Duration::from_secs(180))
        .user_agent("PikminPilotSetup/render-stable-1")
        .build().unwrap_or_else(|e| fail(args.no_pause, 11, format!("HTTP client: {e}")));

    let cloud = request_cloud_job(&client, &base, &device.udid).await
        .unwrap_or_else(|e| fail(args.no_pause, 12, e));
    let app_version = cloud.app_version.clone();
    let job_id = cloud.job_id;

    // Keep RC3's hardware-validated pairing engine unchanged while cloud provisioning proceeds.
    let pairing_bytes = run_known_good_pair_helper(&device.udid).await
        .unwrap_or_else(|e| fail(args.no_pause, 20, format!("{e}\n保持 USB 連接、裝置解鎖並已按『信任』後重跑 Setup。")));

    let ipa = wait_cloud_ipa(&client, &base, &job_id, &app_version).await
        .unwrap_or_else(|e| fail(args.no_pause, 25, e));

    let provider = device.to_provider(UsbmuxdAddr::default(), "PikminPilotSetup");
    if let Err(e) = install_or_upgrade(&provider, &device.udid, &ipa, &app_version).await { fail(args.no_pause, 30, e); }
    if let Err(e) = inject_pairing(&provider, &pairing_bytes).await {
        fail(args.no_pause, 40, format!("{e}\nApp 已安裝、pairing 也已備份；保持 USB 連接後直接重跑 Setup。"));
    }
    print_success(&device.udid, &app_version);
    pause_if_needed(args.no_pause);
}
