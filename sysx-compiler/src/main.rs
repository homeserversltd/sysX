//! SysX Compiler CLI
//!
//! Usage:
//!   sysxc compile <service.yaml>
//!   sysxc forge-core <output.bin> <admin_gid>

use std::env;
use sysx_compiler::{compile_service, forge_core_bin};

fn main() {
    let args: Vec<String> = env::args().collect();

    if args.len() < 2 {
        eprintln!("Usage:");
        eprintln!("  {} compile <service.yaml>", args[0]);
        eprintln!("  {} forge-core <output.bin> <admin_gid>", args[0]);
        std::process::exit(1);
    }

    let result = match args[1].as_str() {
        "compile" => {
            if args.len() < 3 {
                eprintln!("Usage: {} compile <service.yaml>", args[0]);
                std::process::exit(1);
            }
            match compile_service(&args[2]) {
                Ok(schema) => {
                    println!("✓ Compiled schema for service: {}", schema.service.name);
                    println!("  enabled: {}", schema.enabled);
                    println!("  exec: {}", schema.service.exec);
                    Ok(())
                }
                Err(e) => Err(e),
            }
        }
        "forge-core" => {
            if args.len() < 4 {
                eprintln!("Usage: {} forge-core <output.bin> <admin_gid>", args[0]);
                std::process::exit(1);
            }
            let admin_gid = args[3].parse::<u32>().unwrap_or(1000);
            match forge_core_bin(&args[2], admin_gid) {
                Ok(_) => {
                    println!("✓ Forged 32-byte SysxCoreConfig to {}", args[2]);
                    Ok(())
                }
                Err(e) => Err(e),
            }
        }
        _ => {
            eprintln!("Unknown command: {}", args[1]);
            std::process::exit(1);
        }
    };

    if let Err(e) = result {
        eprintln!("Error: {}", e);
        std::process::exit(1);
    }
}
