use criterion::{criterion_group, criterion_main, Criterion};

use gnostr_modal::bench::{Bench, HasherBench};

fn bench(c: &mut Criterion) {
  let bench = HasherBench::init();

  c.bench_function(&bench.name(), |b| b.iter(|| bench.iteration()));
}

criterion_group!(benches, bench);

criterion_main!(benches);
