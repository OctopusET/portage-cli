use std::collections::HashSet;

use criterion::{Criterion, Throughput, black_box, criterion_group, criterion_main};
use portage_atom::{Cpn, Cpv, Dep};
use portage_atom_resolvo::{
    DepEntry, InMemoryRepository, PackageDeps, PackageMetadata, PortageDependencyProvider,
    UseConfig, interner,
};
use resolvo::{Problem, Solver};

fn bench_cpn_parsing(c: &mut Criterion) {
    let inputs = [
        "dev-lang/rust",
        "app-editors/vim",
        "media-video/ffmpeg",
        "sys-kernel/gentoo-sources",
        "dev-python/numpy",
    ];

    c.bench_function("cpn_parse", |b| {
        b.iter(|| {
            for input in &inputs {
                black_box(Cpn::parse(input).unwrap());
            }
        })
    });
}

fn bench_cpv_parsing(c: &mut Criterion) {
    let inputs = [
        "dev-lang/rust-1.75.0",
        "app-editors/vim-9.0.2167",
        "media-video/ffmpeg-6.1",
        "sys-kernel/gentoo-sources-6.6.13",
        "dev-python/numpy-1.26.3",
    ];

    c.bench_function("cpv_parse", |b| {
        b.iter(|| {
            for input in &inputs {
                black_box(Cpv::parse(input).unwrap());
            }
        })
    });
}

fn bench_dep_parsing(c: &mut Criterion) {
    let inputs = [
        "dev-lang/rust",
        ">=dev-lang/rust-1.75.0",
        ">=dev-lang/rust-1.75.0:0",
        ">=dev-lang/rust-1.75.0:0[llvm_targets_AMDGPU]",
        "!!>=dev-lang/rust-1.75.0:0/1.75::gentoo[llvm_targets_AMDGPU,-debug]",
    ];

    c.bench_function("dep_parse", |b| {
        b.iter(|| {
            for input in &inputs {
                black_box(Dep::parse(input).unwrap());
            }
        })
    });
}

fn bench_cpn_comparison(c: &mut Criterion) {
    let cpns: Vec<Cpn> = (0..1000)
        .map(|i| Cpn::new(format!("cat-{}", i / 100), format!("pkg-{}", i)))
        .collect();

    c.bench_function("cpn_compare", |b| {
        b.iter(|| {
            for i in 0..cpns.len() - 1 {
                black_box(cpns[i] == cpns[i + 1]);
            }
        })
    });
}

fn bench_repository_add(c: &mut Criterion) {
    let mut group = c.benchmark_group("repository");
    group.throughput(Throughput::Elements(1000));

    group.bench_function("add_1000_packages", |b| {
        b.iter(|| {
            let mut repo = InMemoryRepository::new();
            for i in 0..1000 {
                let cpv = Cpv::parse(&format!("cat/pkg-{i}-1.0")).unwrap();
                let meta = PackageMetadata {
                    cpv,
                    slot: None,
                    subslot: None,
                    iuse: vec![],
                    use_flags: Default::default(),
                    repo: None,
                    dependencies: PackageDeps::default(),
                };
                repo.add(meta);
            }
            black_box(repo);
        })
    });

    group.finish();
}

fn bench_string_alloc(c: &mut Criterion) {
    c.bench_function("create_same_string_1000x", |b| {
        b.iter(|| {
            for _ in 0..1000 {
                black_box(Cpn::new("dev-lang".to_string(), "rust".to_string()));
            }
        })
    });
}

fn pkg(cpv: &str, slot: &str, deps: Vec<DepEntry>) -> PackageMetadata {
    PackageMetadata {
        cpv: Cpv::parse(cpv).unwrap(),
        slot: Some(interner::Interned::intern(slot)),
        subslot: None,
        iuse: vec![],
        use_flags: HashSet::new(),
        repo: None,
        dependencies: PackageDeps {
            depend: deps,
            ..PackageDeps::default()
        },
    }
}

