//! Raw bindings to llama.cpp b11461 (llama.h, mtmd.h, mtmd-helper.h). Use `digup-embed` for the safe API.
#![allow(non_camel_case_types, non_snake_case, non_upper_case_globals, dead_code, clippy::all)]
include!(concat!(env!("OUT_DIR"), "/bindings.rs"));
