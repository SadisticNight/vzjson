use criterion::{black_box, criterion_group, criterion_main, Criterion, Throughput};
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

fn bench_vzjson(c: &mut Criterion) {
    let dense_numbers = make_dense_numbers_json(20_000);
    let long_strings = make_strings_json(1000, 256);
    let numbers_json = make_numbers_json(5000);
    let mixed_json = make_mixed_json(1000);
    let pretty_json = make_pretty_json(400);

    let dense_doc = Doc::read(&dense_numbers, ReadFlags::NOFLAG).unwrap();
    let strings_doc = Doc::read(&long_strings, ReadFlags::NOFLAG).unwrap();
    let mixed_doc = Doc::read(&mixed_json, ReadFlags::NOFLAG).unwrap();

    let mut group = c.benchmark_group("vzjson_read");
    group.throughput(Throughput::Bytes(dense_numbers.len() as u64));
    group.bench_function("dense_numbers_20k", |b| {
        b.iter(|| Doc::read(black_box(&dense_numbers), ReadFlags::NOFLAG).unwrap())
    });

    group.throughput(Throughput::Bytes(long_strings.len() as u64));
    group.bench_function("long_strings_1000x256", |b| {
        b.iter(|| Doc::read(black_box(&long_strings), ReadFlags::NOFLAG).unwrap())
    });

    group.throughput(Throughput::Bytes(numbers_json.len() as u64));
    group.bench_function("numbers_5000", |b| {
        b.iter(|| Doc::read(black_box(&numbers_json), ReadFlags::NOFLAG).unwrap())
    });

    group.throughput(Throughput::Bytes(mixed_json.len() as u64));
    group.bench_function("mixed_1000_records", |b| {
        b.iter(|| Doc::read(black_box(&mixed_json), ReadFlags::NOFLAG).unwrap())
    });

    group.throughput(Throughput::Bytes(pretty_json.len() as u64));
    group.bench_function("pretty_400_records", |b| {
        b.iter(|| Doc::read(black_box(&pretty_json), ReadFlags::NOFLAG).unwrap())
    });
    group.finish();

    let mut write_group = c.benchmark_group("vzjson_write");
    write_group.throughput(Throughput::Bytes(dense_numbers.len() as u64));
    write_group.bench_function("dense_numbers_20k", |b| {
        b.iter(|| black_box(&dense_doc).write(WriteFlags::NOFLAG).unwrap())
    });

    write_group.throughput(Throughput::Bytes(long_strings.len() as u64));
    write_group.bench_function("long_strings_1000x256", |b| {
        b.iter(|| black_box(&strings_doc).write(WriteFlags::NOFLAG).unwrap())
    });

    write_group.throughput(Throughput::Bytes(mixed_json.len() as u64));
    write_group.bench_function("mixed_1000_records", |b| {
        b.iter(|| black_box(&mixed_doc).write(WriteFlags::NOFLAG).unwrap())
    });

    let pretty_doc = Doc::read(&pretty_json, ReadFlags::NOFLAG).unwrap();

    write_group.throughput(Throughput::Bytes(pretty_json.len() as u64));
    write_group.bench_function("mixed_400_pretty", |b| {
        b.iter(|| black_box(&pretty_doc).write(WriteFlags::PRETTY).unwrap())
    });
    write_group.finish();
}

criterion_group!(benches, bench_vzjson);
criterion_main!(benches);
