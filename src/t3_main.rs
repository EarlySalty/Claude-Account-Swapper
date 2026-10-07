fn main() {
    let outcome = claude_account_swapper::App::discover().and_then(|app| {
        claude_account_swapper::bridge::run(&app, std::env::args().skip(1).collect())
    });
    match outcome {
        Ok(code) => std::process::exit(code),
        Err(error) => {
            eprintln!("Claude Account Launcher: {error:#}");
            std::process::exit(1);
        }
    }
}
