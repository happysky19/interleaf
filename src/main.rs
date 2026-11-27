fn run() -> anyhow::Result<()> {
    let input = std::io::stdin();
    let output = std::io::stdout();
    match std::env::var_os("OL_SESSION_FD") {
        None => interleaf::serve(input.lock(), output.lock()),
        Some(value) => {
            #[cfg(unix)]
            {
                let fd = value
                    .to_str()
                    .and_then(|s| s.parse::<i32>().ok())
                    .ok_or_else(|| anyhow::anyhow!("invalid OL_SESSION_FD"))?;
                let receipts = std::env::var_os("INTERLEAF_NATIVE_RECEIPTS")
                    .ok_or_else(|| anyhow::anyhow!("the launcher must set INTERLEAF_NATIVE_RECEIPTS to private worker state outside the paper checkout"))?;
                let native = interleaf::NativeSession::from_inherited_socket(fd, receipts.into())?;
                interleaf::serve_with_native(input.lock(), output.lock(), native)
            }
            #[cfg(not(unix))]
            {
                let _ = value;
                anyhow::bail!("inherited native sessions require macOS or Linux")
            }
        }
    }
}

fn main() -> std::process::ExitCode {
    if std::env::args_os().len() > 1 {
        eprintln!(
            "interleaf {}: an MCP server on stdio; start it from an MCP client",
            interleaf::VERSION
        );
        return std::process::ExitCode::from(2);
    }
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("interleaf: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}
