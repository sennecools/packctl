use std::io::Write;
use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    match packctl::cli::run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            // Not eprintln!: a closed stderr pipe must not turn the error
            // into a panic.
            let _ = writeln!(std::io::stderr(), "error: {error}");
            ExitCode::from(error.exit_code())
        }
    }
}
