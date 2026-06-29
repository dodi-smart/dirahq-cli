//! Benchmark the de-duplication accounting over a realistic multi-session stream.
//! This is the per-status/-report computation; it must stay fast as a day's worth
//! of events accumulates.

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use dira_core::accounting::{per_project_seconds, Signal};
use time::{Duration, OffsetDateTime};

fn stream(n: usize) -> Vec<Signal> {
    // 4 projects, signals ~every 30s, interleaved as if from several sessions.
    let base = OffsetDateTime::UNIX_EPOCH;
    (0..n)
        .map(|i| Signal {
            at: base + Duration::seconds((i as i64) * 30),
            project: Some(format!("github.com/acme/repo{}", i % 4)),
        })
        .collect()
}

fn bench_accounting(c: &mut Criterion) {
    let idle = Duration::minutes(5);
    let mut group = c.benchmark_group("per_project_seconds");
    for n in [100usize, 1_000, 10_000] {
        let signals = stream(n);
        group.bench_with_input(BenchmarkId::from_parameter(n), &signals, |b, s| {
            b.iter(|| per_project_seconds(s, idle));
        });
    }
    group.finish();
}

criterion_group!(benches, bench_accounting);
criterion_main!(benches);
