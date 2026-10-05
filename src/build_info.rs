//! Identity of the binary, not merely its package release version.

use serde_json::{Value, json};

pub const VERSION: &str = env!("SCIPI_BUILD_VERSION");

pub fn info() -> Value {
    let commit = env!("SCIPI_BUILD_COMMIT");
    json!({
        "id": env!("SCIPI_BUILD_ID"),
        "commit": if commit.is_empty() { None } else { Some(commit) },
        "dirty": env!("SCIPI_BUILD_DIRTY") == "true",
        "built_at": env!("SCIPI_BUILD_TIMESTAMP").parse::<u64>().expect("build timestamp"),
        "version": env!("CARGO_PKG_VERSION"),
    })
}
