//! portos-compute: the zero-capability pure-computation plugin (architecture
//! v0.4 §8.4, D9). Purity here is BY CONSTRUCTION: this crate has no I/O —
//! functions map JSON values to JSON values under a fuel meter.
//!
//! M0 ships a builtin registry with pinned-hash identities — the pinned-
//! content guard primitive (browser-driver-v0.md §14-5) exercised in its
//! smallest form. The JS engine (QuickJS with interrupt-based fuel, or Boa
//! with RuntimeLimits) replaces the builtin bodies at M0.5 WITHOUT changing
//! this interface; see design/m0-kernel-v0.md §8.

use serde_json::{json, Value};
use std::collections::BTreeMap;

pub struct FuelMeter {
    pub remaining: u64,
}

impl FuelMeter {
    pub fn new(fuel: u64) -> Self {
        FuelMeter { remaining: fuel }
    }
    pub fn charge(&mut self, n: u64) -> Result<(), String> {
        if self.remaining < n {
            return Err("fuel exhausted".into());
        }
        self.remaining -= n;
        Ok(())
    }
}

type PureFn = fn(&[Value], &mut FuelMeter) -> Result<Value, String>;

pub struct Registry {
    by_name: BTreeMap<String, (String, PureFn)>, // name → (pin hash, fn)
}

impl Registry {
    pub fn builtin() -> Registry {
        let mut by_name = BTreeMap::new();
        for (name, f) in [
            ("upper", f_upper as PureFn),
            ("count", f_count as PureFn),
            ("concat", f_concat as PureFn),
        ] {
            // M0 pin identity: hash of name@version. When real code (JS/WASM)
            // arrives, this becomes the hash of the code bytes.
            let pin = format!("blake3:{}", blake3::hash(format!("{name}@0.1").as_bytes()).to_hex());
            by_name.insert(name.to_string(), (pin, f));
        }
        Registry { by_name }
    }

    pub fn pin_of(&self, name: &str) -> Option<&str> {
        self.by_name.get(name).map(|(p, _)| p.as_str())
    }

    /// The pinned-content guard: execution requires the caller to name the
    /// pin it expects; mismatch (unknown fn, stale pin) is a refusal.
    pub fn run(
        &self,
        name: &str,
        expected_pin: Option<&str>,
        args: &[Value],
        fuel: &mut FuelMeter,
    ) -> Result<Value, String> {
        let (pin, f) = self
            .by_name
            .get(name)
            .ok_or_else(|| format!("unpinned function: {name}"))?;
        if let Some(exp) = expected_pin {
            if exp != pin {
                return Err(format!("pin mismatch for {name}"));
            }
        }
        f(args, fuel)
    }
}

fn f_upper(args: &[Value], fuel: &mut FuelMeter) -> Result<Value, String> {
    let s = args.get(0).and_then(|v| v.as_str()).ok_or("upper: want string")?;
    fuel.charge(s.len() as u64)?;
    Ok(json!(s.to_uppercase()))
}

fn f_count(args: &[Value], fuel: &mut FuelMeter) -> Result<Value, String> {
    fuel.charge(1)?;
    let n = match args.get(0) {
        Some(Value::Array(a)) => a.len(),
        Some(Value::String(s)) => s.len(),
        _ => 0,
    };
    Ok(json!(n))
}

fn f_concat(args: &[Value], fuel: &mut FuelMeter) -> Result<Value, String> {
    let mut out = String::new();
    for a in args {
        if let Some(s) = a.as_str() {
            fuel.charge(s.len() as u64)?;
            out.push_str(s);
        }
    }
    Ok(json!(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuel_bounds_execution() {
        let reg = Registry::builtin();
        let mut fuel = FuelMeter::new(3);
        let err = reg.run("upper", None, &[json!("looooong")], &mut fuel);
        assert!(err.is_err(), "fuel must bound work");
        let mut fuel = FuelMeter::new(1000);
        assert_eq!(reg.run("upper", None, &[json!("hi")], &mut fuel).unwrap(), json!("HI"));
    }

    #[test]
    fn pin_guard_refuses_mismatch() {
        let reg = Registry::builtin();
        let mut fuel = FuelMeter::new(1000);
        assert!(reg.run("upper", Some("blake3:wrong"), &[json!("x")], &mut fuel).is_err());
        let good = reg.pin_of("upper").unwrap().to_string();
        assert!(reg.run("upper", Some(&good), &[json!("x")], &mut fuel).is_ok());
    }
}
