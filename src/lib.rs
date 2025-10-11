pub mod operations;
pub mod readers;
pub mod schemas;
pub mod writers;

use operations::Operation;

pub struct Courier {
    operations: Vec<Box<dyn Operation>>,
}

impl Courier {
    pub fn new(operations: Vec<Box<dyn Operation>>) -> Self {
        Self { operations }
    }

    pub async fn run(self) {
        for op in self.operations {
            tokio::spawn(async move {
                op.run().await;
            });
        }

        loop {}
    }
}
