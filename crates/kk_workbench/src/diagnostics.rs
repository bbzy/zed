use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use smol::channel::{Receiver, Sender, bounded};
use std::{
    collections::HashSet,
    sync::{Arc, Mutex, MutexGuard},
    time::Instant,
};

#[derive(Clone, Debug, Default)]
pub struct Diagnostics(Arc<Mutex<DiagnosticState>>);

#[derive(Debug, Default)]
struct DiagnosticState {
    lines: Vec<String>,
    subscribers: Vec<Sender<()>>,
    categories: HashSet<String>,
    session: Option<String>,
    sequence: u64,
    span_sequence: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PerformanceContext {
    session: String,
    trace: u64,
    span: u64,
}

pub struct PerformanceSpan {
    diagnostics: Diagnostics,
    context: Option<PerformanceContext>,
    parent: Option<u64>,
    name: String,
    fields: Value,
    started_at: String,
    started: Instant,
}

impl Diagnostics {
    pub fn configure(&self, categories: &[String]) {
        let mut state = self.state();
        state.categories = categories.iter().cloned().collect();
        if state.categories.contains("perf") {
            state
                .session
                .get_or_insert_with(|| uuid::Uuid::new_v4().to_string());
        } else {
            state.session = None;
        }
    }

    pub fn subscribe(&self) -> Receiver<()> {
        let (sender, receiver) = bounded(1);
        self.state().subscribers.push(sender);
        receiver
    }

    pub fn read_since(&self, cursor: usize) -> (usize, String) {
        let state = self.state();
        (
            state.lines.len(),
            state.lines.get(cursor..).unwrap_or_default().concat(),
        )
    }

    pub fn log(&self, message: impl AsRef<str>) {
        self.state().append(message.as_ref());
    }

    pub fn append_output(&self, text: &str) {
        if !text.is_empty() {
            self.state().append_output(text);
        }
    }

    pub fn error(&self, message: impl AsRef<str>) {
        self.log(format!("[ERROR] {}", message.as_ref()));
    }

    pub fn debug(&self, category: &str, message: impl FnOnce() -> String) {
        if self.state().categories.contains(category) {
            self.log(message());
        }
    }

    pub fn start(
        &self,
        name: impl Into<String>,
        parent: Option<&PerformanceContext>,
        fields: Value,
    ) -> PerformanceSpan {
        let mut state = self.state();
        let context = state.session.clone().and_then(|session| {
            if parent.is_some_and(|parent| parent.session != session) {
                return None;
            }

            state.span_sequence += 1;
            Some(PerformanceContext {
                session,
                trace: parent.map_or(state.span_sequence, |parent| parent.trace),
                span: state.span_sequence,
            })
        });
        drop(state);
        let span = PerformanceSpan {
            diagnostics: self.clone(),
            context,
            parent: parent.map(|parent| parent.span),
            name: name.into(),
            fields,
            started_at: timestamp(),
            started: Instant::now(),
        };
        span.write("begin", None, Value::Null);
        span
    }

    fn state(&self) -> MutexGuard<'_, DiagnosticState> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl DiagnosticState {
    fn append(&mut self, message: &str) {
        self.append_output(&format!("[{}] {message}\n", timestamp()));
    }

    fn append_output(&mut self, text: &str) {
        self.lines.push(text.to_owned());
        self.subscribers.retain(|sender| match sender.try_send(()) {
            Ok(()) | Err(smol::channel::TrySendError::Full(())) => true,
            Err(smol::channel::TrySendError::Closed(())) => false,
        });
    }
}

impl PerformanceSpan {
    pub fn context(&self) -> Option<&PerformanceContext> {
        self.context.as_ref()
    }

    pub fn finish(mut self, outcome: &str, details: Value) {
        self.write("end", Some(outcome), details);
        self.context = None;
    }

