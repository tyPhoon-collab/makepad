use std::path::Path;
use std::process::ExitCode;

fn main() -> ExitCode {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    makepad_cef_fetch::run_from_workspace(&workspace)
}
