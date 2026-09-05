#![allow(non_snake_case)]
//! SSC1 v2 — the chunk arithmetic, the seek economy, and the immovable chunk.

use engram_blob::{ChunkError, SealedBlob, open_range, seal_chunked};
use engram_crypto::{Sealer, Secret, derive_dek};
use engram_key::Realm;

const MASTER: [u8; 32] = [0x11; 32];

fn sealer(realm: u32) -> Sealer {
    Sealer::new(derive_dek(&MASTER, Realm(realm)), 0)
}

fn plain(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

#[test]
fn a_chunked_blob_round_trips_whole() {
    let data = plain(1000);
    let blob = seal_chunked(&mut sealer(1), &Secret::new(data.clone()), 256).expect("seal");
    assert_eq!(blob.chunk_count(), 4, "1000 bytes at 256 is 4 chunks");
    let (out, r) = open_range(&derive_dek(&MASTER, Realm(1)), &blob, 0, 1000).expect("open");
    assert_eq!(out, data);
    assert_eq!(r.chunks_opened, 4);
}

#[test]
fn seeking_decrypts_ONLY_the_covering_chunks() {
    // "Seek to minute 40 of 90 decrypts at most 2 MiB" — the property is the
    // COUNT, and the report carries it so this is measured, not believed.
    let data = plain(10_000);
    let blob = seal_chunked(&mut sealer(1), &Secret::new(data.clone()), 1000).expect("seal");
    let dek = derive_dek(&MASTER, Realm(1));

    // A range inside one chunk.
    let (out, r) = open_range(&dek, &blob, 4200, 4300).expect("open");
    assert_eq!(out, &data[4200..4300]);
    assert_eq!((r.chunks_opened, r.chunks_total), (1, 10));

    // A range spanning a boundary.
    let (out, r) = open_range(&dek, &blob, 3900, 4100).expect("open");
    assert_eq!(out, &data[3900..4100]);
    assert_eq!(r.chunks_opened, 2);

    // The empty range opens nothing.
    let (out, r) = open_range(&dek, &blob, 5000, 5000).expect("open");
    assert!(out.is_empty());
    assert_eq!(r.chunks_opened, 0);
}

#[test]
fn a_chunk_moved_WITHIN_the_blob_refuses() {
    // Position is in the AAD: two ciphertexts swapped between positions both
    // fail, they do not decrypt each other's content into the wrong offsets.
    let data = plain(1000);
    let mut blob = seal_chunked(&mut sealer(1), &Secret::new(data), 256).expect("seal");
    let c1 = blob.swap_chunk(1, Vec::new());
    let c2 = blob.swap_chunk(2, c1);
    let _ = blob.swap_chunk(1, c2);
    let dek = derive_dek(&MASTER, Realm(1));
    assert_eq!(
        open_range(&dek, &blob, 256, 512).unwrap_err(),
        ChunkError::Refused { chunk: 1 }
    );
    assert_eq!(
        open_range(&dek, &blob, 512, 768).unwrap_err(),
        ChunkError::Refused { chunk: 2 }
    );
    // Chunks 0 and 3 are untouched and still open.
    assert!(open_range(&dek, &blob, 0, 256).is_ok());
    assert!(open_range(&dek, &blob, 768, 1000).is_ok());
}

#[test]
fn a_chunk_spliced_from_ANOTHER_blob_refuses() {
    // Same realm, same key, same position, same chunk size — but a different
    // blob header (total length differs), so the binding differs.
    let mut blob_a = seal_chunked(&mut sealer(1), &Secret::new(plain(1000)), 256).expect("a");
    let blob_b = seal_chunked(&mut sealer(1), &Secret::new(plain(999)), 256).expect("b");
    let dek = derive_dek(&MASTER, Realm(1));
    let (_, _) = open_range(&dek, &blob_b, 0, 256).expect("b's chunk 0 is fine in b");
    let foreign = blob_b.clone().swap_chunk(0, Vec::new());
    let _ = blob_a.swap_chunk(0, foreign);
    assert_eq!(
        open_range(&dek, &blob_a, 0, 256).unwrap_err(),
        ChunkError::Refused { chunk: 0 }
    );
}

#[test]
fn truncation_fails_on_the_STRUCTURE_before_any_key() {
    // v2's decode refuses a blob whose byte length disagrees with its header
    // — countable by an unkeyed scrubber.
    let blob = seal_chunked(&mut sealer(1), &Secret::new(plain(1000)), 256).expect("seal");
    let encoded = blob.encode();
    let truncated = &encoded[..encoded.len() - 10];
    assert!(matches!(
        SealedBlob::decode(truncated),
        Err(ChunkError::Malformed(_))
    ));
    // And a well-shaped decode round-trips.
    assert_eq!(SealedBlob::decode(&encoded).expect("decode"), blob);
}

#[test]
fn a_count_lie_in_the_header_refuses() {
    // A header rewritten to claim fewer chunks (the truncate-and-fix-header
    // attack) must fail: decode checks n against ceil(total/C), and the AAD
    // binds n into EVERY chunk, so even a consistent lie (total shortened
    // too) fails on the first chunk it opens.
    let data = plain(1000);
    let blob = seal_chunked(&mut sealer(1), &Secret::new(data.clone()), 256).expect("seal");
    let encoded = blob.encode();

    // Rewrite total_len 1000→744 and n 4→3, drop the last chunk's bytes.
    let mut lied = encoded.clone();
    lied[5..13].copy_from_slice(&744u64.to_be_bytes());
    lied[13..17].copy_from_slice(&3u32.to_be_bytes());
    // 744 = 2*256 + 232: chunk 2 would need 232+29 bytes; slice to header + 2
    // full chunks + 261.
    let keep = 17 + 2 * (256 + 29) + 232 + 29;
    lied.truncate(keep);
    let doctored = SealedBlob::decode(&lied).expect("the SHAPE is now self-consistent");
    let dek = derive_dek(&MASTER, Realm(1));
    // ...but chunk 0 was sealed with (total=1000, n=4) in its binding.
    assert_eq!(
        open_range(&dek, &doctored, 0, 256).unwrap_err(),
        ChunkError::Refused { chunk: 0 },
        "the count lie fails on ANY chunk, not only the last — the v2 property"
    );
}

#[test]
fn the_wrong_realm_refuses_every_chunk() {
    let blob = seal_chunked(&mut sealer(1), &Secret::new(plain(300)), 256).expect("seal");
    let dek_b = derive_dek(&MASTER, Realm(2));
    assert!(matches!(
        open_range(&dek_b, &blob, 0, 100),
        Err(ChunkError::Refused { chunk: 0 })
    ));
}

#[test]
fn bad_requests_refuse_before_any_work() {
    let blob = seal_chunked(&mut sealer(1), &Secret::new(plain(100)), 64).expect("seal");
    let dek = derive_dek(&MASTER, Realm(1));
    assert!(matches!(
        open_range(&dek, &blob, 50, 40),
        Err(ChunkError::BadRequest(_))
    ));
    assert!(matches!(
        open_range(&dek, &blob, 0, 101),
        Err(ChunkError::BadRequest(_))
    ));
    assert!(matches!(
        seal_chunked(&mut sealer(1), &Secret::new(plain(10)), 0),
        Err(ChunkError::BadRequest(_))
    ));
}

#[test]
fn an_empty_blob_is_one_empty_chunk_and_round_trips() {
    // Zero-length content still seals (n is max'd to 1) so an empty file is
    // representable, authenticated, and distinguishable from a missing one.
    let blob = seal_chunked(&mut sealer(1), &Secret::new(Vec::new()), 256).expect("seal");
    assert_eq!(blob.chunk_count(), 1);
    let (out, _) = open_range(&derive_dek(&MASTER, Realm(1)), &blob, 0, 0).expect("open");
    assert!(out.is_empty());
    assert_eq!(SealedBlob::decode(&blob.encode()).expect("decode"), blob);
}
