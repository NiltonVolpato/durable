//! Runtime side of the serde glue that durable-driver generates.

use serde::Deserialize;
pub use serde_json::{Map, Value};
use std::future::Future;
use std::pin::Pin;

pub type Error = String;
pub type Result<T> = std::result::Result<T, Error>;
pub type SerFn = unsafe fn(*const u8) -> Result<Value>;
pub type DeFn = unsafe fn(&Value, *mut u8) -> Result<()>;

/// Generated glue for one workflow root.
pub struct Entry {
    /// `std::any::type_name` of the root future, as computed by rustc.
    pub type_name: &'static str,
    pub size: usize,
    pub align: usize,
    pub ser: SerFn,
    pub de: DeFn,
}

/// The glue entry for root future type `F`.
pub fn find<F>(table: &'static [Entry]) -> Result<&'static Entry> {
    let name = std::any::type_name::<F>();
    let mut it = table.iter().filter(|e| e.type_name == name);
    let e = it.next().ok_or_else(|| {
        format!("no glue for workflow {name}: mark it #[durable::workflow] and build with `cargo durable build`")
    })?;
    if it.next().is_some() {
        return Err(format!("ambiguous glue for {name}"));
    }
    if e.size != size_of::<F>() || e.align != align_of::<F>() {
        return Err(format!("glue for {name} is from another build; rebuild with `cargo durable build`"));
    }
    Ok(e)
}

/// Serialize a parked root future. Call only between polls.
pub fn serialize<F: Future>(e: &Entry, fut: &Pin<Box<F>>) -> Result<Value> {
    unsafe { (e.ser)(&**fut as *const F as *const u8) }
}

/// Rebuild a root future from `serialize`'s output.
pub fn deserialize<F: Future>(e: &Entry, v: &Value) -> Result<Pin<Box<F>>> {
    let mut b = Box::<F>::new_uninit();
    // On error the partly written frame is not dropped (values already
    // written leak); on success every live value and the state were written.
    unsafe {
        (e.de)(v, b.as_mut_ptr() as *mut u8)?;
        Ok(Box::into_pin(b.assume_init()))
    }
}

// ---------- helpers called by generated code ----------

/// Serialize the `T` at `p`.
pub unsafe fn ser_field<T: serde::Serialize>(p: *const u8) -> Result<Value> {
    serde_json::to_value(unsafe { &*(p as *const T) }).map_err(|e| e.to_string())
}

/// Deserialize field `name` of `v` and write it to `p`.
pub unsafe fn de_field<T: serde::de::DeserializeOwned>(v: &Value, name: &str, p: *mut u8) -> Result<()> {
    let t = T::deserialize(field(v, name)?).map_err(|e| format!("field {name}: {e}"))?;
    unsafe { (p as *mut T).write(t) };
    Ok(())
}

/// `[T; N]` as a sequence (serde's own impls stop at N = 32).
pub unsafe fn ser_array<T: serde::Serialize, const N: usize>(p: *const u8) -> Result<Value> {
    serde_json::to_value(unsafe { &*(p as *const [T; N]) }.as_slice()).map_err(|e| e.to_string())
}

pub unsafe fn de_array<T: serde::de::DeserializeOwned, const N: usize>(v: &Value, name: &str, p: *mut u8) -> Result<()> {
    let items = Vec::<T>::deserialize(field(v, name)?).map_err(|e| format!("field {name}: {e}"))?;
    let arr: [T; N] = items.try_into().map_err(|_| format!("field {name}: expected {N} items"))?;
    unsafe { (p as *mut [T; N]).write(arr) };
    Ok(())
}

pub fn field<'a>(v: &'a Value, name: &str) -> Result<&'a Value> {
    v.get(name).ok_or_else(|| format!("missing field {name}"))
}

pub fn state(v: &Value) -> Result<u64> {
    field(v, "state")?.as_u64().ok_or_else(|| "state is not a number".into())
}

/// A state loads only if its live locals and types are unchanged.
pub fn check_sig(v: &Value, expected: &str) -> Result<()> {
    let got = field(v, "sig")?.as_str().unwrap_or_default();
    if got != expected {
        return Err(format!(
            "state {} at {} changed in this build:\n  snapshot: {got}\n  this build: {expected}",
            v["state"], v["at"]
        ));
    }
    Ok(())
}
