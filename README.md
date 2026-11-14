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

Measured on Windows x86_64 (`opt-level = 3`, `lto = "fat"`, `codegen-units = 1`):

| Benchmark | Mean Time | Throughput |
| --- | ---: | ---: |
| `vzjson_read/dense_numbers_20k` | 319.08 µs | 292.25 MiB/s |
| `vzjson_read/long_strings_1000x256` | 128.88 µs | 1.87 GiB/s |
| `vzjson_read/numbers_5000` | 119.28 µs | 559.62 MiB/s |
| `vzjson_read/mixed_1000_records` | 361.98 µs | 469.73 MiB/s |
| `vzjson_read/pretty_400_records` | 192.41 µs | 715.74 MiB/s |
| `vzjson_write/dense_numbers_20k` | 171.96 µs | 542.27 MiB/s |
| `vzjson_write/long_strings_1000x256` | 19.77 µs | 12.20 GiB/s |
| `vzjson_write/mixed_1000_records` | 155.22 µs | 1.07 GiB/s |
| `vzjson_write/mixed_400_pretty` | 112.62 µs | 1.19 GiB/s |

## Author

- **Luis Morinigo** — [LinkedIn](https://www.linkedin.com/in/luis-morinigo/)

## Acknowledgments

`vzjson` is a pure-Rust port/adaptation inspired by YaoYuan's C library **[yyjson](https://github.com/ibireme/yyjson)** (v0.13.0).

## License

Licensed under the [MIT License](LICENSE).
