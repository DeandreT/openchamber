//! Builds the static libghostty-vt archive from the pinned Ghostty source.
//!
//! The build never touches the network or Git. `native/scripts/fetch-ghostty.py`
//! downloads the source tarball and Zig's package cache once; this script checks
//! the tarball's SHA-256, unpacks it into `OUT_DIR` and runs Zig 0.15.2 against
//! the prefetched packages. See `DOCUMENTATION.md` next to this file.

use std::env;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Ghostty revision the checked-in bindings and the web build both use
/// (`packages/ui/src/lib/ghostty/vendor/VERSION`).
const GHOSTTY_COMMIT: &str = "9f62873bf195e4d8a762d768a1405a5f2f7b1697";
/// SHA-256 of `https://github.com/ghostty-org/ghostty/archive/<commit>.tar.gz`.
const GHOSTTY_TARBALL_SHA256: &str =
    "16c78c876b07cdf7c88e6dcd622ce88b9723329b07e82a04a7b8c00d1586d226";
/// The only Zig release this Ghostty revision builds with.
const ZIG_VERSION: &str = "0.15.2";
const CACHE_ENV: &str = "OPENCHAMBER_GHOSTTY_CACHE";
const FETCH_HINT: &str = "run `python3 native/scripts/fetch-ghostty.py` from the repository root";

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=ZIG");
    println!("cargo:rerun-if-env-changed={CACHE_ENV}");

    // docs.rs has no Zig toolchain; the checked-in bindings are enough there.
    if env::var_os("DOCS_RS").is_some() {
        return;
    }
    assert!(
        env::var_os("CARGO_FEATURE_LINK_DYNAMIC").is_none(),
        "libghostty-vt-sys: the vendored build links the static archive only"
    );
    assert!(
        env::var_os("CARGO_FEATURE_KITTY_GRAPHICS").is_none(),
        "libghostty-vt-sys: the vendored build excludes Kitty graphics; depend on \
         libghostty-vt with default-features = false"
    );

    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("Cargo sets OUT_DIR"));
    let target = env::var("TARGET").expect("Cargo sets TARGET");
    let host = env::var("HOST").expect("Cargo sets HOST");

    let cache = cache_dir();
    let zig = locate_zig();
    let source = unpack_source(&cache, &out_dir);
    let packages = cache.join("zig").join("p");
    assert!(
        packages.is_dir(),
        "libghostty-vt-sys: the Zig package cache {} is missing; {FETCH_HINT}",
        packages.display()
    );
    verify_packages(&zig, &source, &packages, &out_dir);

    let install_prefix = out_dir.join("ghostty-install");
    let mut build = Command::new(&zig);
    build
        .arg("build")
        .arg("-Demit-lib-vt")
        // Optimized in every Cargo profile: a Debug Zig build needs milliseconds
        // per line of output and stalls tests that scroll.
        .arg("-Doptimize=ReleaseFast")
        // Zig otherwise targets the build machine's CPU features.
        .arg("-Dcpu=baseline")
        .arg("-Demit-xcframework=false")
        .arg("-Dapp-runtime=none")
        .arg("--prefix")
        .arg(&install_prefix)
        .arg("--cache-dir")
        .arg(out_dir.join("zig-cache"))
        .arg("--global-cache-dir")
        .arg(out_dir.join("zig-global-cache"))
        .arg("--system")
        .arg(&packages)
        .current_dir(&source);
    if target != host {
        build.arg(format!("-Dtarget={}", zig_target(&target)));
    }
    run(build, "zig build");

    let lib_dir = install_prefix.join("lib");
    let archive = if target.contains("windows") {
        "ghostty-vt-static.lib"
    } else {
        "libghostty-vt.a"
    };
    assert!(
        lib_dir.join(archive).is_file(),
        "libghostty-vt-sys: zig build did not produce {}",
        lib_dir.join(archive).display()
    );
    println!("cargo:rustc-link-search=native={}", lib_dir.display());
    println!("cargo:rustc-link-lib=static=ghostty-vt");
    println!("cargo:include={}", install_prefix.join("include").display());
}

/// The manifest comes from the digest-checked archive. Its pinned format has
/// one package hash per two-space-indented object key; nested keys are metadata.
fn package_hashes(manifest: &str) -> Vec<&str> {
    let hashes: Vec<_> = manifest
        .lines()
        .filter_map(|line| line.strip_prefix("  \"")?.strip_suffix("\": {"))
        .collect();
    assert!(
        !hashes.is_empty(),
        "libghostty-vt-sys: no package hashes in the pinned manifest; {FETCH_HINT}"
    );
    for hash in &hashes {
        assert!(
            !hash.starts_with('.')
                && hash
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')),
            "libghostty-vt-sys: invalid package hash; {FETCH_HINT}"
        );
    }
    hashes
}

