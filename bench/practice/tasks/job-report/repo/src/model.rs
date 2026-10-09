#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Success,
    Failed,
    Cancelled,
}

#[derive(Clone, Debug)]
pub struct Job {
    pub status: Status,
    pub elapsed_ms: u64,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Summary {
    pub success: usize,
    pub failed: usize,
    pub cancelled: usize,
    pub elapsed_ms: u64,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
}
