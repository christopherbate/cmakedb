//! §6.5 trace-ingestion invariant under libFuzzer: malformed JSONL yields
//! a clean Err, never a panic.
#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = cmakedb_db::ingest::scan_trace_bytes(data);
});
