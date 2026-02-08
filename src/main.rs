mod player;
mod ui;

use std::path::PathBuf;

fn main() -> anyhow::Result<()> {
    // Optional argument: if provided, must be a valid directory.
    // If not provided, start in the current directory.
    let arg = std::env::args().nth(1).map(PathBuf::from);
    let start_path = match arg {
        Some(p) => {
            if p.is_dir() {
                std::fs::canonicalize(p)?
            } else {
                return Err(anyhow::anyhow!("Argument must be a valid directory"));
            }
        }
        None => std::fs::canonicalize(std::env::current_dir()?)?,
    };

    ui::run(&start_path)
}
