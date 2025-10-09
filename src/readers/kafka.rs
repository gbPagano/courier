use std::any::type_name;
use std::time::Duration;

use async_stream::stream;
use rdkafka::Message;
use rdkafka::config::ClientConfig;
use rdkafka::consumer::{Consumer, StreamConsumer};
use tokio::time::sleep;
use tokio_stream::Stream;

use crate::readers::StreamReader;
use crate::schemas::Json;
use crate::schemas::kafka::KafkaMessage;

pub struct KafkaReader<T> {
    consumer: StreamConsumer,
    _marker: std::marker::PhantomData<T>,
}

impl<T: Json> KafkaReader<T> {
    pub fn new(brokers: &str, group_id: &str, topics: Vec<&str>) -> Self {
        let consumer: StreamConsumer = ClientConfig::new()
            .set("group.id", group_id)
            .set("bootstrap.servers", brokers)
            .set("enable.partition.eof", "false")
            .set("session.timeout.ms", "6000")
            .set("enable.auto.commit", "false")
            .create()
            .expect("Kafka Consumer creation failed");

        consumer
            .subscribe(&topics)
            .expect("Can't subscribe to specified topics");

        Self {
            consumer,
            _marker: std::marker::PhantomData,
        }
    }
}

impl<T: Json> StreamReader for KafkaReader<T> {
    type Item = KafkaMessage<T>;

    async fn stream(&self) -> impl Stream<Item = Self::Item> {
        stream! {
            loop {
                log::debug!("Waiting for next message");
                match self.consumer.recv().await {
                    Ok(m) => {
                        let m = m.detach();
                        let offset = m.offset();
                        let partition = m.partition();
                        let topic = m.topic();

                        log::debug!(
                            "Received message from topic: {} (partition {}, offset {})",
                            topic, partition, offset
                        );

                        // Parse key
                        let key = match m.key() {
                            Some(k) => match std::str::from_utf8(k) {
                                Ok(key_str) => key_str.trim(),
                                Err(e) => {
                                    log::error!(
                                        "Failed to parse message key as UTF-8 at offset {}: {}",
                                        offset, e
                                    );
                                    continue;
                                }
                            },
                            None => {
                                let key_str = type_name::<Self>();
                                log::warn!(
                                    "Message at offset {} has no key, using default key: {}",
                                    offset,
                                    key_str
                                );
                                key_str
                            }
                        };

                        // Parse payload
                        let value_str = match m.payload() {
                            Some(p) => match std::str::from_utf8(p) {
                                Ok(v) => v,
                                Err(e) => {
                                    log::error!(
                                        "Failed to parse message payload as UTF-8 at offset {}: {}",
                                        offset, e
                                    );
                                    continue;
                                }
                            },
                            None => {
                                log::error!("Message at offset {} has no payload", offset);
                                continue;
                            }
                        };

                        // Deserialize JSON
                        let value: T = match serde_json::from_str(value_str) {
                            Ok(v) => v,
                            Err(_) => {
                                log::error!(
                                    "Failed to deserialize JSON at offset {}. Payload: {}",
                                    offset, value_str
                                );
                                continue;
                            }
                        };

                        let message = KafkaMessage::new(&key, value);
                        log::debug!(
                            "Processed message from topic: {} (partition: {}, offset: {})",
                            topic, partition, offset
                        );

                        yield message;
                    }
                    Err(e) => {
                        log::error!("Error receiving message from Kafka: {:?}", e);
                        sleep(Duration::from_millis(100)).await;
                    }
                }
            }
        }
    }
}
