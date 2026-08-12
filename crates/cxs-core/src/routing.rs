//! Version-qualified App Server routing contract.
//!
//! Codex's generated schema proves that these methods exist. This manifest adds
//! the Shuttle-specific ownership decision that the schema cannot express.

pub const HOST_REQUEST_METHODS: &[&str] = &[
    "command/exec",
    "command/exec/resize",
    "command/exec/terminate",
    "command/exec/write",
    "fs/copy",
    "fs/createDirectory",
    "fs/getMetadata",
    "fs/readDirectory",
    "fs/readFile",
    "fs/remove",
    "fs/unwatch",
    "fs/watch",
    "fs/writeFile",
    "fuzzyFileSearch",
    "fuzzyFileSearch/sessionStart",
    "fuzzyFileSearch/sessionStop",
    "fuzzyFileSearch/sessionUpdate",
    "process/kill",
    "process/resizePty",
    "process/spawn",
    "process/writeStdin",
];

pub const HOST_NOTIFICATION_METHODS: &[&str] = &[
    "command/exec/outputDelta",
    "fs/changed",
    "fuzzyFileSearch/sessionCompleted",
    "fuzzyFileSearch/sessionUpdated",
    "process/exited",
    "process/outputDelta",
];

pub const REQUIRED_INITIALIZE_FIELDS: &[&str] =
    &["userAgent", "codexHome", "platformFamily", "platformOs"];

#[must_use]
pub fn is_host_request(method: &str) -> bool {
    HOST_REQUEST_METHODS.contains(&method)
}

#[must_use]
pub fn is_host_notification(method: &str) -> bool {
    HOST_NOTIFICATION_METHODS.contains(&method)
}
