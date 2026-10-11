use std::{
    collections::VecDeque, future::poll_fn, io, pin::Pin, sync::Mutex, task::Poll, time::Duration,
};

use serde_json::Value;
use tokio::{io::AsyncWrite, sync::Notify, time::Instant};
use tokio_util::sync::CancellationToken;

use crate::CLIENT_QUEUE_CAPACITY;

/// The ring never enters this queue: only this disposable delivery copy merges.
pub(crate) struct ClientQueue {
    delivery: Mutex<Delivery>,
    ready: Notify,
    pub closed: CancellationToken,
}

#[derive(Default)]
struct Delivery {
    items: VecDeque<Value>,
    in_flight: Option<Frame>,
}

struct Frame {
    session_id: Option<String>,
    bytes: Vec<u8>,
    written: usize,
}

impl ClientQueue {
    pub fn new() -> Self {
        Self {
            delivery: Mutex::new(Delivery::default()),
            ready: Notify::new(),
            closed: CancellationToken::new(),
        }
    }

    pub fn push(&self, value: Value) {
        let mut delivery = self.delivery.lock().expect("client queue poisoned");
        let items = &mut delivery.items;
        if self.closed.is_cancelled() {
            return;
        }
        if items.len() >= CLIENT_QUEUE_CAPACITY {
            let mut compacted = VecDeque::new();
            for item in items.drain(..).chain(std::iter::once(value)) {
                if let Some(last) = compacted.back_mut()
                    && merge(last, &item)
                {
                    continue;
                }
                compacted.push_back(item);
            }
            *items = compacted;
            if items.len() > CLIENT_QUEUE_CAPACITY {
                items.clear();
                self.closed.cancel();
                return;
            }
        } else {
            items.push_back(value);
        }
        drop(delivery);
        self.ready.notify_one();
    }

    pub fn discard_session(&self, session_id: &str) {
        let mut delivery = self.delivery.lock().expect("client queue poisoned");
        delivery
            .items
            .retain(|item| item["session_id"].as_str() != Some(session_id));
        if delivery
            .in_flight
            .as_ref()
            .is_some_and(|frame| frame.session_id.as_deref() == Some(session_id))
        {
            if delivery.in_flight.as_ref().unwrap().written > 0 {
                // A partial NDJSON frame cannot be reordered or withdrawn. Close
                // without completing it; reconnect/attach reconstructs the state.
                self.closed.cancel();
            }
            delivery.in_flight = None;
        }
    }

    /// Poll writes under the same lock as attach invalidation. Pending writes do
    /// not own a frame privately, so attach can discard a popped, unwritten note.
    /// Reset the deadline only on actual bytes written, never on queue activity.
    pub async fn write_to<W: AsyncWrite + Unpin>(
        &self,
        write: &mut W,
        timeout: Duration,
    ) -> io::Result<()> {
        let mut deadline = Instant::now() + timeout;
        loop {
            let notified = self.ready.notified();
            let has_frame = {
                let mut delivery = self.delivery.lock().expect("client queue poisoned");
                if delivery.in_flight.is_none()
                    && let Some(value) = delivery.items.pop_front()
                {
                    let bytes = elefante_rpc::encode_frame(&value).map_err(io::Error::other)?;
                    delivery.in_flight = Some(Frame {
                        session_id: value["session_id"].as_str().map(str::to_owned),
                        bytes,
                        written: 0,
                    });
                }
                delivery.in_flight.is_some()
            };
            if !has_frame {
                tokio::select! {
                    () = self.closed.cancelled() => return Ok(()),
                    () = notified => {},
                }
                deadline = Instant::now() + timeout;
                continue;
            }
            let progress = poll_fn(|cx| {
                let mut delivery = self.delivery.lock().expect("client queue poisoned");
                if self.closed.is_cancelled() {
                    return Poll::Ready(Ok(false));
                }
                let Some(frame) = &mut delivery.in_flight else {
                    return Poll::Ready(Ok(false));
                };
                match Pin::new(&mut *write).poll_write(cx, &frame.bytes[frame.written..]) {
                    Poll::Ready(Ok(0)) => Poll::Ready(Err(io::ErrorKind::WriteZero.into())),
                    Poll::Ready(Ok(count)) => {
                        frame.written += count;
                        if frame.written == frame.bytes.len() {
                            delivery.in_flight = None;
                        }
                        Poll::Ready(Ok(true))
                    }
                    Poll::Ready(Err(error)) => Poll::Ready(Err(error)),
                    Poll::Pending => Poll::Pending,
                }
            });
            tokio::select! {
                biased;
                () = self.closed.cancelled() => return Ok(()),
                result = progress => { if result? { deadline = Instant::now() + timeout; } },
                () = tokio::time::sleep_until(deadline) => return Err(io::ErrorKind::TimedOut.into()),
                () = notified => {},
            }
        }
    }

