use tokio::sync::mpsc;

pub struct AsyncRingBuffer {
    tx: mpsc::Sender<Vec<u8>>,
    rx: mpsc::Receiver<Vec<u8>>,
    capacity: usize,
    closed: bool,
}

impl AsyncRingBuffer {
    pub fn new(max_chunks: usize) -> Self {
        let (tx, rx) = mpsc::channel(max_chunks);
        Self {
            tx,
            rx,
            capacity: max_chunks,
            closed: false,
        }
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn is_closed(&self) -> bool {
        self.closed
    }

    pub async fn put(&self, chunk: Vec<u8>) -> bool {
        if self.closed {
            return false;
        }
        self.tx.send(chunk).await.is_ok()
    }

    pub async fn get(&mut self) -> Option<Vec<u8>> {
        self.rx.recv().await
    }

    pub fn close(&mut self) {
        self.closed = true;
    }

    pub fn reset(&mut self) {
        self.closed = false;
        let (tx, rx) = mpsc::channel(self.capacity);
        self.tx = tx;
        self.rx = rx;
    }

    pub fn sender(&self) -> mpsc::Sender<Vec<u8>> {
        self.tx.clone()
    }
}
