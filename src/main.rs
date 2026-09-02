mod http;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let config_path = args.get(1).map(String::as_str).unwrap_or("config/default.conf");

    println!("localhost: starting up with config `{}`", config_path);
    println!("localhost: not implemented yet");
}
