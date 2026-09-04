//! `freeflow-overlay` - the recording indicator.
//!
//! Replaces `Sources/RecordingOverlay.swift`, which built a borderless
//! `NSPanel` at screen-saver level and did a lot of work to render around the
//! MacBook notch. None of that applies here; a `wlr-layer-shell` surface is
//! the direct equivalent and is considerably simpler.
//!
//! Runs as a **separate process** from the daemon, talking over the same
//! control socket. That is deliberate: mixing a GTK main loop with the
//! daemon's async runtime is a known source of trouble, and keeping the daemon
//! free of GUI dependencies is what lets the whole pipeline be driven from a
//! script with no display attached.
//!
//! The socket is polled from a GLib timeout using a blocking read. The socket
//! is local and the exchange is one line each way, so this costs far less than
//! bridging two event loops would.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use freeflow_core::proto::{encode, DaemonState, Request, Response};
use gtk4::prelude::*;
use gtk4::{glib, Align, Application, ApplicationWindow, Box as GtkBox, Label, Orientation};
use gtk4_layer_shell::{Edge, Layer, LayerShell};

/// How often to ask the daemon what it is doing.
///
/// Fast enough that the level meter looks live, slow enough to stay
/// invisible in a CPU profile.
const POLL: Duration = Duration::from_millis(66);

fn query(request: &Request) -> Option<Response> {
    let socket = freeflow_config::paths::control_socket().ok()?;
    let mut stream = UnixStream::connect(socket).ok()?;
    stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .ok()?;
    stream.write_all(encode(request).as_bytes()).ok()?;
    stream.flush().ok()?;

    let mut line = String::new();
    BufReader::new(&stream).read_line(&mut line).ok()?;
    serde_json::from_str(&line).ok()
}

/// Renders the level as a fixed-width bar, as Pango markup.
///
/// Filled and empty segments are coloured separately. With one colour the bar
/// reads as a texture rather than a measurement, and the whole point of it is
/// to answer "is the microphone hearing me" at a glance - which is the fastest
/// way to notice the wrong capture device is selected.
///
/// Speech peaks well below 1.0 in practice, so the scale is expanded. This is
/// an indicator, not an accurate meter.
fn level_bar(level: f32) -> String {
    const WIDTH: usize = 16;
    let filled = ((level * 4.0).min(1.0) * WIDTH as f32).round() as usize;
    format!(
        "<span foreground=\"#6fc38a\">{}</span><span foreground=\"#3a3a40\">{}</span>",
        "\u{2588}".repeat(filled),
        "\u{2588}".repeat(WIDTH - filled)
    )
}

fn state_colour(state: DaemonState) -> &'static str {
    match state {
        DaemonState::Recording => "#e05252",
        DaemonState::Transcribing | DaemonState::Cleaning => "#e0a052",
        DaemonState::Injecting => "#52c07a",
        DaemonState::Error => "#e05252",
        DaemonState::Idle => "#8a8a8a",
    }
}

fn build(app: &Application) {
    let window = ApplicationWindow::builder()
        .application(app)
        .decorated(false)
        .resizable(false)
        .build();

    window.init_layer_shell();
    // Above normal windows but below lock screens.
    window.set_layer(Layer::Overlay);
    // Never take keyboard focus: the user is mid-dictation and the keystrokes
    // belong to whatever they were typing into.
    window.set_keyboard_mode(gtk4_layer_shell::KeyboardMode::None);
    // -1 means "do not reserve space", so a tiling layout is not reflowed
    // every time the overlay appears.
    window.set_exclusive_zone(-1);
    window.set_anchor(Edge::Bottom, true);
    window.set_margin(Edge::Bottom, 64);

    let container = GtkBox::new(Orientation::Horizontal, 12);
    container.set_margin_top(10);
    container.set_margin_bottom(10);
    container.set_margin_start(18);
    container.set_margin_end(18);
    container.set_halign(Align::Center);

    let dot = Label::new(Some("\u{25cf}"));
    let status = Label::new(Some("Ready"));
    let meter = Label::new(Some(""));
    meter.add_css_class("meter");
    container.append(&dot);
    container.append(&status);
    container.append(&meter);
    window.set_child(Some(&container));

    let css = gtk4::CssProvider::new();
    css.load_from_string(
        "window { background: rgba(24,24,28,0.92); border-radius: 18px; }
         label { color: #e8e8ea; font-family: monospace; font-size: 14px; }
         .meter { color: #6fc38a; letter-spacing: 1px; }",
    );
    if let Some(display) = gtk4::gdk::Display::default() {
        gtk4::style_context_add_provider_for_display(
            &display,
            &css,
            gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }

    // Start hidden; the overlay should only appear once dictation begins.
    window.set_visible(false);

    glib::timeout_add_local(POLL, move || {
        match query(&Request::Status) {
            Some(response) => {
                let state = response.state;
                if state == DaemonState::Idle {
                    window.set_visible(false);
                    return glib::ControlFlow::Continue;
                }

                status.set_text(response.message.as_deref().unwrap_or_else(|| state.label()));
                dot.set_markup(&format!(
                    "<span foreground=\"{}\">\u{25cf}</span>",
                    state_colour(state)
                ));
                match response.level {
                    Some(level) => meter.set_markup(&level_bar(level)),
                    None => meter.set_text(""),
                }
                window.set_visible(true);
            }
            // The daemon may not be running yet, or may be restarting. That is
            // not the overlay's problem; keep polling quietly.
            None => window.set_visible(false),
        }
        glib::ControlFlow::Continue
    });
}

fn main() -> glib::ExitCode {
    let app = Application::builder()
        .application_id("com.zachlatta.freeflow.overlay")
        .build();
    app.connect_activate(build);
    // The overlay takes no arguments; passing GTK the real argv would let it
    // try to interpret them.
    app.run_with_args::<&str>(&[])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filled_blocks(markup: &str) -> usize {
        // The first span holds the filled segments.
        markup
            .split("</span>")
            .next()
            .map(|s| s.matches('\u{2588}').count())
            .unwrap_or(0)
    }

    #[test]
    fn silence_renders_an_empty_bar() {
        assert_eq!(filled_blocks(&level_bar(0.0)), 0);
    }

    #[test]
    fn a_loud_input_fills_the_bar_without_overflowing() {
        assert_eq!(filled_blocks(&level_bar(1.0)), 16);
        // Clamping matters: a hot input must not produce a longer bar that
        // reflows the overlay's width mid-recording.
        assert_eq!(filled_blocks(&level_bar(9.9)), 16);
    }

    #[test]
    fn the_bar_is_always_the_same_width() {
        for level in [0.0, 0.01, 0.1, 0.25, 0.5, 1.0, 5.0] {
            let markup = level_bar(level);
            assert_eq!(
                markup.matches('\u{2588}').count(),
                16,
                "width changed at level {level}"
            );
        }
    }

    #[test]
    fn speech_level_audio_moves_the_meter() {
        // Ambient room noise measured around 0.0015 should read as empty,
        // while ordinary speech should be clearly visible.
        assert_eq!(filled_blocks(&level_bar(0.0015)), 0);
        assert!(filled_blocks(&level_bar(0.15)) >= 8);
    }

    #[test]
    fn idle_is_the_only_state_without_a_signal_colour() {
        for state in [
            DaemonState::Recording,
            DaemonState::Transcribing,
            DaemonState::Cleaning,
            DaemonState::Injecting,
            DaemonState::Error,
        ] {
            assert_ne!(state_colour(state), state_colour(DaemonState::Idle));
        }
    }
}
