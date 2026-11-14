# vzjson

A high-performance, zero-dependency, pure-Rust JSON library (`#![forbid(unsafe_code)]`) inspired by [yyjson](https://github.com/ibireme/yyjson) (v0.13.0).

## Features

- **100% Safe Rust**: Enforced with `#![forbid(unsafe_code)]` and zero runtime crate dependencies.
- **Flat Pre-Order DOM**: Immutable `Doc` stores 16-byte `Val` nodes in a contiguous `Vec<Val>` with `u64` payloads and a pooled string buffer (plus zero-copy `Doc::read_insitu`).
- **Mutable DOM (`MutDoc`)**: Build, edit, and serialize JSON trees with linked container nodes and `Doc::to_mut()` / `MutDoc::to_doc()`.
- **SWAR Acceleration (Safe Rust)**:
  - 8-byte SWAR string scanning (`swar_scan_str_ascii`) with `trailing_zeros()` byte-index resolution.
  - 4-byte and 8-byte SWAR ASCII digit block parsing (`swar_parse_4_digits`, `swar_parse_8_digits`) in `read_number`.
  - Adaptive reader/writer capacity estimation (`estimate_reader_capacities`, `estimate_writer_capacity`) across dense numeric, long-string, mixed, and pretty-printed payloads.
- **Standards & Extensions**:
  - Strict RFC 8259 parsing and serialization by default (`ReadFlags::NOFLAG`).
  - Optional JSON5-style extensions (comments, trailing commas, `Inf`/`NaN`, hex numbers, single quotes).
  - RFC 6901 JSON Pointer (`doc.ptr(...)` / `val.ptr(...)`) and RFC 7396 JSON Merge Patch.
  - Incremental reader (`IncrReader`) and Schubfach-based `f64` formatting.

## Quick Start

```rust
use vzjson::{Doc, MutDoc, ReadFlags, WriteFlags};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let input = r#"{"project":"vzjson","version":1,"tags":["rust","swar","json"]}"#;
    let doc = Doc::read_str(input, ReadFlags::NOFLAG)?;
    let root = doc.root().unwrap();

    assert_eq!(root.get("project").unwrap().as_str(), Some("vzjson"));
    assert_eq!(root.get("version").unwrap().as_u64(), Some(1));
    assert_eq!(doc.ptr("/tags/0").unwrap().as_str(), Some("rust"));

    // Mutate and serialize
    let mut mdoc = doc.to_mut();
    let mroot = mdoc.root().unwrap();
    mdoc.obj_add_bool(mroot, "safe", true);
    let out = mdoc.write(WriteFlags::PRETTY)?;
    println!("{out}");
    Ok(())
}
```

## Conformance ([JSONTestSuite](https://github.com/nst/JSONTestSuite))

Tested against `nst/JSONTestSuite` (`test_parsing`) with `ReadFlags::NOFLAG`:

- `y_*` (must accept): **95 / 95 passed** (0 failed)
- `n_*` (must reject): **188 / 188 rejected** (0 accepted)
- `i_*` (implementation-defined): **35 / 35 handled without panics** (6 accepted)

To run the suite locally:

```sh
git clone --depth 1 https://github.com/nst/JSONTestSuite.git tests/JSONTestSuite
cargo test --test json_test_suite -- --nocapture
```

## Benchmarks

Run the Criterion benchmark suite:

```sh
cargo bench --bench json_bench
```

Measured on Windows x86_64 (`opt-level = 3`, `lto = "fat"`, `codegen-units = 1`) comparing `vzjson::Doc` against `serde_json::Value`:

| Operation / Workload | `vzjson` | `serde_json::Value` | Relative |
| --- | ---: | ---: | ---: |
| **Read** `mixed_1000_records` | 357.20 µs (`476.01 MiB/s`) | 1.0892 ms (`156.11 MiB/s`) | **3.05x faster** |
| **Read** `pretty_400_records` | 169.14 µs (`814.19 MiB/s`) | 471.90 µs (`291.83 MiB/s`) | **2.79x faster** |
| **Read** `dense_numbers_20k` | 327.06 µs (`285.12 MiB/s`) | 530.62 µs (`175.74 MiB/s`) | **1.62x faster** |
| **Read** `long_strings_1000x256` | 146.02 µs (`1.65 GiB/s`) | 120.29 µs (`2.01 GiB/s`) | 0.82x |
| **Read** `numbers_5000` | 125.42 µs (`532.23 MiB/s`) | 107.28 µs (`622.23 MiB/s`) | 0.86x |
| **Write** `long_strings_1000x256` | 21.60 µs (`11.17 GiB/s`) | 86.38 µs (`2.79 GiB/s`) | **4.00x faster** |
| **Write** `mixed_1000_records` | 146.63 µs (`1.13 GiB/s`) | 157.10 µs (`1.06 GiB/s`) | **1.07x faster** |
| **Write** `mixed_400_pretty` | 109.21 µs (`1.23 GiB/s`) | 107.55 µs (`1.25 GiB/s`) | 0.98x |
| **Write** `dense_numbers_20k` | 174.04 µs (`535.79 MiB/s`) | 104.28 µs (`894.26 MiB/s`) | 0.60x |

## Author

- **Luis Morinigo** — [LinkedIn](https://www.linkedin.com/in/luis-morinigo/)

## Acknowledgments

`vzjson` is a pure-Rust port/adaptation inspired by YaoYuan's C library **[yyjson](https://github.com/ibireme/yyjson)** (v0.13.0).

## License

Licensed under the [MIT License](LICENSE).
