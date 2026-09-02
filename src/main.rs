mod cgi;
mod config;
mod error_page;
mod fs_util;
mod http;
mod router;
mod server;
mod session;
mod upload;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let config_path = args.get(1).map(String::as_str).unwrap_or("config/default.conf");

    let cfg = match config::parse_file(config_path) {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("localhost: configuration error: {}", e);
            std::process::exit(1);
        }
    };

    println!(
        "localhost: parsed `{}` — {} server block(s), {} listener(s) requested",
        config_path,
        cfg.servers.len(),
        cfg.all_listens().len()
    );

    if let Err(e) = server::run(cfg) {
        eprintln!("localhost: fatal: {}", e);
        std::process::exit(1);
    }
}
