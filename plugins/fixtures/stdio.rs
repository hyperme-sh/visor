//! Independent wire fixture. This program uses only the published frames and std.
use std::io::{self, BufRead};

fn main() {
    let mode = env!("VISOR_PLUGIN_FIXTURE");
    let mut lines = io::stdin().lock().lines();
    let init = lines.next().unwrap().unwrap();
    if mode == "mcp" {
        assert!(init.contains("\"protocolVersion\":\"2025-11-25\""));
        println!(
            r#"{{"jsonrpc":"2.0","id":1,"result":{{"protocolVersion":"2025-11-25","capabilities":{{"tools":{{}}}},"serverInfo":{{"name":"reference","version":"1"}}}}}}"#
        );
        assert!(lines
            .next()
            .unwrap()
            .unwrap()
            .contains("notifications/initialized"));
    } else {
        let (versions, version) = if mode == "v0" {
            ("[0]", 0)
        } else {
            ("[1,0]", 1)
        };
        assert!(init.contains(&format!("\"api_versions\":{versions}")));
        println!(
            r#"{{"jsonrpc":"2.0","id":1,"result":{{"plugin_id":"org.example.byte-count","api_version":{version}}}}}"#
        );
    }
    let call = lines.next().unwrap().unwrap();
    assert!(call.contains("\"name\":\"count-bytes\""));
    if mode == "v0" {
        assert!(!call.contains("timeout_ms"));
    } else if mode != "mcp" {
        assert!(call.contains("timeout_ms"));
    }
    match mode {
        "slow" => std::thread::sleep(std::time::Duration::from_secs(30)),
        "invalid-output" => {
            println!(r#"{{"jsonrpc":"2.0","id":2,"result":{{"byte_count":"wrong type"}}}}"#)
        }
        "mcp" => println!(
            r#"{{"jsonrpc":"2.0","id":2,"result":{{"content":[],"structuredContent":{{"byte_count":3}}}}}}"#
        ),
        "isolation" => {
            assert!(std::env::vars_os().all(|(name, value)| name == "PWD" && value == "/"));
            assert!(std::fs::File::open("/etc/passwd").is_err());
            assert!(std::fs::File::open("/proc/self/status").is_err());
            println!(r#"{{"jsonrpc":"2.0","id":2,"result":{{"byte_count":3}}}}"#);
        }
        "v0" => println!(r#"{{"jsonrpc":"2.0","id":2,"result":{{"byte_count":3}}}}"#),
        _ => panic!("unexpected fixture mode"),
    }
}
