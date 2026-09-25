# Third-party notices

Wheat incorporates material from the third-party projects listed below. Each entry
names what we use, where it lives in this tree, and reproduces the notice its license
requires us to carry.

Add an entry here whenever you vendor, port, or adapt third-party material.

---

## gitoxide `gix-pack`

Wheat vendors a locally modified copy of `gix-pack` 0.74.2 at
`vendor/gix-pack-0.74.2`. The upstream package is part of
[gitoxide](https://github.com/GitoxideLabs/gitoxide), authored by Sebastian
Thiel and distributed under the MIT License or Apache License 2.0. The local
patch gives delta decoding separate base, result and instruction buffers, so
peak memory stays bounded; provenance and maintenance instructions are recorded
in `vendor/gix-pack-0.74.2/PATCHES.md`.

The complete upstream `LICENSE-MIT` and `LICENSE-APACHE` texts are preserved in
the vendored directory.