    #[cfg(test)]
    pub async fn pop(&self) -> Option<Value> {
        loop {
            let notified = self.ready.notified();
            if self.closed.is_cancelled() {
                return None;
            }
            if let Some(item) = self
                .delivery
                .lock()
                .expect("client queue poisoned")
                .items
                .pop_front()
            {
                return Some(item);
            }
            tokio::select! {
                () = self.closed.cancelled() => return None,
                () = notified => {},
            }
        }
    }
}

fn merge(left: &mut Value, right: &Value) -> bool {
    let (Ok(mut first), Ok(second)) = (
        serde_json::from_value::<elefante_rpc::Notification>(left.clone()),
        serde_json::from_value::<elefante_rpc::Notification>(right.clone()),
    ) else {
        return false;
    };
    if !first.coalesce(&second) {
        return false;
    }
    *left = serde_json::to_value(first).expect("notification serialization");
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn delta(seq: u64, method: &str) -> Value {
        json!({"session_id":"s1", "turn_id":"t1", "seq":seq, "method":method,"params":{"text":"x"}})
    }

    #[tokio::test]
    async fn pressure_coalesces_channels_and_preserves_ranges() {
        let queue = ClientQueue::new();
        for seq in 1..=128 {
            queue.push(delta(seq, "reasoning.delta"));
        }
        for seq in 129..=257 {
            queue.push(delta(seq, "text.delta"));
        }
        let reasoning = queue.pop().await.unwrap();
        let text = queue.pop().await.unwrap();
        assert_eq!(reasoning["seq_from"], 1);
        assert_eq!(reasoning["seq_to"], 128);
        assert_eq!(reasoning["params"]["text"], "x".repeat(128));
        assert_eq!(text["seq_from"], 129);
        assert_eq!(text["seq_to"], 257);
        assert_eq!(text["params"]["text"], "x".repeat(129));
        assert!(reasoning.get("seq").is_none());
    }

    #[tokio::test]
    async fn discrete_overflow_evicts_without_session_error() {
        let queue = ClientQueue::new();
        for seq in 1..=257 {
            queue.push(delta(seq, "tool.started"));
        }
        assert!(queue.closed.is_cancelled());
        assert!(queue.pop().await.is_none());
    }

    #[test]
    fn never_merge_across_sessions_turns_or_gaps() {
        let mut first = delta(1, "text.delta");
        assert!(!merge(&mut first, &delta(3, "text.delta")));
        let mut second = delta(2, "text.delta");
        second["session_id"] = "s2".into();
        assert!(!merge(&mut first, &second));
        second["session_id"] = "s1".into();
        second["turn_id"] = "t2".into();
        assert!(!merge(&mut first, &second));
    }

    #[tokio::test]
    async fn stopped_unix_reader_times_out_even_when_notifications_coalesce() {
        use std::sync::Arc;
        use tokio::io::AsyncReadExt as _;
        let (mut writer, mut stopped_reader) = tokio::net::UnixStream::pair().unwrap();
        let queue = Arc::new(ClientQueue::new());
        let mut large = delta(1, "text.delta");
        large["params"]["text"] = "x".repeat(4 * 1024 * 1024).into();
        queue.push(large);
        let writer_queue = queue.clone();
        let task = tokio::spawn(async move {
            let result = writer_queue
                .write_to(&mut writer, Duration::from_millis(100))
                .await;
            writer_queue.closed.cancel();
            result
        });
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if queue
                    .delivery
                    .lock()
                    .unwrap()
                    .in_flight
                    .as_ref()
                    .is_some_and(|frame| frame.written > 0)
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        // Keep adding mergeable deltas while the transport makes zero progress.
        // Queue activity must not reset the write deadline.
        for seq in 2..700 {
            queue.push(delta(seq, "text.delta"));
            assert!(!queue.closed.is_cancelled());
        }
        let error = tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(queue.closed.is_cancelled());
        let mut received = Vec::new();
        stopped_reader.read_to_end(&mut received).await.unwrap();
        assert_ne!(received.len(), 0);
        assert!(
            !received
                .windows(b"session.error".len())
                .any(|window| window == b"session.error")
        );
        assert!(
            !received.contains(&b'\n'),
            "partial frame must never be completed after eviction"
        );
    }
}
