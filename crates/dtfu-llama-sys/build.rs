//! Links a llama.cpp b11461 build (patched with patches/llama.cpp-no-logits.patch) and generates raw bindings.
//!
//! The build dir comes from `DTFU_LLAMA_BUILD` (default: <workspace>/build-llama). Create it with
//! `scripts/build-llama.sh`, which mirrors DigUp's own `scripts/build-llama.sh` but picks backends per platform.
use std::{env, path::{Path, PathBuf}};

fn main() {
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let ws = manifest.join("../..").canonicalize().unwrap();
    let build = env::var("DTFU_LLAMA_BUILD").map(PathBuf::from).unwrap_or_else(|_| ws.join("build-llama"));
    let src = env::var("DTFU_LLAMA_SRC").map(PathBuf::from).unwrap_or_else(|_| ws.join("vendor/llama.cpp"));
    println!("cargo:rerun-if-env-changed=DTFU_LLAMA_BUILD");
    println!("cargo:rerun-if-env-changed=DTFU_LLAMA_SRC");
    println!("cargo:rerun-if-changed=wrapper.h");

    // Backend libraries exist only if that backend was enabled in the cmake build.
    let mut dirs: Vec<PathBuf> = vec![build.join("src"), build.join("tools/mtmd"), build.join("ggml/src"), build.join("vendor/hash")];
    for b in ["ggml-cuda", "ggml-vulkan", "ggml-metal", "ggml-blas", "ggml-hip"] {
        dirs.push(build.join("ggml/src").join(b));
    }
    // MSVC / Xcode generators put libraries under a per-config subfolder (UNTESTED on Windows; issue #1).
    let extra: Vec<PathBuf> = dirs.iter().map(|d| d.join("Release")).collect();
    dirs.extend(extra);
    for d in &dirs {
        if d.is_dir() {
            println!("cargo:rustc-link-search=native={}", d.display());
        }
    }
    let has = |d: &Path, name: &str| d.join(format!("lib{name}.a")).exists() || d.join(format!("{name}.lib")).exists();
    let all = |name: &str| dirs.iter().any(|d| has(d, name));
    // Order matters for static linking: dependents first.
    for lib in ["mtmd", "llama", "ggml", "ggml-cpu", "ggml-blas", "ggml-metal", "ggml-cuda", "ggml-vulkan", "ggml-hip", "ggml-base", "vendor-hash"] {
        if all(lib) {
            println!("cargo:rustc-link-lib=static={lib}");
        }
    }
    let target = env::var("TARGET").unwrap();
    if target.contains("apple") {
        println!("cargo:rustc-link-lib=c++");
        for f in ["Accelerate", "Metal", "Foundation", "MetalKit"] {
            println!("cargo:rustc-link-lib=framework={f}");
        }
        // ggml-metal uses `@available`, which needs clang's `___isPlatformVersionAtLeast`;
        // rustc does not link clang's runtime, so link it explicitly.
        if let Ok(out) = std::process::Command::new("clang").arg("--print-file-name=libclang_rt.osx.a").output() {
            let p = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
            if let (true, Some(dir)) = (p.is_absolute() && p.exists(), p.parent()) {
                println!("cargo:rustc-link-search=native={}", dir.display());
                println!("cargo:rustc-link-lib=static=clang_rt.osx");
            }
        }
    } else if target.contains("windows") {
        // MSVC links the C++ runtime itself; ggml-cpu reads the registry for CPU info.
        println!("cargo:rustc-link-lib=advapi32");
    } else {
        println!("cargo:rustc-link-lib=stdc++");
        println!("cargo:rustc-link-lib=gomp"); // GGML_OPENMP=ON
        println!("cargo:rustc-link-lib=m");
        println!("cargo:rustc-link-lib=pthread");
        println!("cargo:rustc-link-lib=dl");
    }

    let inc = [src.join("include"), src.join("ggml/include"), src.join("tools/mtmd")];
    let mut b = bindgen::Builder::default()
        .header("wrapper.h")
        .allowlist_function("(llama|mtmd|ggml_backend|gguf)_.*")
        .allowlist_type("(llama|mtmd|ggml)_.*")
        .allowlist_var("(LLAMA|MTMD|GGML)_.*")
        .default_enum_style(bindgen::EnumVariation::Consts)
        .prepend_enum_name(false)
        .derive_default(true)
        .layout_tests(false)
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()));
    for i in &inc {
        b = b.clang_arg(format!("-I{}", i.display()));
    }
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    b.generate().expect("bindgen failed (is libclang installed?)").write_to_file(out.join("bindings.rs")).unwrap();
}
