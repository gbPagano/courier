use futures::future;
use operations::Operation;

pub mod operations;
pub mod readers;
pub mod schemas;
pub mod writers;

pub struct Courier {
    operations: Vec<Box<dyn Operation>>,
}

impl Courier {
    pub fn new(operations: Vec<Box<dyn Operation>>) -> Self {
        Self { operations }
    }

    pub async fn run(self) {
        let mut handles = Vec::new();

        for op in self.operations {
            let handle = tokio::spawn(async move {
                op.run().await;
            });
            handles.push(handle);
        }

        future::join_all(handles).await;
    }
}

#[macro_export]
macro_rules! courier {
    ($($op:expr),* $(,)?) => {
        Courier::new(vec![$(Box::new($op)),*])
    };
}
