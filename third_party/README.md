# Third-party code kept here

## dimpl 0.9.0

DTLS for str0m (`str0m-rust-crypto` → `dimpl`), from crates.io, MIT OR Apache-2.0
(`dimpl/LICENSE-MIT.txt`, `dimpl/LICENSE-APACHE.txt`). Used through `[patch.crates-io]`
in the workspace `Cargo.toml`.

One change: upstream's `rcgen` feature also turns on `aws-lc-rs`, and its `rcgen`
dependency uses the `aws_lc_rs` backend. aws-lc is C that needs the target's libc headers,
so the Linux host could not be cross-compiled from macOS. Here `rcgen = ["dep:rcgen"]`
and rcgen uses `ring`, which cross-compiles like the rest; and the certificate
fingerprint in `src/certificate.rs` is SHA-256 from `sha2` instead of aws-lc. Nothing else
in the sources is changed; upstream's tests, lints config and agent notes are left out.

Drop this patch once str0m's pure-Rust crypto no longer pulls in aws-lc.
