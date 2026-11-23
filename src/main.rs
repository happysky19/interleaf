fn main() -> std::process::ExitCode {
    if std::env::args_os().len() > 1 {
        eprintln!(
            "interleaf {}: an MCP server on stdio; start it from an MCP client",
            interleaf::VERSION
        );
        return std::process::ExitCode::from(2);
    }
    match interleaf::serve(std::io::stdin().lock(), std::io::stdout().lock()) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("interleaf: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}
