//! Local interchange/recovery utility: cargo run -p frame-metadata --example events
//! -- export|import|recover <absolute-source> <absolute-new-output>, or inspect <source>.
use std::path::PathBuf;
fn main() -> std::io::Result<()> {
    let mut args = std::env::args_os().skip(1);
    let action = args.next().unwrap_or_default();
    let source = PathBuf::from(args.next().unwrap_or_default());
    let summary = if action == "inspect" {
        frame_metadata::read_sidecar(&source, |record| {
            println!("{}", serde_json::to_string(record)?);
            Ok(())
        })?
    } else {
        let output = PathBuf::from(args.next().unwrap_or_default());
        if !output.is_absolute() || args.next().is_some() {
            return Err(std::io::Error::other("Choose a new absolute output path."));
        }
        match action.to_str() {
            Some("export") => frame_metadata::export_jsonl(&source, &output)?,
            Some("import") => frame_metadata::import_jsonl(&source, &output)?,
            Some("recover") => frame_metadata::recover_sidecar(&source, &output)?,
            _ => {
                return Err(std::io::Error::other(
                    "Use inspect, export, import or recover.",
                ));
            }
        }
    };
    eprintln!("{summary:?}");
    Ok(())
}
