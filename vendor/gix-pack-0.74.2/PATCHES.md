# Local patch provenance

This copy came from the gothab repository, which carries the same patch.

- Package: `gix-pack 0.74.2`
- Source: https://crates.io/crates/gix-pack/0.74.2
- Crate SHA-256: `3d199d515cbdcf05f531f3859be9771fc12acac7b7fe0f5b581b2c1ab1c3a61e`
- Upstream repository: https://github.com/GitoxideLabs/gitoxide
- Published VCS commit: `3ebca8b66017ab2dd02a38f75f78f485bee1ded8`
- License: MIT OR Apache-2.0; both upstream license files are retained here.

## Local modification

`src/data/file/decode/entry.rs` gives deltified objects independent allocation
lifetimes modeled on C Git's `unpack_entry()` and `patch_delta()` implementation:
one current base object, one exact-sized result object, and a separate buffer of
decompressed delta instructions. After each delta succeeds, the prior base is
released before allocating the next result. This replaces upstream's single
growing `[source][target][instructions]` vector, whose reallocation could
transiently retain both the old and new aggregate allocations.

The public decoder, resolver, and cache APIs are unchanged. The patch preserves
complete zlib-stream and delta-size validation, allocation limits, application
order, final-only cache insertion, and outcome accounting. It additionally
rejects an out-of-pack resolver result whose reported end exceeds the bytes it
wrote. Intermediate objects are deliberately not cached because that would
change gix's cache behavior rather than merely its allocation strategy.

This remains a full-object decoder, like C Git for deltified pack entries; it is
not streaming. Peak live data still includes the current base, exact result,
and decompressed delta instructions. Native RSS is allocator-dependent, so
capacity and heap-profile measurements should accompany it.

When upgrading, extract the new crates.io archive, verify its checksum, check
whether upstream has adopted an independent-buffer decoder, and reapply this
localized `resolve_deltas()` patch only if it remains necessary. Compare against
C Git's `packfile.c::unpack_entry` and `patch-delta.c::patch_delta`, while
retaining gix's existing final-only cache semantics.
