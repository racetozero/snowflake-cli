use mimalloc::MiMalloc;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    // Release builds in a terminal: a one-line notice when a newer release exists.
    let check = snow_plugins::update::start(&argv);
    let code = snow_plugins::run(argv);
    snow_plugins::update::finish(check);
    std::process::exit(code);
}
