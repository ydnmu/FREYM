#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
fn main() {
    if std::env::args().nth(1).as_deref() == Some("--describe-update-config") {
        match freym_update::trust().and_then(|trust| Ok(serde_json::to_string(&trust)?)) {
            Ok(text) => {
                println!("{text}");
                return;
            }
            Err(error) => {
                eprintln!("Invalid update configuration: {error:#}");
                std::process::exit(1);
            }
        }
    }
    match freym_update::install::launch() {
        Ok(code) => std::process::exit(code),
        Err(error) => {
            eprintln!("Could not launch Freym: {error:#}");
            freym_update::show_launch_error(&error);
            std::process::exit(1);
        }
    }
}
