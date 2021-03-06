//! Future producer
//!
//! A high level producer that returns a Future for every produced message.
// TODO: extend docs

use client::{ClientContext, DefaultClientContext};
use config::{ClientConfig, FromClientConfig, FromClientConfigAndContext, RDKafkaLogLevel};
use producer::{DeliveryResult, ThreadedProducer, ProducerContext};
use statistics::Statistics;
use error::{KafkaError, KafkaResult, RDKafkaError};
use message::{Message, OwnedMessage, Timestamp, ToBytes};

use futures::{self, Canceled, Complete, Future, Poll, Oneshot, Async};

use std::sync::Arc;
use std::time::{Duration, Instant};

//
// ********** FUTURE PRODUCER **********
//

/// The `ProducerContext` used by the `FutureProducer`. This context will use a Future as its
/// `DeliveryOpaque` and will complete the future when the message is delivered (or failed to).
#[derive(Clone)]
struct FutureProducerContext<C: ClientContext + 'static> {
    wrapped_context: C,
}

/// Represents the result of message production as performed from the `FutureProducer`.
///
/// If message delivery was successful, `OwnedDeliveryResult` will return the partition and offset
/// of the message. If the message failed to be delivered an error will be returned, together with
/// an owned copy of the original message.
type OwnedDeliveryResult = Result<(i32, i64), (KafkaError, OwnedMessage)>;

// Delegates all the methods calls to the wrapped context.
impl<C: ClientContext + 'static> ClientContext for FutureProducerContext<C> {
    fn log(&self, level: RDKafkaLogLevel, fac: &str, log_message: &str) {
        self.wrapped_context.log(level, fac, log_message);
    }

    fn stats(&self, statistics: Statistics) {
        self.wrapped_context.stats(statistics);
    }

    fn error(&self, error: KafkaError, reason: &str) {
        self.wrapped_context.error(error, reason);
    }
}

impl<C: ClientContext + 'static> ProducerContext for FutureProducerContext<C> {
    type DeliveryOpaque = Box<Complete<OwnedDeliveryResult>>;

    fn delivery(&self, delivery_result: &DeliveryResult, tx: Box<Complete<OwnedDeliveryResult>>) {
        let owned_delivery_result = match *delivery_result {
            Ok(ref message) => Ok((message.partition(), message.offset())),
            Err((ref error, ref message)) => Err((error.clone(), message.detach())),
        };
        let _ = tx.send(owned_delivery_result); // TODO: handle error
    }
}


/// A producer that returns a `Future` for every message being produced.
///
/// Since message production in rdkafka is asynchronous, the called cannot immediately know if the
/// delivery of the message was successful or not. The `FutureProducer` provides this information in
/// a `Future`, that will be completed once the information becomes available. This producer has an
/// internal polling thread and as such it doesn't need to be polled. It can be cheaply cloned to
/// get a reference to the same underlying producer. The internal will be terminated once the
/// the `FutureProducer` goes out of scope.
#[must_use = "Producer polling thread will stop immediately if unused"]
pub struct FutureProducer<C: ClientContext + 'static = DefaultClientContext> {
    producer: Arc<ThreadedProducer<FutureProducerContext<C>>>,
}

impl<C: ClientContext + 'static> Clone for FutureProducer<C> {
    fn clone(&self) -> FutureProducer<C> {
        FutureProducer { producer: self.producer.clone() }
    }
}

impl FromClientConfig for FutureProducer {
    fn from_config(config: &ClientConfig) -> KafkaResult<FutureProducer> {
        FutureProducer::from_config_and_context(config, DefaultClientContext)
    }
}

impl<C: ClientContext + 'static> FromClientConfigAndContext<C> for FutureProducer<C> {
    fn from_config_and_context(config: &ClientConfig, context: C) -> KafkaResult<FutureProducer<C>> {
        let future_context = FutureProducerContext { wrapped_context: context };
        let threaded_producer = ThreadedProducer::from_config_and_context(config, future_context)?;
        Ok(FutureProducer { producer: Arc::new(threaded_producer) })
    }
}

/// A `Future` wrapping the result of the message production.
///
/// Once completed, the future will contain an `OwnedDeliveryResult` with information on the
/// delivery status of the message.
pub struct DeliveryFuture {
    rx: Oneshot<OwnedDeliveryResult>,
}

impl Future for DeliveryFuture {
    type Item = OwnedDeliveryResult;
    type Error = Canceled;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        match self.rx.poll() {
            Ok(Async::NotReady) => Ok(Async::NotReady),
            Ok(Async::Ready(owned_delivery_result)) => Ok(Async::Ready(owned_delivery_result)),
            Err(Canceled) => Err(Canceled),
        }
    }
}

