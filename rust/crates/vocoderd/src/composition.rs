//! Shared composition-replay runner (conformance/composition-replay).
//! One trace row drives a machine and asserts its rpc.result; `$NAME`
//! placeholders substitute captured values from earlier rows' outputs.

use vocoder_cordis::{EventName, MachineIn, MachineOut, PluginMachine};

pub fn replay_trace(
    machine: &mut dyn PluginMachine<In = MachineIn, Out = MachineOut>,
    trace_text: &str,
    vars: &mut std::collections::HashMap<String, String>,
) -> usize {
    let mut steps = 0usize;
    for (line_no, line) in trace_text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let row: serde_json::Value = serde_json::from_str(line).unwrap();
        if row["kind"].as_str() != Some("in") {
            continue;
        }
        let mut payload = row["payload"].to_string();
        for (k, v) in vars.iter() {
            payload = payload.replace(&format!("${k}"), v);
        }
        let payload: serde_json::Value = serde_json::from_str(&payload).unwrap();
        let outs = machine.handle(MachineIn::Event {
            name: EventName::new(row["event"].as_str().unwrap()),
            payload,
        });
        let result = outs.iter().find_map(|o| {
            if let MachineOut::Realize(vocoder_cordis::RealizeRequest::Raw(v)) = o
                && v.get("kind").and_then(|k| k.as_str()) == Some("rpc.result")
            {
                return Some(v["result"].clone());
            }
            None
        });
        let expect = &row["expect"];
        match expect["result"].as_str() {
            Some("ok") => {
                let r = result.clone().expect("expected an rpc.result row");
                assert!(
                    r["ok"].as_bool().unwrap_or(false),
                    "line {}: expected ok, got {r}",
                    line_no + 1
                );
                if let Some(capture) = expect["capture"].as_object() {
                    for (var, path) in capture {
                        let mut cur = &r["value"];
                        for seg in path.as_str().unwrap().split('.') {
                            cur = &cur[seg];
                        }
                        vars.insert(
                            var.clone(),
                            cur.as_str().expect("captured value must be a string").to_string(),
                        );
                    }
                }
            }
            Some("err") => {
                let r = result.expect("expected an rpc.result row");
                assert_eq!(r["ok"].as_bool(), Some(false), "line {}", line_no + 1);
                if let Some(code) = expect["code"].as_str() {
                    assert_eq!(
                        r["error"]["code"].as_str(),
                        Some(code),
                        "line {}",
                        line_no + 1
                    );
                }
            }
            other => panic!("line {}: unknown expect {other:?}", line_no + 1),
        }
        steps += 1;
    }
    steps
}
