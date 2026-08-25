//! portos-echo: the toy driver. Exists to exercise kernel mechanisms end to
//! end — verb calls over IPC, fd-passing reads, and the two-layer naming
//! rule (ephemeral refs live here, NOT in the kernel handle table;
//! browser-driver-v0.md §14-7).

use serde_json::{json, Value};
use std::collections::HashSet;
use std::io::Read;

fn main() -> std::io::Result<()> {
    let mut ephemeral: HashSet<String> = HashSet::new();
    let mut next_ref = 0u32;

    portos_sdk::serve("portos-echo", move |verb, args, ctx| match verb {
        // toy effect: visible side channel for tests (stdout)
        "echo.emit" => {
            let s = args.get(0).and_then(|v| v.as_str()).unwrap_or("");
            println!("emit: {s}");
            Ok(Value::Null)
        }
        // observation over the data plane: payload arrives as an fd,
        // never inside the JSON frame. Returns a bounded digest (this is
        // the control-plane preview discipline, not a payload copy).
        "echo.digest" => {
            let fd = ctx
                .fds
                .into_iter()
                .next()
                .ok_or_else(|| "echo.digest needs an fd".to_string())?;
            let mut f = std::fs::File::from(fd);
            let mut n: u64 = 0;
            let mut first = [0u8; 32];
            let mut firstn = 0usize;
            let mut buf = vec![0u8; 1 << 20];
            loop {
                let k = f.read(&mut buf).map_err(|e| e.to_string())?;
                if k == 0 {
                    break;
                }
                if firstn == 0 {
                    firstn = k.min(32);
                    first[..firstn].copy_from_slice(&buf[..firstn]);
                }
                n += k as u64;
            }
            Ok(json!({
                "bytes": n,
                "head_hex": hex_of(&first[..firstn]),
            }))
        }
        // two-layer naming demo: refs are driver-session-local, volatile,
        // and never enter the kernel handle table.
        "echo.make_ref" => {
            next_ref += 1;
            let r = format!("e{next_ref}");
            ephemeral.insert(r.clone());
            Ok(json!({ "ref": r }))
        }
        "echo.use_ref" => {
            let r = args.get(0).and_then(|v| v.as_str()).unwrap_or("");
            if ephemeral.contains(r) {
                Ok(json!({ "used": r }))
            } else {
                Err(format!("stale ephemeral ref: {r}"))
            }
        }
        other => Err(format!("unknown verb: {other}")),
    })
}

fn hex_of(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}
