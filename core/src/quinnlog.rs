//! What QUIC's own code warns of, in the node's log. quinn and quinn-udp
//! report trouble with the socket (a send the system refused, and why,
//! with the address and interface it was from and to; a receive that
//! failed) only through `tracing`, which nothing here listened to: those
//! packets were dropped without a word. Warnings and errors from them are
//! passed on, each line as it comes (quinn-udp says a refused send at
//! most once a minute itself).

use std::sync::OnceLock;

use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Level, Metadata, Subscriber};

/// Where the lines go: the first node's log (one subscriber per process;
/// a second node, as in tests, keeps the first's).
static LOG: OnceLock<crate::node::Logger> = OnceLock::new();

/// Starts passing quinn's warnings to `log`, once per process.
pub fn forward(log: crate::node::Logger) {
    if LOG.set(log).is_ok() {
        let _ = tracing::subscriber::set_global_default(Forward);
    }
}

struct Forward;

fn wanted(meta: &Metadata<'_>) -> bool {
    *meta.level() <= Level::WARN && meta.target().starts_with("quinn")
}

impl Subscriber for Forward {
    fn enabled(&self, meta: &Metadata<'_>) -> bool {
        wanted(meta)
    }

    fn max_level_hint(&self) -> Option<tracing::level_filters::LevelFilter> {
        Some(tracing::level_filters::LevelFilter::WARN)
    }

    fn register_callsite(&self, meta: &'static Metadata<'static>) -> tracing::subscriber::Interest {
        if wanted(meta) { tracing::subscriber::Interest::always() } else { tracing::subscriber::Interest::never() }
    }

    fn new_span(&self, _: &Attributes<'_>) -> Id {
        Id::from_u64(1)
    }

    fn record(&self, _: &Id, _: &Record<'_>) {}

    fn record_follows_from(&self, _: &Id, _: &Id) {}

    fn event(&self, event: &Event<'_>) {
        if !wanted(event.metadata()) {
            return;
        }
        let mut text = Text(String::new());
        event.record(&mut text);
        if let Some(log) = LOG.get() {
            log(format!("QUIC ({}): {}", event.metadata().target(), text.0.trim()));
        }
    }

    fn enter(&self, _: &Id) {}

    fn exit(&self, _: &Id) {}
}

/// An event's fields as one line: its message, then `name=value` for the
/// rest.
struct Text(String);

impl Visit for Text {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.0.insert_str(0, &format!("{value:?} "));
        } else {
            self.0 += &format!("{}={value:?} ", field.name());
        }
    }
}
