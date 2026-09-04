//! Dumps the embedded prompt text so it can be diffed against the Swift source.
//!
//! `cargo run -p freeflow-core --example dump_prompts -- cleanup|edit`
fn main() {
    let which = std::env::args().nth(1).unwrap_or_default();
    print!(
        "{}",
        match which.as_str() {
            "edit" => freeflow_core::prompt_text::EDIT_MODE_SYSTEM_PROMPT,
            _ => freeflow_core::prompt_text::CLEANUP_SYSTEM_PROMPT,
        }
    );
}
