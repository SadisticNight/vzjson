use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use vzjson::{Doc, ReadFlags, WriteFlags};

fn make_dense_numbers_json(count: usize) -> Vec<u8> {
    let mut s = String::from("[");
    for i in 0..count {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&(i % 10000).to_string());
    }
    s.push(']');
    s.into_bytes()
}

fn make_strings_json(count: usize, str_len: usize) -> Vec<u8> {
    let payload: String = (0..str_len).map(|i| (b'a' + (i % 26) as u8) as char).collect();
    let mut s = String::from("[");
    for i in 0..count {
        if i > 0 {
            s.push(',');
        }
        s.push('"');
        s.push_str(&payload);
        s.push('"');
    }
    s.push(']');
    s.into_bytes()
}

fn make_numbers_json(count: usize) -> Vec<u8> {
    let mut s = String::from("[");
    for i in 0..count {
        if i > 0 {
            s.push(',');
        }
        if i % 3 == 0 {
            s.push_str(&format!("{}", 10_000_000_000u64 + i as u64 * 1234567));
        } else if i % 3 == 1 {
            s.push_str(&format!("-{}", 1_000_000_000i64 + i as i64 * 98765));
        } else {
            s.push_str(&format!("{}.12345678", 12345678 + i));
        }
    }
    s.push(']');
    s.into_bytes()
}

fn make_mixed_json(records: usize) -> Vec<u8> {
    let mut s = String::from(r#"{"users":["#);
    for i in 0..records {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&format!(
            r#"{{"id":{},"name":"user_profile_number_{}_active","email":"user{}@example.com/api/v1","score":{}.87654321,"active":{},"tags":["rust","json","swar","performance"]}}"#,
            100_000_000 + i,
            i,
            i,
            10_000_000 + i,
            if i % 2 == 0 { "true" } else { "false" }
        ));
    }
    s.push_str("]}");
    s.into_bytes()
}

fn make_pretty_json(records: usize) -> Vec<u8> {
    let doc = Doc::read(&make_mixed_json(records), ReadFlags::NOFLAG).unwrap();
    doc.write(WriteFlags::PRETTY).unwrap().into_bytes()
}

fn bench_vzjson_vs_serde(c: &mut Criterion) {
    let dense_numbers = make_dense_numbers_json(20_000);
    let long_strings = make_strings_json(1000, 256);
    let numbers_json = make_numbers_json(5000);
    let mixed_json = make_mixed_json(1000);
    let pretty_json = make_pretty_json(400);

    let read_cases: [(&str, &[u8]); 5] = [
        ("dense_numbers_20k", &dense_numbers),
        ("long_strings_1000x256", &long_strings),
        ("numbers_5000", &numbers_json),
        ("mixed_1000_records", &mixed_json),
        ("pretty_400_records", &pretty_json),
    ];

    let mut read_group = c.benchmark_group("read");
    for (name, data) in read_cases {
        read_group.throughput(Throughput::Bytes(data.len() as u64));
        read_group.bench_with_input(BenchmarkId::new("vzjson", name), data, |b, input| {
            b.iter(|| Doc::read(black_box(input), ReadFlags::NOFLAG).unwrap())
        });
        read_group.bench_with_input(BenchmarkId::new("serde_json_value", name), data, |b, input| {
            b.iter(|| serde_json::from_slice::<serde_json::Value>(black_box(input)).unwrap())
        });
    }
    read_group.finish();

    let dense_vz = Doc::read(&dense_numbers, ReadFlags::NOFLAG).unwrap();
    let strings_vz = Doc::read(&long_strings, ReadFlags::NOFLAG).unwrap();
    let mixed_vz = Doc::read(&mixed_json, ReadFlags::NOFLAG).unwrap();
    let pretty_vz = Doc::read(&pretty_json, ReadFlags::NOFLAG).unwrap();

    let dense_serde: serde_json::Value = serde_json::from_slice(&dense_numbers).unwrap();
    let strings_serde: serde_json::Value = serde_json::from_slice(&long_strings).unwrap();
    let mixed_serde: serde_json::Value = serde_json::from_slice(&mixed_json).unwrap();
    let pretty_serde: serde_json::Value = serde_json::from_slice(&pretty_json).unwrap();

    let mut write_group = c.benchmark_group("write");

    write_group.throughput(Throughput::Bytes(dense_numbers.len() as u64));
    write_group.bench_function(BenchmarkId::new("vzjson", "dense_numbers_20k"), |b| {
        b.iter(|| black_box(&dense_vz).write(WriteFlags::NOFLAG).unwrap())
    });
    write_group.bench_function(BenchmarkId::new("serde_json_value", "dense_numbers_20k"), |b| {
        b.iter(|| serde_json::to_vec(black_box(&dense_serde)).unwrap())
    });

    write_group.throughput(Throughput::Bytes(long_strings.len() as u64));
    write_group.bench_function(BenchmarkId::new("vzjson", "long_strings_1000x256"), |b| {
        b.iter(|| black_box(&strings_vz).write(WriteFlags::NOFLAG).unwrap())
    });
    write_group.bench_function(BenchmarkId::new("serde_json_value", "long_strings_1000x256"), |b| {
        b.iter(|| serde_json::to_vec(black_box(&strings_serde)).unwrap())
    });

    write_group.throughput(Throughput::Bytes(mixed_json.len() as u64));
    write_group.bench_function(BenchmarkId::new("vzjson", "mixed_1000_records"), |b| {
        b.iter(|| black_box(&mixed_vz).write(WriteFlags::NOFLAG).unwrap())
    });
    write_group.bench_function(BenchmarkId::new("serde_json_value", "mixed_1000_records"), |b| {
        b.iter(|| serde_json::to_vec(black_box(&mixed_serde)).unwrap())
    });

    write_group.throughput(Throughput::Bytes(pretty_json.len() as u64));
    write_group.bench_function(BenchmarkId::new("vzjson", "mixed_400_pretty"), |b| {
        b.iter(|| black_box(&pretty_vz).write(WriteFlags::PRETTY).unwrap())
    });
    write_group.bench_function(BenchmarkId::new("serde_json_value", "mixed_400_pretty"), |b| {
        b.iter(|| serde_json::to_vec_pretty(black_box(&pretty_serde)).unwrap())
    });

    write_group.finish();
}

criterion_group!(benches, bench_vzjson_vs_serde);
criterion_main!(benches);
