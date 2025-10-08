use std::time::Duration;

use anyhow::Result;
use rdkafka::config::ClientConfig;
use rdkafka::message::{OwnedHeaders, ToBytes};
use rdkafka::producer::{FutureProducer, FutureRecord};

use crate::readers::kafka::KafkaMessage;
use crate::writers::Writer;
//
// pub struct KafkaMessage<K: ToBytes + Send, V: ToBytes + Send> {
//     pub key: K,
//     pub value: V,
//     pub headers: OwnedHeaders,
// }

pub struct KafkaWriter<K: ToBytes + Send, V: ToBytes + Send> {
    producer: FutureProducer,
    topic: String,
    _marker: std::marker::PhantomData<(K, V)>,
}
impl<K, V> KafkaWriter<K, V>
where
    K: ToBytes + Send,
    V: ToBytes + Send,
{
    pub fn new(brokers: &str, topic: &str) -> Self {
        let producer: FutureProducer = ClientConfig::new()
            .set("bootstrap.servers", brokers)
            .set("message.timeout.ms", "5000")
            .create()
            .unwrap();

        Self {
            producer,
            topic: topic.into(),
            _marker: std::marker::PhantomData,
        }
    }
}

impl<K, V> Writer for KafkaWriter<K, V>
where
    K: ToBytes + Send + Sync,
    V: ToBytes + Send + Sync,
{
    type Item = KafkaMessage<K, V>;
    async fn setup(&mut self) -> Result<()> {
        Ok(())
    }

    async fn write(&self, data: KafkaMessage<K, V>) -> Result<()> {
        // let futures = data.iter().map(|message| async move {
        //     // The send operation on the topic returns a future, which will be
        //     // completed once the result or failure from Kafka is received.
        //     log::info!("Sending status for message {} received", 1);
        //     let delivery_status = self
        //         .producer
        //         .send(
        //             FutureRecord::to(&self.topic)
        //                 .key(&message.key)
        //                 .payload(&message.value)
        //                 .headers(message.headers.clone()),
        //             Duration::from_secs(0),
        //         )
        //         .await;
        //
        //     // This will be executed when the result is received.
        //     log::info!("Delivery status for message {} received", 1);
        //     delivery_status
        // });
        //
        // // This loop will wait until all delivery statuses have been received.
        // for future in futures {
        //     log::info!("Future completed. Result: {:?}", future.await);
        // }

        log::info!("Sending status for message {} received", 1);
        let delivery_status = self
            .producer
            .send(
                FutureRecord::to(&self.topic)
                    .key(&data.key)
                    .payload(&data.value)
                    .headers(data.headers.clone()),
                Duration::from_secs(0),
            )
            .await;

        // This will be executed when the result is received.
        log::info!("Delivery status for message {} received", 1);

        Ok(())
    }
}