fn verify_packages(zig: &Path, source: &Path, packages: &Path, out_dir: &Path) {
    let manifest = fs::read_to_string(source.join("build.zig.zon.json")).unwrap_or_else(|error| {
        panic!("libghostty-vt-sys: missing package manifest: {error}; {FETCH_HINT}")
    });
    let verification = out_dir.join("package-verification");
    for expected in package_hashes(&manifest) {
        let package = packages.join(expected);
        println!("cargo:rerun-if-changed={}", package.display());
        assert!(
            package.is_dir(),
            "libghostty-vt-sys: missing package {}; {FETCH_HINT}",
            package.display()
        );
        // A local path fetch only hashes/copies local files. No URL or package
        // dependency resolution is involved, including with networking disabled.
        let result = Command::new(zig)
            .arg("fetch")
            .arg("--global-cache-dir")
            .arg(&verification)
            .arg(&package)
            .output()
            .unwrap_or_else(|error| {
                panic!("libghostty-vt-sys: cannot verify package: {error}; {FETCH_HINT}")
            });
        let actual = String::from_utf8_lossy(&result.stdout);
        assert!(
            result.status.success() && actual.trim() == expected,
            "libghostty-vt-sys: corrupt package {}: expected {expected}, got {}; remove it and {FETCH_HINT}",
            package.display(),
            actual.trim()
        );
    }
    if verification.exists() {
        fs::remove_dir_all(&verification).expect("remove temporary package verification copies");
    }
}

#[cfg(test)]
mod cache_tests {
    use super::*;

    #[test]
    fn missing_and_corrupt_packages_fail_with_fetch_hint() {
        let root = env::temp_dir().join(format!("ghostty-build-integrity-{}", std::process::id()));
        fs::create_dir_all(&root).expect("create test root");
        let packages = root.join("packages");
        fs::create_dir_all(&packages).expect("create package cache");
        let zig = locate_zig();
        let source = root.join("fixture");
        fs::create_dir_all(&source).expect("create fixture");
        fs::write(source.join("payload"), b"original").expect("write fixture");
        let fetched = Command::new(&zig)
            .arg("fetch")
            .arg("--global-cache-dir")
            .arg(root.join("fetch"))
            .arg(&source)
            .output()
            .expect("hash fixture");
        assert!(fetched.status.success(), "fixture hashing succeeds");
        let expected = String::from_utf8(fetched.stdout).expect("hash is UTF-8");
        let expected = expected.trim();
        fs::write(
            root.join("build.zig.zon.json"),
            format!("{{\n  \"{expected}\": {{\n    \"name\": \"fixture\"\n  }}\n}}\n"),
        )
        .expect("write manifest");
        let check_fails = || {
            let error = std::panic::catch_unwind(|| verify_packages(&zig, &root, &packages, &root))
                .expect_err("invalid package must fail before compiling");
            let message = error.downcast_ref::<String>().expect("panic diagnostic");
            assert!(
                message.contains(FETCH_HINT),
                "failure explains how to repair the cache"
            );
        };
        check_fails();
        let package = packages.join(expected);
        fs::rename(root.join("fetch/p").join(expected), &package).expect("move hashed package");
        verify_packages(&zig, &root, &packages, &root);
        fs::write(package.join("payload"), b"corrupt").expect("corrupt fixture");
        check_fails();
        fs::remove_dir_all(root).expect("remove test fixture");
    }
}

/// The fetched inputs live in `native/.cache/ghostty` unless the environment
/// names another directory, such as one shared by several worktrees.
fn cache_dir() -> PathBuf {
    if let Some(dir) = env::var_os(CACHE_ENV).filter(|dir| !dir.is_empty()) {
        return PathBuf::from(dir);
    }
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("Cargo sets it"));
    manifest.join("../../.cache/ghostty")
}

