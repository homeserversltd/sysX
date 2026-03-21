use std::env;

fn main() {
    println!("sysx-cli stub - built successfully");
    println!("SysX control socket client (per canonical spec §2)");
    env::set_var("RUST_BACKTRACE", "1");
}
