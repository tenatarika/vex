pub struct PaymentService {
    gateway: Box<dyn PaymentGateway>,
}

pub trait PaymentGateway {
    fn charge(&self, amount: u64) -> Result<(), String>;
}

impl PaymentService {
    pub fn new(gateway: Box<dyn PaymentGateway>) -> Self {
        Self { gateway }
    }

    pub fn process_payment(&self, amount: u64) -> Result<(), String> {
        self.gateway.charge(amount)
    }
}

pub enum TransactionStatus {
    Pending,
    Completed,
    Failed,
}

pub type UserId = u64;

pub const MAX_RETRIES: u32 = 3;
