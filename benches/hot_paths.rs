// Criterion benches for the paths the README's performance pitch rests on:
// serving a cached dataset (the read a hundred job runs hit instead of
// Device42) and assembling a view (the union /metrics also computes). Run with
// `cargo bench`; they are not part of `cargo test`, but clippy compiles them,
// so they cannot rot silently.
use std::collections::HashMap;
use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};

use unified_api::adapters::out::cache::memory::MemoryCache;
use unified_api::application::views::snapshot;
use unified_api::domain::cache_entry::CacheEntry;
use unified_api::domain::dataset::Dataset;
use unified_api::domain::source::Source;
use unified_api::domain::view::View;
use unified_api::ports::cache::CachePort;

// A dataset shaped like a real gather: N hosts, each with a few vars, all in
// one group named after the "datacenter".
fn dataset(hosts: usize, group: &str) -> Dataset {
    let mut hostvars = serde_json::Map::new();
    let mut members = Vec::new();
    for i in 0..hosts {
        let name = format!("host-{}.{}.example", i, group);
        members.push(serde_json::Value::String(name.clone()));
        hostvars.insert(
            name,
            serde_json::json!({
                "ansible_host": format!("10.{}.{}.{}", group.len(), i / 250, i % 250),
                "os": "OracleLinux",
                "role": "web",
                "datacenter": group,
            }),
        );
    }
    serde_json::from_value(serde_json::json!({
        "hostvars": hostvars,
        "groups": { group: { "hosts": members } }
    }))
    .expect("bench dataset")
}

fn source() -> Source {
    serde_yaml_ng::from_str("name: S\nproject_id: p\nscript_path: x\nttl_seconds: 3600\n")
        .expect("bench source")
}

// The cold serialize: what one cache write costs the first reader.
fn serialize_dataset(c: &mut Criterion) {
    let ds = dataset(1000, "dc1");
    c.bench_function("serialize_dataset_1000_hosts", |b| {
        b.iter(|| serde_json::to_vec(black_box(&ds)).expect("serialize"))
    });
}

// The warm read: cache lookup + the serialize-once buffer, which is the whole
// point of the middleware — this is the path that must stay cheap for "a
// hundred job runs cost the origin what one does" to hold.
fn cached_dataset_read(c: &mut Criterion) {
    let cache = MemoryCache::new();
    cache.set("src-a", CacheEntry::new(dataset(1000, "dc1"), 3600));

    c.bench_function("cached_dataset_read_1000_hosts", |b| {
        b.iter(|| {
            let entry = cache.get(black_box("src-a")).expect("cached");
            entry.serialized_json().expect("serialized").bytes.len()
        })
    });
}

// The view union: route every host to its owning member and collect the
// served set — the cost of one view read, and (memoized on the cache
// generation since 0.15) of a /metrics scrape after a write.
fn view_snapshot_union(c: &mut Criterion) {
    let cache = MemoryCache::new();
    cache.set("src-dc1", CacheEntry::new(dataset(500, "dc1"), 3600));
    cache.set("src-dc2", CacheEntry::new(dataset(500, "dc2"), 3600));

    // The inventory source both ownership patterns resolve against
    let mut inventory = dataset(500, "dc1");
    let dc2 = dataset(500, "dc2");
    inventory.hostvars.extend(dc2.hostvars.clone());
    inventory.groups.extend(dc2.groups.clone());
    cache.set("src-inv", CacheEntry::new(inventory, 3600));

    let mut sources: HashMap<String, Source> = HashMap::new();
    for id in ["src-dc1", "src-dc2", "src-inv"] {
        sources.insert(id.to_string(), source());
    }

    let view: View = serde_yaml_ng::from_str(concat!(
        "name: bench\n",
        "members:\n",
        "  - source: src-dc1\n",
        "    owns: { source: src-inv, groups: [\"dc1\"] }\n",
        "  - source: src-dc2\n",
        "    owns: { source: src-inv, groups: [\"dc2\"] }\n",
    ))
    .expect("bench view");

    c.bench_function("view_snapshot_hosts_union_1000_hosts", |b| {
        b.iter(|| {
            let snap = snapshot(
                &cache,
                &sources,
                &unified_api::domain::source::AdvertisedScopeRegistry::new(),
                black_box("vw-bench"),
                &view,
            );
            snap.hosts().len()
        })
    });
}

criterion_group!(
    benches,
    serialize_dataset,
    cached_dataset_read,
    view_snapshot_union
);
criterion_main!(benches);
