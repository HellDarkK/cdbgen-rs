# cdbgen-rs

`cdbgen-rs` builds DNS blocklist databases in DJB CDB format. It loads the
lists you name in a TOML config, from HTTP(S) or local files, parses the common
DNS blocklist formats, then writes each configured output with an atomic
replace. CDB keys are DNS QNAME wire-format bytes by default.

The release build is a static Linux binary for `x86_64-unknown-linux-musl`.
Release assets are uploaded as the binary itself plus a `.sha256` file. After
downloading it, make it executable:

```bash
chmod +x cdbgen-rs-v0.1.4-x86_64-unknown-linux-musl
```

## Build From Source

You need Rust 1.88 or newer, installed with [rustup](https://rustup.rs). The
crate uses edition 2024 and let chains. TLS uses rustls, so no system OpenSSL
is needed.

```bash
git clone https://github.com/HellDarkK/cdbgen-rs.git
cd cdbgen-rs
cargo build --release --locked
./target/release/cdbgen-rs --version
```

The binary is written to `target/release/cdbgen-rs`. To install it into
`~/.cargo/bin` instead:

```bash
cargo install --path . --locked
```

To build the same static musl binary as the release workflow (the package
install line is for Debian/Ubuntu):

```bash
rustup target add x86_64-unknown-linux-musl
sudo apt-get install -y musl-tools
cargo build --release --locked --target x86_64-unknown-linux-musl
```

That binary is written to
`target/x86_64-unknown-linux-musl/release/cdbgen-rs`.

Run the test suite with:

```bash
cargo test --all-targets --locked
```

## Run It

```bash
cdbgen-rs --config /etc/cdbgen/config.toml
```

Flags:

```text
--dry-run          load and parse, but do not write cache or output files
--verbose          show debug logs
--force-refresh    ignore remote cache validators and stale-cache fallback
```

There is no output selector. The config file is the source of truth: every
output listed there is built.

## Config

Start with the example file, then replace the placeholder URLs and paths:

```bash
sudo mkdir -p /etc/cdbgen /var/lib/cdbgen
sudo cp config.example.toml /etc/cdbgen/config.toml
sudo editor /etc/cdbgen/config.toml
```

```toml
[sources]
hosts_example = "https://example.com/blocklists/hosts.txt"
adblock_example = "https://example.com/blocklists/filter.txt"
unbound_example = "https://example.com/blocklists/unbound.conf"
rpz_example = "https://example.com/blocklists/rpz.txt"

[outputs]
"hosts_example, adblock_example" = "/var/lib/cdbgen/default.cdb"

"hosts_example, unbound_example, rpz_example" = [
    "/var/lib/cdbgen/combined.cdb",
    "/srv/www/blocklists/combined.cdb",
]

adblock_example = { path = "/var/lib/cdbgen/plaintext.cdb", key_format = "plaintext" }
```

Source IDs must match `^[a-zA-Z0-9_-]+$`. A source value is an `http://` or
`https://` URL, a `file://` URL, or a filesystem path. Relative paths resolve
against the directory containing the config file, not the process working
directory.

Local files work the same way:

```toml
[sources]
remote = "https://example.com/blocklists/hosts.txt"
absolute = "/srv/blocklists/local.txt"
file_url = "file:///srv/blocklists/filter.txt"
# If this config lives at /etc/cdbgen/config.toml, this is
# /etc/cdbgen/lists/extra.txt:
relative = "lists/extra.txt"
```

An output value can be one path string or an array of path strings. Arrays are
the normal way to write the same database to more than one place. These compact
forms use `key_format = "wire"`.

For compatibility, this string form also works:

```toml
"hosts_example" = "'/var/lib/cdbgen/a.cdb', '/srv/www/blocklists/a.cdb'"
```

Use object output values when you need to set the key format:

```toml
"hosts_example" = { path = "/var/lib/cdbgen/hosts.cdb", key_format = "wire" }
"adblock_example" = { paths = ["/var/lib/cdbgen/a.cdb", "/srv/www/a.cdb"], key_format = "plaintext" }
```

`key_format = "wire"` stores DNS QNAME wire keys, e.g. `example.com` becomes
`\x07example\x03com\x00`. `key_format = "plaintext"` stores legacy ASCII domain
keys, e.g. `example.com`.

## What It Parses

- hosts/plain-domain lists
- Adblock/uBO exact-domain rules
- Unbound `local-zone` and `local-data` rows
- RPZ `CNAME .` rows
- exact `@@...` allow rules, used as removals from the block set

For hosts/plain-domain input, field 0 auto behavior accepts `domain IP`,
`IP domain`, and bare domains.

## Runtime Behavior

- Loads only sources referenced by configured outputs.
- Loads sources concurrently.
- Reads local file sources directly; they are not cached and ignore
  `--force-refresh`.
- Sends `User-Agent: cdbgen-rs/<version>` for remote sources.
- Handles `gzip`, `brotli`, `zstd`, and `deflate` remote responses.
- Uses `ETag` and `If-Modified-Since` validators for remote sources.
- Uses stale cache data for remote sources when retries fail, unless
  `--force-refresh` is set.
- Skips only outputs that depend on unavailable sources (failed remote fetches
  or unreadable local files), then exits with code `3`.
- Writes sorted, deduplicated CDB keys with empty values.
- Uses DNS QNAME wire-format keys by default; plaintext keys require
  `key_format = "plaintext"`.
- Writes through a same-directory temp file, fsync, atomic rename, and parent
  fsync.
- Refuses to replace an output with an empty CDB.

## Exit Codes

```text
0  success
1  runtime failure or empty-output guard
2  config parse/validation failure
3  source fetch/read failure affecting at least one output
4  output write/atomic replace failure
```

## Current Limits

- Adblock/uBO support is exact-domain only. Regex, wildcard, path,
  element-hiding, and scriptlet rules are skipped.
- Cache directory is fixed at `/var/cache/cdbgen-rs`.
- Local files are read as plain files. Compressed local files (`.gz`, `.zst`,
  ...) are not decompressed.
- YAML config is not supported.
