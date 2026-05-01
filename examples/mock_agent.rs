//! Mock agent — a small program that speaks the expected app-server protocol
//! for development and testing. Emits JSON-RPC events over stdout.
//!
//! Usage:
//!   cargo run --example mock_agent

use std::io::{self, Write};

fn main() {
    let events = vec![
        serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notification",
            "params": { "message": "Starting work on issue", "type": "notification" }
        }),
        serde_json::json!({
            "jsonrpc": "2.0",
            "result": {
                "type": "v2/TurnCompleted",
                "message": "Turn 1 done: analyzed the codebase",
                "usage": { "inputTokens": 120, "outputTokens": 250 }
            },
            "id": 1
        }),
        serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notification",
            "params": { "message": "Turn 2: implementing fix", "type": "notification" }
        }),
        serde_json::json!({
            "jsonrpc": "2.0",
            "result": {
                "type": "v2/TurnCompleted",
                "message": "Turn 2 done: wrote implementation and tests",
                "usage": { "inputTokens": 180, "outputTokens": 420 }
            },
            "id": 2
        }),
        serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notification",
            "params": { "message": "Turn 3: creating PR", "type": "notification" }
        }),
        serde_json::json!({
            "jsonrpc": "2.0",
            "result": {
                "type": "v2/TurnCompleted",
                "message": "Turn 3 done: PR created at https://github.com/org/repo/pull/42",
                "usage": { "inputTokens": 200, "outputTokens": 500 }
            },
            "id": 3
        }),
        // Exit signal
        serde_json::json!({
            "jsonrpc": "2.0",
            "method": "exit",
            "params": {}
        }),
    ];

    let stdout = io::stdout();
    let mut handle = stdout.lock();

    for event in &events {
        let line = serde_json::to_string(event).unwrap();
        writeln!(handle, "{}", line).unwrap();
        handle.flush().unwrap();
        // Simulate work delay
        std::thread::sleep(std::time::Duration::from_millis(200));
    }

    eprintln!("mock_agent: session complete");
}