impl<C: ClientContext + 'static> FutureProducer<C> {
    /// Sends a copy of the payload and key provided to the specified topic. When no partition is
    /// specified the underlying Kafka library picks a partition based on the key, or a random one
    /// if the key is not specified. Returns a `DeliveryFuture` that will eventually contain the
    /// result of the send. The `block_ms` parameter will control for how long the producer
    /// is allowed to block if the queue is full. Set it to -1 to block forever, or 0 to never block.
    /// If `block_ms` is reached and the queue is still full, a `RDKafkaError::QueueFull` will be
    /// reported in the `DeliveryFuture`.
    pub fn send_copy<P, K>(
        &self,
        topic: &str,
        partition: Option<i32>,
        payload: Option<&P>,
        key: Option<&K>,
        timestamp: Option<i64>,
        block_ms: i64,
    ) -> DeliveryFuture
    where K: ToBytes + ?Sized,
          P: ToBytes + ?Sized {
        let start_time = Instant::now();

        loop {
            let (tx, rx) = futures::oneshot();
            match self.producer.send_copy(topic, partition, payload, key, timestamp, Box::new(tx)) {
                Ok(_) => break DeliveryFuture{ rx },
                Err(e) => {
                    if let KafkaError::MessageProduction(RDKafkaError::QueueFull) = e {
                        if block_ms == -1 {
                            continue;
                        } else if block_ms > 0 && start_time.elapsed() < Duration::from_millis(block_ms as u64) {
                            self.poll(Duration::from_millis(100));
                            continue;
                        }
                    }
                    let (tx, rx) = futures::oneshot();
                    let owned_message = OwnedMessage::new(
                        payload.map(|p| p.to_bytes().to_vec()),
                        key.map(|k| k.to_bytes().to_vec()),
                        topic.to_owned(),
                        timestamp.map_or(Timestamp::NotAvailable, Timestamp::CreateTime),
                        partition.unwrap_or(-1),
                        0
                    );
                    let _ = tx.send(Err((e, owned_message)));
                    break DeliveryFuture { rx };
                }
            }
        }
    }

    /// Sends a copy of the payload and key provided to the specified topic. This works the same
    /// way as `send_copy`, the only difference is that it returns an error if enqueuing fails.
    pub fn send_copy_result<P, K>(
        &self,
        topic: &str,
        partition: Option<i32>,
        payload: Option<&P>,
        key: Option<&K>,
        timestamp: Option<i64>
    ) -> KafkaResult<DeliveryFuture>
    where K: ToBytes + ?Sized,
          P: ToBytes + ?Sized {
        let (tx, rx) = futures::oneshot();
        self.producer.send_copy(topic, partition, payload, key, timestamp, Box::new(tx))?;
        Ok(DeliveryFuture { rx })
    }

    /// Polls the internal producer. This is not normally required since the `ThreadedProducer` had
    /// a thread dedicated to calling `poll` regularly.
    pub fn poll<T: Into<Option<Duration>>>(&self, timeout: T) {
        self.producer.poll(timeout);
    }

    /// Flushes the producer. Should be called before termination.
    pub fn flush<T: Into<Option<Duration>>>(&self, timeout: T) {
        self.producer.flush(timeout);
    }

    /// Returns the number of messages waiting to be sent, or send but not acknowledged yet.
    pub fn in_flight_count(&self) -> i32 {
        self.producer.in_flight_count()
    }
}

#[cfg(test)]
mod tests {
    // Just test that there are no panics, and that each struct implements the expected
    // traits (Clone, Send, Sync etc.). Behavior is tested in the integrations tests.
    use super::*;
    use config::ClientConfig;

    struct TestContext;

    impl ClientContext for TestContext {}
    impl ProducerContext for TestContext {
        type DeliveryOpaque = Box<i32>;

        fn delivery(&self, _: &DeliveryResult, _: Self::DeliveryOpaque) {
            unimplemented!()
        }
    }

    // Verify that the future producer is clone, according to documentation.
    #[test]
    fn test_future_producer_clone() {
        let producer = ClientConfig::new().create::<FutureProducer>().unwrap();
        let _producer_clone = producer.clone();
    }

    // Test that the future producer can be cloned even if the context is not Clone.
    #[test]
    fn test_base_future_topic_send_sync() {
        let test_context = TestContext;
        let producer = ClientConfig::new()
            .create_with_context::<_, FutureProducer<TestContext>>(test_context)
            .unwrap();
        let _producer_clone = producer.clone();
    }
}
