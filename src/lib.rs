pub mod client;
pub mod server;
pub use std::sync::{Arc, Mutex};
pub use std::collections::VecDeque;
pub type FileQueue = Arc<Mutex<VecDeque<String>>>; 