fn build_realistic_repo() -> InMemoryRepository {
    let mut repo = InMemoryRepository::new();

    // sys-libs/zlib: two versions
    repo.add(pkg("sys-libs/zlib-1.2.13", "0", vec![]));
    repo.add(pkg("sys-libs/zlib-1.3.1", "0", vec![]));

    // app-arch/bzip2
    repo.add(pkg("app-arch/bzip2-1.0.8-r4", "0", vec![]));

    // dev-libs/expat
    repo.add(pkg("dev-libs/expat-2.6.2", "0", vec![]));

    // dev-libs/openssl: depends on zlib, weak-blocks libressl
    repo.add(pkg(
        "dev-libs/openssl-3.1.7",
        "0",
        vec![
            DepEntry::Atom(Dep::parse(">=sys-libs/zlib-1.2.13").unwrap()),
            DepEntry::Atom(Dep::parse("!dev-libs/libressl").unwrap()),
        ],
    ));
    repo.add(pkg(
        "dev-libs/openssl-3.2.1",
        "0",
        vec![
            DepEntry::Atom(Dep::parse(">=sys-libs/zlib-1.2.13").unwrap()),
            DepEntry::Atom(Dep::parse("!dev-libs/libressl").unwrap()),
        ],
    ));

    // dev-libs/libressl: alternative TLS
    repo.add(pkg(
        "dev-libs/libressl-3.9.2",
        "0",
        vec![
            DepEntry::Atom(Dep::parse("sys-libs/zlib").unwrap()),
            DepEntry::Atom(Dep::parse("!!dev-libs/openssl").unwrap()),
        ],
    ));

    // media-libs/libpng
    repo.add(pkg(
        "media-libs/libpng-1.6.43",
        "0",
        vec![DepEntry::Atom(
            Dep::parse(">=sys-libs/zlib-1.2.13").unwrap(),
        )],
    ));

    // dev-lang/python: multi-slot
    let python_base = vec![
        DepEntry::Atom(Dep::parse(">=sys-libs/zlib-1.2.13").unwrap()),
        DepEntry::Atom(Dep::parse("app-arch/bzip2").unwrap()),
        DepEntry::UseConditional {
            flag: interner::Interned::intern("xml"),
            negate: false,
            children: vec![DepEntry::Atom(Dep::parse("dev-libs/expat").unwrap())],
        },
    ];
    repo.add(pkg("dev-lang/python-3.11.9", "3.11", python_base.clone()));
    repo.add(pkg("dev-lang/python-3.12.4", "3.12", python_base));

    // dev-python/certifi
    repo.add(pkg(
        "dev-python/certifi-2024.2.2",
        "0",
        vec![DepEntry::Atom(Dep::parse("dev-lang/python:*").unwrap())],
    ));

    // net-misc/curl
    repo.add(pkg(
        "net-misc/curl-8.7.1",
        "0",
        vec![
            DepEntry::Atom(Dep::parse(">=sys-libs/zlib-1.2.13").unwrap()),
            DepEntry::AnyOf(vec![
                DepEntry::Atom(Dep::parse("dev-libs/openssl").unwrap()),
                DepEntry::Atom(Dep::parse("dev-libs/libressl").unwrap()),
            ]),
            DepEntry::UseConditional {
                flag: interner::Interned::intern("ssl"),
                negate: false,
                children: vec![DepEntry::Atom(Dep::parse("dev-python/certifi").unwrap())],
            },
        ],
    ));

    // app-portage/gentoolkit
    repo.add(pkg(
        "app-portage/gentoolkit-0.6.3",
        "0",
        vec![
            DepEntry::Atom(Dep::parse("dev-lang/python:3.12").unwrap()),
            DepEntry::Atom(Dep::parse("dev-python/certifi").unwrap()),
        ],
    ));

    // www-client/firefox
    repo.add(pkg(
        "www-client/firefox-125.0.3",
        "0",
        vec![
            DepEntry::Atom(Dep::parse("dev-lang/python:3.11").unwrap()),
            DepEntry::Atom(Dep::parse("dev-lang/python:3.12").unwrap()),
            DepEntry::Atom(Dep::parse("media-libs/libpng").unwrap()),
            DepEntry::Atom(Dep::parse(">=dev-libs/openssl-3.2.0:0=").unwrap()),
        ],
    ));

    repo
}

fn bench_provider_construction(c: &mut Criterion) {
    let repo = build_realistic_repo();
    let use_config = UseConfig::from(
        ["ssl", "xml"]
            .iter()
            .map(|s| interner::Interned::intern(*s))
            .collect::<HashSet<_>>(),
    );

    c.bench_function("provider_construction", |b| {
        b.iter(|| {
            black_box(PortageDependencyProvider::new(&repo, &use_config));
        })
    });
}

fn bench_solve_resolution(c: &mut Criterion) {
    let repo = build_realistic_repo();
    let use_config = UseConfig::from(
        ["ssl", "xml"]
            .iter()
            .map(|s| interner::Interned::intern(*s))
            .collect::<HashSet<_>>(),
    );

    c.bench_function("solve_resolution", |b| {
        b.iter(|| {
            let mut provider = PortageDependencyProvider::new(&repo, &use_config);
            let reqs: Vec<_> = [
                "net-misc/curl",
                "app-portage/gentoolkit",
                "www-client/firefox",
            ]
            .iter()
            .map(|s| provider.intern_requirement(&Dep::parse(s).unwrap()))
            .collect();
            let problem = Problem::new().requirements(reqs);
            let mut solver = Solver::new(provider);
            let result = solver.solve(problem);
            black_box(result);
        })
    });
}

fn bench_solve_resolution_isolated(c: &mut Criterion) {
    let repo = build_realistic_repo();
    let use_config = UseConfig::from(
        ["ssl", "xml"]
            .iter()
            .map(|s| interner::Interned::intern(*s))
            .collect::<HashSet<_>>(),
    );

    let mut group = c.benchmark_group("resolution");
    group.throughput(Throughput::Elements(3));

    group.bench_function("solve_3_requirements", |b| {
        b.iter_batched(
            || PortageDependencyProvider::new(&repo, &use_config),
            |mut provider| {
                let reqs: Vec<_> = [
                    "net-misc/curl",
                    "app-portage/gentoolkit",
                    "www-client/firefox",
                ]
                .iter()
                .map(|s| provider.intern_requirement(&Dep::parse(s).unwrap()))
                .collect();
                let problem = Problem::new().requirements(reqs);
                let mut solver = Solver::new(provider);
                solver.solve(problem)
            },
            criterion::BatchSize::SmallInput,
        )
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_cpn_parsing,
    bench_cpv_parsing,
    bench_dep_parsing,
    bench_cpn_comparison,
    bench_repository_add,
    bench_string_alloc,
    bench_provider_construction,
    bench_solve_resolution,
    bench_solve_resolution_isolated,
);

criterion_main!(benches);
