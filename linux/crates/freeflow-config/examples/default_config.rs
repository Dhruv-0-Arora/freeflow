//! Prints a fully-populated config file showing every setting at its default.
//!
//! `cargo run -p freeflow-config --example default_config > ~/.config/freeflow/config.toml`

fn main() {
    print!(
        "{}",
        toml::to_string_pretty(&freeflow_config::Config::default())
            .expect("the default config is serializable")
    );
}
