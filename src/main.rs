use async_stream::stream;
use std::time::Duration;
use tokio_stream::{Stream, StreamExt};

// Esta função retorna uma implementação de Stream
fn count_down_stream() -> impl Stream<Item = u32> {
    stream! {
        for i in (1..=5).rev() {
            println!("Gerando valor {}...", i);
            tokio::time::sleep(Duration::from_millis(300)).await;
            yield i;
        }
    }
}

#[tokio::main]
async fn main() {
    // Criamos a stream chamando a função
    let my_stream = count_down_stream();
    tokio::pin!(my_stream);
    // Consumimos os valores
    while let Some(value) = my_stream.next().await {
        println!("Valor da stream: {}", value);
    }
    println!("Contagem regressiva finalizada!");
}
