//! `sysx` — thin client for `/run/sysx/control.sock` (`12-canonical-spec-v1.md` §2).

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::ExitCode;

use bytes::Bytes;
use clap::{Parser, Subcommand};
use sysx_ipc::{decode_frame, Command, Header, Message};

#[derive(Parser)]
#[command(name = "sysx", about = "SysX control socket client (binary IPC v1)")]
struct Cli {
    /// UNIX socket path (or `SYSX_SOCKET`)
    #[arg(long, default_value = "/run/sysx/control.sock", env = "SYSX_SOCKET")]
    socket: PathBuf,
    #[command(subcommand)]
    command: Sub,
}

#[derive(Subcommand)]
enum Sub {
    /// Start service (0x10); payload = UTF-8 name
    Start { name: String },
    /// Stop service (0x20)
    Stop { name: String },
    /// Status (0x30) — reply is §2.1.1 two-byte state ABI
    Status { name: String },
    /// Reset (0x40)
    Reset { name: String },
    /// Poweroff — Terminal Broadcast (0x50), empty payload
    Poweroff,
    /// Reboot (0x60), empty payload
    Reboot,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("sysx: {}", e);
            ExitCode::from(1)
        }
    }
}

fn run(cli: Cli) -> Result<(), String> {
    let (cmd, payload): (Command, Bytes) = match &cli.command {
        Sub::Start { name } => {
            validate_name(name)?;
            (Command::Start, Bytes::copy_from_slice(name.as_bytes()))
        }
        Sub::Stop { name } => {
            validate_name(name)?;
            (Command::Stop, Bytes::copy_from_slice(name.as_bytes()))
        }
        Sub::Status { name } => {
            validate_name(name)?;
            (Command::Status, Bytes::copy_from_slice(name.as_bytes()))
        }
        Sub::Reset { name } => {
            validate_name(name)?;
            (Command::Reset, Bytes::copy_from_slice(name.as_bytes()))
        }
        Sub::Poweroff => (Command::Poweroff, Bytes::new()),
        Sub::Reboot => (Command::Reboot, Bytes::new()),
    };

    let mut stream = UnixStream::connect(&cli.socket)
        .map_err(|e| format!("connect {}: {}", cli.socket.display(), e))?;

    let frame = Message::new(cmd, payload).encode();
    stream
        .write_all(&frame)
        .map_err(|e| format!("send: {}", e))?;

    let mut hdr_buf = [0u8; Header::SIZE];
    stream
        .read_exact(&mut hdr_buf)
        .map_err(|e| format!("read header: {}", e))?;

    let header = Header::decode(bytes::Bytes::copy_from_slice(&hdr_buf))
        .map_err(|e| format!("decode header: {}", e))?;

    let plen = header.payload_length as usize;
    let mut body = vec![0u8; plen];
    if plen > 0 {
        stream
            .read_exact(&mut body)
            .map_err(|e| format!("read payload: {}", e))?;
    }

    let mut full = Vec::with_capacity(Header::SIZE + plen);
    full.extend_from_slice(&hdr_buf);
    full.extend_from_slice(&body);

    let (_rh, pl) = decode_frame(&full).map_err(|e| format!("decode reply: {}", e))?;

    if pl.len() >= 2 {
        println!(
            "reply command={:?} [0x{:02x} 0x{:02x}]",
            header.command,
            pl[0],
            pl[1]
        );
        if header.command == Command::Status {
            println!("{}", explain_status_abi(pl[0], pl[1]));
        }
    } else {
        println!("reply command={:?} (short body)", header.command);
    }

    Ok(())
}

fn validate_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("service name must be non-empty".into());
    }
    Ok(())
}

fn explain_status_abi(b0: u8, b1: u8) -> String {
    let state = match b0 {
        0x00 => "Offline",
        0x01 => "Starting",
        0x02 => "Running",
        0x03 => "Sweeping",
        0x04 => "Dead",
        0x05 => "Failed",
        0x06 => "Tombstoned",
        0x07 => "Recovering",
        _ => "Unknown(primary)",
    };
    format!("  state: {} (byte0=0x{:02x} byte1=0x{:02x})", state, b0, b1)
}
