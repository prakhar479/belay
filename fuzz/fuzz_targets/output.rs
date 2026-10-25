//! Failure extraction, normalization, and shaping never panic, and a summary never
//! has more lines than the output (I5, T5).
#![no_main]

use belay_core::perception::{extract_failures, normalize_message};
use belay_core::shape::shape;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let text = String::from_utf8_lossy(data);
    let _ = belay_core::text::redact(&text);
    let _ = belay_core::text::strip_ansi(&text);
    for f in extract_failures(&text) {
        let _ = normalize_message(&f.message);
    }
    for level in 1..=3 {
        if let Some(s) = shape(&text, level, 1.0) {
            assert!(s.to_lines <= s.from_lines);
        }
    }
});
