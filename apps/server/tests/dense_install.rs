#[allow(dead_code)]
#[path = "../examples/dense_install_baseline/workload.rs"]
mod fixture;
mod support;
use fixture::*;
use support::Directory;
use torse_server::{bootstrap as base, execution::*, *};
use torse_types::*;
use torse_world::*;

#[test]
fn dense_batch_keeps_one_atomic_cut_and_exact_recovery_and_rejects_a_bad_last_page() {
    let dir = Directory::new();
    let (mut server, pages, template) = create(dir.acquire()).unwrap();
    let input = column(server.authority().unwrap(), 3, 0, &template, false);
    assert_eq!(
        input
            .iter()
            .filter(|p| matches!(p.sections[0].cells, SnapshotCells::Dense(_)))
            .count(),
        16
    );
    let mut invalid = input.clone();
    invalid[23].sections[0].cells = SnapshotCells::Dense(vec![BlockStateId(65535); 4096]);
    assert!(
        server
            .install_section_batch(&pages, invalid, &mut NoReplicas)
            .is_err()
    );
    assert_eq!(server.authority().unwrap().cursor().seq, CommitSeq(0));
    assert_eq!(
        server
            .authority()
            .unwrap()
            .world()
            .entities()
            .next_entity_id(),
        1
    );
    let before = server.flush_metrics().calls;
    let CommitStep::Durable(done) = server
        .install_section_batch(&pages, input, &mut NoReplicas)
        .unwrap()
    else {
        panic!("not durable")
    };
    assert_eq!(done.publication.at.seq, CommitSeq(1));
    assert_eq!(server.flush_metrics().calls - before, 1);
    let d = server.section_install_diagnostics();
    assert!(d.page_preparation + d.validation + d.commit + d.durability_wait <= d.elapsed);
    let world = server.authority().unwrap().world();
    let expected = world
        .capture_snapshot(torse_sim::streaming::page_meta(world))
        .unwrap();
    drop(server);
    let (world, actions) = base::world().unwrap();
    let recovered = base::Files::open_file(
        dir.acquire(),
        world.coordinator().compatibility(),
        base::Codec::new(base::codec_limits()).unwrap(),
        file_policy(),
        |binding| {
            Ok(ExecutionRecovery::new(
                binding,
                world.registry(),
                world.schemas(),
                actions,
                vec![],
                limits(),
            )
            .with_section_pages(pages.clone()))
        },
    )
    .unwrap();
    let world = recovered.authority().unwrap().world();
    assert_eq!(
        world
            .capture_snapshot(torse_sim::streaming::page_meta(world))
            .unwrap(),
        expected
    );
}

#[test]
fn a_borrowed_page_iterator_cannot_lie_about_its_length_or_publish_a_prefix() {
    struct Reported<'a> {
        inner: std::slice::Iter<'a, Snapshot>,
        length: usize,
    }
    impl<'a> Iterator for Reported<'a> {
        type Item = &'a Snapshot;
        fn next(&mut self) -> Option<Self::Item> {
            self.inner.next()
        }
    }
    impl ExactSizeIterator for Reported<'_> {
        fn len(&self) -> usize {
            self.length
        }
    }
    let dir = Directory::new();
    let (server, _, template) = create(dir.acquire()).unwrap();
    let pages = column(server.authority().unwrap(), 3, 0, &template, false);
    let world = server.authority().unwrap().world();
    for length in [1, 25] {
        assert!(
            world
                .prepare_generated_sections_iter(
                    Reported {
                        inner: pages.iter(),
                        length
                    },
                    limits().runtime
                )
                .is_err()
        );
        assert_eq!(world.seq(), CommitSeq(0));
        assert!(world.store().is_empty());
    }
}
