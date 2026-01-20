//! Information about features the crates were compiled with.

use std::sync::OnceLock;

pub static ENABLED_FEATURES: OnceLock<&'static [&'static str]> = OnceLock::new();
pub static AVAILABLE_FEATURES: OnceLock<&'static [&'static str]> = OnceLock::new();
