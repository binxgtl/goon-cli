fn main() {
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("tokio runtime");
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let code = runtime.block_on(goon::cli::main(argv));
    runtime.shutdown_timeout(std::time::Duration::from_millis(500));
    std::process::exit(code);
}
