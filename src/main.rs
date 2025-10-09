use std::time::Duration;

use tokio::time;

async fn task_that_takes_a_second() {
    let time = time::Instant::now();
    println!("{:?}", time);
    println!("hello");
    // time::sleep(time::Duration::from_secs(3)).await
}

#[tokio::main]
async fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let mut interval = time::interval(time::Duration::from_secs(2));
    for _i in 0..5 {
        let start = std::time::Instant::now();

        task_that_takes_a_second().await;

        let elapsed = start.elapsed();
        let dur = Duration::from_secs(2);
        if elapsed > dur {
            log::warn!(
                "Loop iteration took {:?}, which exceeds the configured interval of {:?}",
                elapsed,
                dur
            );
        }
        interval.tick().await;
    }
}

