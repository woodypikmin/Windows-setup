use std::{env, fs, path::PathBuf, process};

use idevice::{
    IdeviceError, IdeviceService, RemoteXpcClient,
    core_device_proxy::CoreDeviceProxy,
    provider::IdeviceProvider,
    remote_pairing::{RemotePairingClient, RpPairingFile},
    rsd::RsdHandshake,
    usbmuxd::{Connection, UsbmuxdAddr, UsbmuxdConnection, UsbmuxdDevice},
};

fn usage() {
    eprintln!(
        "Usage:\n  PikminPilotPairingHelper.exe [--udid <UDID>] [--output <FILE>] [--hostname <NAME>]\n\nExample:\n  PikminPilotPairingHelper.exe --udid 00008103-000A78D13684C01E --output C:\\Users\\woody\\Downloads\\rp_pairing_file.plist"
    );
}

fn parse_args() -> (Option<String>, PathBuf, String) {
    let mut udid = None;
    let mut output = PathBuf::from("rp_pairing_file.plist");
    let mut hostname = String::from("Pikmin Pilot Windows Setup");
    let args: Vec<String> = env::args().collect();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--udid" => {
                i += 1;
                if i >= args.len() { usage(); process::exit(2); }
                udid = Some(args[i].clone());
            }
            "--output" => {
                i += 1;
                if i >= args.len() { usage(); process::exit(2); }
                output = PathBuf::from(&args[i]);
            }
            "--hostname" => {
                i += 1;
                if i >= args.len() { usage(); process::exit(2); }
                hostname = args[i].clone();
            }
            "-h" | "--help" => { usage(); process::exit(0); }
            other => {
                eprintln!("Unknown argument: {other}");
                usage();
                process::exit(2);
            }
        }
        i += 1;
    }
    (udid, output, hostname)
}

async fn choose_usb_device(requested_udid: Option<&str>) -> Result<UsbmuxdDevice, IdeviceError> {
    let mut mux = UsbmuxdConnection::default().await?;
    let devices: Vec<UsbmuxdDevice> = mux
        .get_devices()
        .await?
        .into_iter()
        .filter(|d| d.connection_type == Connection::Usb)
        .collect();

    if let Some(udid) = requested_udid {
        return devices
            .into_iter()
            .find(|d| d.udid == udid)
            .ok_or(IdeviceError::DeviceNotFound);
    }

    match devices.len() {
        0 => Err(IdeviceError::DeviceNotFound),
        1 => Ok(devices.into_iter().next().unwrap()),
        _ => {
            eprintln!("More than one USB iPhone/iPad is connected. Re-run with --udid:");
            for d in &devices {
                eprintln!("  {}", d.udid);
            }
            process::exit(3);
        }
    }
}

async fn generate_remote_pairing_file(
    provider: &dyn IdeviceProvider,
    hostname: &str,
) -> Result<RpPairingFile, IdeviceError> {
    eprintln!("[1/6] Connecting CoreDeviceProxy over USB...");
    let proxy = CoreDeviceProxy::connect(provider).await?;
    let rsd_port = proxy.tunnel_info().server_rsd_port;

    eprintln!("[2/6] Starting software tunnel; RSD port={rsd_port}...");
    let adapter = proxy.create_software_tunnel()?;
    let mut adapter = adapter.to_async_handle();

    eprintln!("[3/6] Performing RSD handshake...");
    let rsd_stream = adapter.connect(rsd_port).await?;
    let handshake = RsdHandshake::new(rsd_stream).await?;
    let tunnel_service = handshake
        .services
        .get("com.apple.internal.dt.coredevice.untrusted.tunnelservice")
        .ok_or_else(|| IdeviceError::InternalError("Untrusted tunnel service not found".into()))?;

    eprintln!("[4/6] Opening untrusted RemoteXPC pairing service...");
    let tunnel_service_stream = adapter.connect(tunnel_service.port).await?;
    let mut remote_xpc = RemoteXpcClient::new(tunnel_service_stream).await?;
    remote_xpc.do_handshake().await?;
    let _ = remote_xpc.recv_root().await;

    eprintln!("[5/6] Creating idevice-native RemotePairing record...");
    eprintln!("      Keep the iPhone/iPad unlocked. Accept Trust/Pairing prompts if shown.");
    let mut pairing_file = RpPairingFile::generate(hostname);
    let mut pairing_client = RemotePairingClient::new(remote_xpc, hostname);
    pairing_client
        .connect(&mut pairing_file, || async { "000000".to_string() })
        .await?;

    // idevice_pair deliberately reconnects once so iOS commits the pairing
    // record to its keychain reliably. Mirror that behavior here.
    eprintln!("[6/6] Verifying/committing pairing identity...");
    let tunnel_service_stream = adapter.connect(tunnel_service.port).await?;
    let mut remote_xpc = RemoteXpcClient::new(tunnel_service_stream).await?;
    remote_xpc.do_handshake().await?;
    let _ = remote_xpc.recv_root().await;
    let mut pairing_client = RemotePairingClient::new(remote_xpc, hostname);
    pairing_client
        .connect(&mut pairing_file, || async { "000000".to_string() })
        .await?;

    Ok(pairing_file)
}

#[tokio::main]
async fn main() {
    let (requested_udid, output, hostname) = parse_args();

    eprintln!("Pikmin Pilot Pairing Helper");
    eprintln!("Pairing host name: {hostname}");

    let device = match choose_usb_device(requested_udid.as_deref()).await {
        Ok(d) => d,
        Err(e) => {
            eprintln!("ERROR: no usable trusted USB iPhone/iPad: {e:?}");
            eprintln!("Connect the device by USB, unlock it, and tap Trust This Computer.");
            process::exit(10);
        }
    };

    eprintln!("Device UDID: {}", device.udid);
    let provider = device.to_provider(UsbmuxdAddr::default(), "PikminPilotPairingHelper");

    let pairing = match generate_remote_pairing_file(&provider, &hostname).await {
        Ok(p) => p,
        Err(e) => {
            eprintln!("PAIR FAILED: {e:?}");
            process::exit(20);
        }
    };

    let bytes = pairing.to_bytes();
    if let Some(parent) = output.parent() {
        if !parent.as_os_str().is_empty() {
            if let Err(e) = fs::create_dir_all(parent) {
                eprintln!("ERROR creating output directory: {e}");
                process::exit(30);
            }
        }
    }
    if let Err(e) = fs::write(&output, &bytes) {
        eprintln!("ERROR writing pairing file: {e}");
        process::exit(31);
    }

    println!("PAIRING OK");
    println!("UDID={}", device.udid);
    println!("OUTPUT={}", output.display());
    println!("BYTES={}", bytes.len());
    println!("Import this file into Pikmin Pilot on THIS SAME device.");
}
