use super::codec::JsonCodec;
use anyhow::Context;
use futures::SinkExt;
use serde::Serialize;
use serde_json::json;
use std::sync::Arc;
use tokio::io::AsyncWrite;
use tokio::sync::mpsc;
use tokio::sync::Mutex;
use tokio_util::codec::FramedWrite;

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "lowercase")]
struct LogEntry {
    level: LogLevel,
    message: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "lowercase")]
enum LogLevel {
    Debug,
    Info,
    Warn,
    Error,
}

impl From<tracing::Level> for LogLevel {
    fn from(lvl: tracing::Level) -> Self {
        match lvl {
            tracing::Level::ERROR => LogLevel::Error,
            tracing::Level::WARN => LogLevel::Warn,
            tracing::Level::INFO => LogLevel::Info,
            tracing::Level::DEBUG | tracing::Level::TRACE => LogLevel::Debug,
        }
    }
}

/// Start a listener that receives incoming log events, and then
/// writes them out to `stdout`, after wrapping them in a valid
/// JSON-RPC notification object.
fn start_writer<O>(out: Arc<Mutex<FramedWrite<O, JsonCodec>>>) -> mpsc::UnboundedSender<LogEntry>
where
    O: AsyncWrite + Send + Unpin + 'static,
{
    let (sender, mut receiver) = mpsc::unbounded_channel::<LogEntry>();
    tokio::spawn(async move {
        while let Some(i) = receiver.recv().await {
            // We continue draining the queue, even if we get some
            // errors when forwarding. Forwarding could break due to
            // an interrupted connection or stdout being closed, but
            // keeping the messages in the queue is a memory leak.
            let payload = json!({
                "jsonrpc": "2.0",
                "method": "log",
                "params": i
            });

            let _ = out.lock().await.send(payload).await;
        }
    });
    sender
}

/// Initialize the logger starting a flusher to the passed in sink.
pub async fn init<O>(out: Arc<Mutex<FramedWrite<O, JsonCodec>>>) -> Result<(), anyhow::Error>
where
    O: AsyncWrite + Send + Unpin + 'static,
{
    return trace::init(out).context("initializing tracing logger");
}

mod trace {
    use std::collections::HashMap;

    use super::*;
    use tracing::span;
    use tracing::Level;
    use tracing_subscriber::prelude::*;
    use tracing_subscriber::Layer;

    /// Initialize the logger starting a flusher to the passed in sink.
    pub fn init<O>(out: Arc<Mutex<FramedWrite<O, JsonCodec>>>) -> Result<(), anyhow::Error>
    where
        O: AsyncWrite + Send + Unpin + 'static,
    {
        let filter = tracing_subscriber::filter::EnvFilter::from_env("CLN_PLUGIN_LOG");
        let sender = start_writer(out);

        tracing_subscriber::registry()
            .with(filter)
            .with(LoggingLayer::new(sender))
            .init();

        Ok(())
    }

    struct LoggingLayer {
        spans: std::sync::Mutex<HashMap<span::Id, Option<String>>>,
        context: std::sync::Mutex<HashMap<span::Id, String>>,
        sender: mpsc::UnboundedSender<LogEntry>,
    }

    impl LoggingLayer {
        fn new(sender: mpsc::UnboundedSender<LogEntry>) -> Self {
            LoggingLayer {
                sender,
                spans: std::sync::Mutex::new(HashMap::new()),
                context: std::sync::Mutex::new(HashMap::new()),
            }
        }
    }

    impl<S> Layer<S> for LoggingLayer
    where
        S: tracing::Subscriber,
    {
        fn on_new_span(
            &self,
            attrs: &span::Attributes<'_>,
            id: &span::Id,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            let mut extractor = LogExtract::default();
            attrs.record(&mut extractor);
            let mut spans = self.spans.lock().unwrap();
            spans.insert(id.clone(), extractor.msg);
        }

        fn on_close(&self, id: span::Id, _ctx: tracing_subscriber::layer::Context<'_, S>) {
            self.spans.lock().unwrap().remove(&id);
            self.context.lock().unwrap().remove(&id);
        }

        fn on_enter(&self, id: &span::Id, _ctx: tracing_subscriber::layer::Context<'_, S>) {
            if let Some(span) = self.spans.lock().unwrap().get(id) {
                if let Some(span) = span {
                    self.context
                        .lock()
                        .unwrap()
                        .insert(id.clone(), span.clone());
                }
            }
        }

        fn on_exit(&self, id: &span::Id, _ctx: tracing_subscriber::layer::Context<'_, S>) {
            self.context.lock().unwrap().remove(id);
        }

        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            let mut extractor = LogExtract::default();
            event.record(&mut extractor);
            let mut message = match extractor.msg {
                Some(m) => m,
                None => return,
            };
            let spanmsg = self
                .context
                .lock()
                .unwrap()
                .iter()
                .map(|f| f.1)
                .map(|f| f.clone())
                .collect::<Vec<String>>()
                .join(", ");
            if spanmsg != "" {
                message.push_str(", ");
                message.push_str(&spanmsg);
            }
            let level = event.metadata().level().into();
            self.sender.send(LogEntry { level, message }).unwrap();
        }
    }

    impl From<&Level> for LogLevel {
        fn from(l: &Level) -> LogLevel {
            match l {
                &Level::DEBUG => LogLevel::Debug,
                &Level::ERROR => LogLevel::Error,
                &Level::INFO => LogLevel::Info,
                &Level::WARN => LogLevel::Warn,
                &Level::TRACE => LogLevel::Debug,
            }
        }
    }

    /// Extracts the message from the visitor
    #[derive(Default)]
    struct LogExtract {
        msg: Option<String>,
    }

    impl tracing::field::Visit for LogExtract {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            if let Some(msg) = &self.msg {
                self.msg = Some(format!("{}, {}: {:?}", msg, field.name(), value));
            } else {
                self.msg = Some(format!("{}: {:?}", field.name(), value))
            }
        }
    }
}
