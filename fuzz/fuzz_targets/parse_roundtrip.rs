//! §6.5 parser invariant under libFuzzer: never panic; whatever parses
//! reprints byte-identically (error-tolerant parses included).
#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(s) = std::str::from_utf8(data) {
        if let Ok(parsed) =
            cmakedb_syntax::parse_source(std::path::PathBuf::from("fuzz.cmake"), s.to_string())
        {
            let printed = cmakedb_syntax::reprint(&parsed).expect("reprint must not fail");
            assert_eq!(printed, s, "lossless reprint violated");
        }
    }
});