    fn write(&self, event: &str, outcome: Option<&str>, details: Value) {
        let Some(context) = self.context.as_ref() else {
            return;
        };
        let mut state = self.diagnostics.state();
        if state.session.as_ref() != Some(&context.session) {
            return;
        }

        state.sequence += 1;
        let mut record = self.fields.as_object().cloned().unwrap_or_default();
        if let Some(details) = details.as_object() {
            record.extend(details.clone());
        }

        record.extend(
            json!({
                "product": "kk", "host": "zed", "platform": std::env::consts::OS,
                "arch": std::env::consts::ARCH, "session": context.session,
                "trace": context.trace, "span": context.span, "parent": self.parent,
                "sequence": state.sequence, "name": self.name, "event": event,
                "startedAt": self.started_at,
            })
            .as_object()
            .cloned()
            .unwrap_or_default(),
        );
        if let Some(outcome) = outcome {
            record.insert("outcome".into(), outcome.into());
            record.insert("endedAt".into(), timestamp().into());
            record.insert(
                "elapsedMs".into(),
                (self.started.elapsed().as_secs_f64() * 1000.0).into(),
            );
        }

        state.append(&format!("[PERF] {}", Value::Object(record)));
    }
}

impl Drop for PerformanceSpan {
    fn drop(&mut self) {
        self.write("end", Some("cancelled"), Value::Null);
    }
}

fn timestamp() -> String {
    chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::OffsetDateTime;

    fn records(diagnostics: &Diagnostics) -> Vec<Value> {
        diagnostics
            .read_since(0)
            .1
            .lines()
            .filter_map(|line| {
                line.split_once("[PERF] ")
                    .map(|(_, record)| serde_json::from_str(record).expect("performance record"))
            })
            .collect()
    }

    #[test]
    fn performance_sessions_isolate_disabled_cancelled_and_nested_work() {
        let diagnostics = Diagnostics::default();
        diagnostics.debug("copydiff", || panic!("disabled categories must be lazy"));
        drop(diagnostics.start("disabled", None, Value::Null));
        assert!(diagnostics.read_since(0).1.is_empty());
        diagnostics.configure(&["perf".into(), "copydiff".into()]);
        diagnostics.debug("copydiff", || "copy reference".into());
        let outer = diagnostics.start("refresh", None, json!({"root": "/test"}));
        let old_context = outer.context().cloned().expect("context");
        diagnostics
            .start("command", outer.context(), Value::Null)
            .finish("completed", json!({"count": 2}));
        drop(diagnostics.start("cancelled", outer.context(), Value::Null));
        outer.finish("completed", Value::Null);
        let before = records(&diagnostics);
        assert_eq!(before.len(), 6);
        assert_eq!(before[1]["parent"], before[0]["span"]);
        assert_eq!(before[1]["trace"], before[0]["trace"]);
        assert_eq!(before[2]["count"], 2);
        assert_eq!(before[4]["outcome"], "cancelled");
        assert!(
            before
                .iter()
                .enumerate()
                .all(|(index, record)| record["sequence"] == index + 1)
        );
        let obsolete = diagnostics.start("old-session", None, Value::Null);
        diagnostics.configure(&[]);
        diagnostics.configure(&["perf".into()]);
        let cursor = diagnostics.read_since(0).0;
        obsolete.finish("completed", Value::Null);
        drop(diagnostics.start("old-child", Some(&old_context), Value::Null));
        assert!(diagnostics.read_since(cursor).1.is_empty());
        diagnostics
            .start("new-session", None, Value::Null)
            .finish("error", Value::Null);
        let after = records(&diagnostics);
        assert_ne!(after.last().expect("last")["session"], before[0]["session"]);
        assert_eq!(after.last().expect("last")["outcome"], "error");
    }

    #[test]
    fn coalesced_notifications_preserve_all_lines_and_reader_positions() {
        let diagnostics = Diagnostics::default();
        let first = diagnostics.subscribe();
        diagnostics.log("first");
        diagnostics.error("second");
        assert!(first.try_recv().is_ok());
        assert!(first.try_recv().is_err());
        let (cursor, text) = diagnostics.read_since(0);
        assert_eq!(cursor, 2);
        assert!(text.contains("first\n") && text.contains("[ERROR] second\n"));
        let timestamp = text
            .split_once(']')
            .expect("timestamp")
            .0
            .trim_start_matches('[');
        assert!(
            OffsetDateTime::parse(timestamp, &time::format_description::well_known::Rfc3339)
                .is_ok()
        );
        let second = diagnostics.subscribe();
        drop(first);
        diagnostics.log("third");
        assert!(second.try_recv().is_ok());
        let (next, appended) = diagnostics.read_since(cursor);
        assert_eq!(next, 3);
        assert!(appended.contains("third\n") && !appended.contains("first\n"));
        assert!(diagnostics.read_since(next).1.is_empty());
    }
}
