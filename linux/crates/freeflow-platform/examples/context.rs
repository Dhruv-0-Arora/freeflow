//! Prints the focused-window context the cleanup prompt would receive.
//!
//! Selected text is reported only by length. It is the user's live screen
//! content, and the repository's privacy rules forbid printing or persisting
//! it; knowing whether the selection was picked up is all this needs to show.
fn main() {
    let ctx = freeflow_platform::context::active_window();
    println!("{ctx:#?}");
    println!("prompt CONTEXT field: {:?}", ctx.summary());
    let selected = freeflow_platform::clipboard::read_selected_text();
    println!(
        "primary selection: {} chars{}",
        selected.chars().count(),
        if selected.trim().is_empty() {
            " (edit mode would be unavailable)"
        } else {
            ""
        }
    );
    println!(
        "wtype available: {}",
        freeflow_platform::inject::wtype_available()
    );
}