/// Zig comes from `ZIG`, or else `zig` on `PATH`, and must be exactly 0.15.2.
fn locate_zig() -> PathBuf {
    let (zig, origin) = match env::var_os("ZIG").filter(|zig| !zig.is_empty()) {
        Some(zig) => (PathBuf::from(zig), "ZIG"),
        None => (PathBuf::from("zig"), "PATH"),
    };
    let output = Command::new(&zig).arg("version").output();
    let version = match output {
        Ok(output) if output.status.success() => {
            String::from_utf8_lossy(&output.stdout).trim().to_owned()
        }
        Ok(output) => panic!(
            "libghostty-vt-sys: `{} version` failed with {}",
            zig.display(),
            output.status
        ),
        Err(error) => panic!(
            "libghostty-vt-sys: Zig {ZIG_VERSION} is required to build Ghostty, and no Zig \
             was found through {origin} ({error}). Install the official Zig {ZIG_VERSION} \
             tarball and set ZIG to its `zig` binary or put it first on PATH."
        ),
    };
    assert!(
        version == ZIG_VERSION,
        "libghostty-vt-sys: Ghostty {GHOSTTY_COMMIT} builds only with Zig {ZIG_VERSION}, \
         but {} (from {origin}) is {version}. Set ZIG to a Zig {ZIG_VERSION} binary.",
        zig.display()
    );
    zig
}

/// Checks the tarball against the pinned digest and unpacks it into `OUT_DIR`.
fn unpack_source(cache: &Path, out_dir: &Path) -> PathBuf {
    let tarball = cache.join(format!("ghostty-{GHOSTTY_COMMIT}.tar.gz"));
    let digest = sha256_file(&tarball).unwrap_or_else(|error| {
        panic!(
            "libghostty-vt-sys: cannot read the Ghostty source {} ({error}); {FETCH_HINT}",
            tarball.display()
        )
    });
    assert!(
        digest == GHOSTTY_TARBALL_SHA256,
        "libghostty-vt-sys: {} has SHA-256 {digest}, expected {GHOSTTY_TARBALL_SHA256}; \
         delete it and {FETCH_HINT}",
        tarball.display()
    );

    let source = out_dir.join("ghostty-src");
    if source.exists() {
        fs::remove_dir_all(&source)
            .unwrap_or_else(|error| panic!("failed to remove {}: {error}", source.display()));
    }
    fs::create_dir_all(&source)
        .unwrap_or_else(|error| panic!("failed to create {}: {error}", source.display()));
    let mut tar = Command::new("tar");
    tar.arg("-xzf")
        .arg(&tarball)
        .arg("-C")
        .arg(&source)
        .arg("--strip-components=1");
    run(tar, "tar");
    assert!(
        source.join("build.zig").is_file(),
        "libghostty-vt-sys: the unpacked source has no build.zig"
    );
    source
}

fn run(mut command: Command, context: &str) {
    let status = command
        .status()
        .unwrap_or_else(|error| panic!("libghostty-vt-sys: failed to run {context}: {error}"));
    assert!(
        status.success(),
        "libghostty-vt-sys: {context} failed with {status}"
    );
}

fn zig_target(target: &str) -> &'static str {
    match target {
        "x86_64-unknown-linux-gnu" => "x86_64-linux-gnu",
        "aarch64-unknown-linux-gnu" => "aarch64-linux-gnu",
        "aarch64-apple-darwin" => "aarch64-macos-none",
        "x86_64-apple-darwin" => "x86_64-macos-none",
        "x86_64-pc-windows-msvc" => "x86_64-windows-msvc",
        "aarch64-pc-windows-msvc" => "aarch64-windows-msvc",
        other => panic!("libghostty-vt-sys: unsupported cross-compilation target {other}"),
    }
}

/// SHA-256 (FIPS 180-4) of a file, as lowercase hex. Implemented here so the
/// build script needs no extra crate.
fn sha256_file(path: &Path) -> std::io::Result<String> {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut state: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let compress = |state: &mut [u32; 8], block: &[u8]| {
        let mut w = [0u32; 64];
        for (i, word) in block.chunks_exact(4).enumerate() {
            w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = *state;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let t1 = h
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (slot, value) in state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *slot = slot.wrapping_add(value);
        }
    };

    let mut file = fs::File::open(path)?;
    let mut buffer = vec![0u8; 1 << 16];
    let mut pending: Vec<u8> = Vec::with_capacity(64);
    let mut total: u64 = 0;
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        total += read as u64;
        let mut data = &buffer[..read];
        if !pending.is_empty() {
            let take = (64 - pending.len()).min(data.len());
            pending.extend_from_slice(&data[..take]);
            data = &data[take..];
            if pending.len() == 64 {
                compress(&mut state, &pending);
                pending.clear();
            }
        }
        let mut blocks = data.chunks_exact(64);
        for block in &mut blocks {
            compress(&mut state, block);
        }
        pending.extend_from_slice(blocks.remainder());
    }
    pending.push(0x80);
    while pending.len() % 64 != 56 {
        pending.push(0);
    }
    pending.extend_from_slice(&(total * 8).to_be_bytes());
    for block in pending.chunks_exact(64) {
        compress(&mut state, block);
    }
    Ok(state.iter().map(|word| format!("{word:08x}")).collect())
}
