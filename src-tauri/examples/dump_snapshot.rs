use std::error::Error;
use std::path::PathBuf;

use gtui_lib::config::{default_gt_root, install_default_tool_path};
use gtui_lib::snapshot::build_snapshot;

#[derive(Debug)]
struct Options {
    root: PathBuf,
    compact: bool,
    out: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    install_default_tool_path();
    let options = match parse_args() {
        Ok(Some(options)) => options,
        Ok(None) => return Ok(()),
        Err(message) => {
            eprintln!("{message}");
            eprintln!();
            print_usage();
            std::process::exit(2);
        }
    };

    let snapshot = build_snapshot(&options.root, &[]).await;
    let encoded = if options.compact {
        serde_json::to_string(&snapshot)?
    } else {
        serde_json::to_string_pretty(&snapshot)?
    };

    if let Some(out) = options.out {
        if let Some(parent) = out.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        std::fs::write(out, encoded)?;
    } else {
        println!("{encoded}");
    }
    Ok(())
}

fn parse_args() -> Result<Option<Options>, String> {
    let mut root: Option<PathBuf> = None;
    let mut compact = false;
    let mut out: Option<PathBuf> = None;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                print_usage();
                return Ok(None);
            }
            "--compact" => compact = true,
            "--root" => {
                let value = args
                    .next()
                    .ok_or_else(|| "--root requires a path".to_string())?;
                root = Some(PathBuf::from(value));
            }
            "--out" => {
                let value = args
                    .next()
                    .ok_or_else(|| "--out requires a path".to_string())?;
                out = Some(PathBuf::from(value));
            }
            _ => return Err(format!("unknown argument: {arg}")),
        }
    }

    Ok(Some(Options {
        root: root.unwrap_or_else(default_gt_root),
        compact,
        out,
    }))
}

fn print_usage() {
    eprintln!(
        "Usage: dump_snapshot [--root PATH] [--compact] [--out PATH]\n\
\n\
Builds one live GTUI workspace snapshot and prints the JSON payload that the\n\
frontend receives from get_snapshot after a refresh."
    );
}
