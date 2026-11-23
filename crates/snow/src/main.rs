use mimalloc::MiMalloc;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

// Return from main instead of calling `process::exit`: on Windows that is
// `ExitProcess`, which skips the CRT atexit handlers that write PGO profiles.
fn main() -> std::process::ExitCode {
    let mut argv: Vec<String> = std::env::args().skip(1).collect();
    let agent = snow_plugins::app::take_agent_flag(&mut argv);
    // Release builds in a terminal: a one-line notice when a newer release exists.
    let check = snow_plugins::update::start(&argv, agent);
    let code = snow_plugins::run(argv, agent);
    snow_plugins::update::finish(check);
    // Truncated to 8 bits, as `exit(code)` is on Unix.
    std::process::ExitCode::from(code as u8)
}
