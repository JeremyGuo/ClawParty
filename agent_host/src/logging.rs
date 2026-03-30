use anyhow::{Context, Result};
use serde_json::{Map, Number, Value};
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{Event, Subscriber};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::Layer;
use tracing_subscriber::fmt;
use tracing_subscriber::layer::{Context as LayerContext, SubscriberExt};
use tracing_subscriber::util::SubscriberInitExt;

pub fn init_logging(workdir: &Path) -> Result<()> {
    let logs_root = workdir.join("logs");
    fs::create_dir_all(logs_root.join("channels"))
        .with_context(|| format!("failed to create {}", logs_root.join("channels").display()))?;
    fs::create_dir_all(logs_root.join("sessions"))
        .with_context(|| format!("failed to create {}", logs_root.join("sessions").display()))?;
    fs::create_dir_all(logs_root.join("agents"))
        .with_context(|| format!("failed to create {}", logs_root.join("agents").display()))?;

    let routing_layer = JsonlRoutingLayer::new(logs_root);
    let fmt_layer = fmt::layer()
        .with_target(false)
        .with_writer(std::io::stderr)
        .compact();
    let filter = EnvFilter::builder()
        .with_default_directive(tracing_subscriber::filter::LevelFilter::INFO.into())
        .from_env_lossy();

    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(fmt_layer)
        .with(routing_layer)
        .try_init();
    Ok(())
}

#[derive(Clone)]
struct JsonlRoutingLayer {
    state: Arc<RoutingState>,
}

struct RoutingState {
    logs_root: PathBuf,
    writers: Mutex<HashMap<PathBuf, BufWriter<File>>>,
}

impl JsonlRoutingLayer {
    fn new(logs_root: PathBuf) -> Self {
        Self {
            state: Arc::new(RoutingState {
                logs_root,
                writers: Mutex::new(HashMap::new()),
            }),
        }
    }

    fn append_record(&self, path: PathBuf, value: &Value) {
        let mut writers = match self.state.writers.lock() {
            Ok(guard) => guard,
            Err(_) => return,
        };
        let writer = match writers.entry(path.clone()) {
            std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
            std::collections::hash_map::Entry::Vacant(entry) => {
                let file = match OpenOptions::new().create(true).append(true).open(&path) {
                    Ok(file) => file,
                    Err(_) => return,
                };
                entry.insert(BufWriter::new(file))
            }
        };
        if let Ok(line) = serde_json::to_string(value) {
            let _ = writer.write_all(line.as_bytes());
            let _ = writer.write_all(b"\n");
            let _ = writer.flush();
        }
    }

    fn route_path(&self, stream: Option<&str>, key: Option<&str>) -> Option<PathBuf> {
        match (stream, key) {
            (Some("channel"), Some(key)) => Some(
                self.state
                    .logs_root
                    .join("channels")
                    .join(format!("{}.jsonl", sanitize_component(key))),
            ),
            (Some("session"), Some(key)) => Some(
                self.state
                    .logs_root
                    .join("sessions")
                    .join(format!("{}.jsonl", sanitize_component(key))),
            ),
            (Some("agent"), Some(key)) => Some(
                self.state
                    .logs_root
                    .join("agents")
                    .join(format!("{}.jsonl", sanitize_component(key))),
            ),
            _ => None,
        }
    }
}

impl<S> Layer<S> for JsonlRoutingLayer
where
    S: Subscriber,
{
    fn on_event(&self, event: &Event<'_>, _ctx: LayerContext<'_, S>) {
        let metadata = event.metadata();
        let mut visitor = JsonVisitor::default();
        event.record(&mut visitor);

        let stream = visitor.get_string("log_stream");
        let key = visitor.get_string("log_key");

        let mut object = Map::new();
        object.insert("ts".to_string(), Value::Number(now_millis()));
        object.insert(
            "level".to_string(),
            Value::String(metadata.level().as_str().to_string()),
        );
        object.insert(
            "target".to_string(),
            Value::String(metadata.target().to_string()),
        );
        if let Some(module_path) = metadata.module_path() {
            object.insert(
                "module_path".to_string(),
                Value::String(module_path.to_string()),
            );
        }
        if let Some(file) = metadata.file() {
            object.insert("file".to_string(), Value::String(file.to_string()));
        }
        if let Some(line) = metadata.line() {
            object.insert("line".to_string(), Value::Number(Number::from(line as u64)));
        }
        object.extend(visitor.fields);
        let value = Value::Object(object);

        self.append_record(self.state.logs_root.join("server.log"), &value);
        if let Some(path) = self.route_path(stream.as_deref(), key.as_deref()) {
            self.append_record(path, &value);
        }
    }
}

#[derive(Default)]
struct JsonVisitor {
    fields: Map<String, Value>,
}

impl JsonVisitor {
    fn get_string(&self, key: &str) -> Option<String> {
        match self.fields.get(key) {
            Some(Value::String(value)) => Some(value.clone()),
            Some(other) => Some(other.to_string()),
            None => None,
        }
    }
}

impl tracing::field::Visit for JsonVisitor {
    fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
        self.fields
            .insert(field.name().to_string(), Value::Number(Number::from(value)));
    }

    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        self.fields
            .insert(field.name().to_string(), Value::Number(Number::from(value)));
    }

    fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
        self.fields
            .insert(field.name().to_string(), Value::Bool(value));
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.fields
            .insert(field.name().to_string(), Value::String(value.to_string()));
    }

    fn record_f64(&mut self, field: &tracing::field::Field, value: f64) {
        if let Some(number) = Number::from_f64(value) {
            self.fields
                .insert(field.name().to_string(), Value::Number(number));
        } else {
            self.fields
                .insert(field.name().to_string(), Value::String(value.to_string()));
        }
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.fields.insert(
            field.name().to_string(),
            Value::String(format!("{:?}", value)),
        );
    }
}

fn now_millis() -> Number {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    Number::from(millis as u64)
}

fn sanitize_component(value: &str) -> String {
    let sanitized: String = value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
                ch
            } else {
                '_'
            }
        })
        .collect();
    if sanitized.is_empty() {
        "default".to_string()
    } else {
        sanitized
    }
}
