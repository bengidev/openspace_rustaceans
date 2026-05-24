use std::future::Future;

use anyhow::Error;
use tokio::task::JoinHandle;
use tracing::{error, Instrument, Span};

/// Spawns a future instrumented with the current tracing span.
///
/// Use this helper at async task boundaries so events emitted inside the spawned
/// task keep the caller's span context attached to structured logs.
pub fn spawn_in_current_span<F>(future: F) -> JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    tokio::spawn(future.instrument(Span::current()))
}

/// Instruments a future with the current tracing span without spawning it.
///
/// This is useful for executors or task runners that own spawning but still need
/// the current span context propagated consistently.
pub fn in_current_span<F>(future: F) -> tracing::instrument::Instrumented<F>
where
    F: Future,
{
    future.instrument(Span::current())
}

/// Formats an anyhow error as a context chain from outer context to root cause.
#[must_use]
pub fn anyhow_chain(error: &Error) -> String {
    error
        .chain()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(": ")
}

/// Logs an anyhow error with both display text and the full context chain.
pub fn log_anyhow_error(error: &Error) {
    error!(error = %error, error_chain = %anyhow_chain(error));
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::{anyhow, Context};
    use std::{
        io,
        sync::{Arc, Mutex},
    };
    use tracing_subscriber::{fmt::MakeWriter, layer::SubscriberExt};

    #[derive(Clone, Default)]
    struct Buffer(Arc<Mutex<Vec<u8>>>);

    impl Buffer {
        fn output(&self) -> String {
            String::from_utf8(self.0.lock().expect("buffer mutex poisoned").clone())
                .expect("utf8 log output")
        }
    }

    struct BufferWriter(Arc<Mutex<Vec<u8>>>);

    impl io::Write for BufferWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0
                .lock()
                .expect("buffer mutex poisoned")
                .extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for Buffer {
        type Writer = BufferWriter;

        fn make_writer(&'a self) -> Self::Writer {
            BufferWriter(self.0.clone())
        }
    }

    fn subscriber(buffer: Buffer) -> impl tracing::Subscriber + Send + Sync {
        tracing_subscriber::registry().with(
            tracing_subscriber::fmt::layer()
                .json()
                .with_current_span(true)
                .with_span_list(true)
                .with_writer(buffer),
        )
    }

    #[tokio::test]
    async fn spawned_task_keeps_current_span_context() {
        let buffer = Buffer::default();
        let subscriber = subscriber(buffer.clone());

        let dispatch = tracing::Dispatch::new(subscriber);
        let _guard = tracing::dispatcher::set_default(&dispatch);
        let span = tracing::info_span!("request", request_id = "req-42");
        let task = {
            let _entered = span.enter();
            spawn_in_current_span(async {
                tracing::info!(event = "child-work");
            })
        };
        task.await.expect("spawned task completes");

        let output = buffer.output();
        assert!(output.contains("child-work"));
        assert!(output.contains("request"));
        assert!(output.contains("req-42"));
    }

    #[test]
    fn anyhow_chain_preserves_context_and_root_cause() {
        let error = Err::<(), _>(anyhow!("disk full"))
            .context("write cache")
            .context("persist session")
            .expect_err("error expected");

        let chain = anyhow_chain(&error);

        assert!(chain.contains("persist session"));
        assert!(chain.contains("write cache"));
        assert!(chain.contains("disk full"));
    }

    #[test]
    fn logs_anyhow_context_chain_as_structured_field() {
        let buffer = Buffer::default();
        let subscriber = subscriber(buffer.clone());
        let error = Err::<(), _>(anyhow!("socket closed"))
            .context("stream response")
            .context("call provider")
            .expect_err("error expected");

        tracing::subscriber::with_default(subscriber, || log_anyhow_error(&error));

        let output = buffer.output();
        assert!(output.contains("error_chain"));
        assert!(output.contains("call provider"));
        assert!(output.contains("stream response"));
        assert!(output.contains("socket closed"));
    }
}
