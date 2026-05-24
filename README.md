# cdbgen-rs

Generate DJB-compatible wire-format CDB databases from remote blocklists.

Release binaries are built for `x86_64-unknown-linux-musl`, so they are
statically linked and do not require a recent host glibc.

## Usage

```bash
cdbgen-rs --config /etc/cdbgen/config.toml
```

Flags:

```text
--dry-run          fetch, parse, and report without cache/output writes
--verbose          enable debug logs
--force-refresh    bypass cache validators and stale cache fallback
```

`--output` is intentionally deferred in v1.

## Config

Start from the bundled example:

```bash
sudo mkdir -p /etc/cdbgen /var/lib/cdbgen
sudo cp config.example.toml /etc/cdbgen/config.toml
sudo editor /etc/cdbgen/config.toml
```

```toml
[sources]
"adblock-a" = "https://example.com/blocklist1"
"adblock-b" = "https://example.com/blocklist2"

[outputs]
"adblock-a, adblock-b" = "/var/lib/cdbgen/general.cdb"
"adblock-a" = "/var/lib/cdbgen/minimal.cdb"
```

Source IDs must match `^[a-zA-Z0-9_-]+$`. URLs must be `http://` or
`https://`.

The default config path is `/etc/cdbgen/config.toml`.

## Behavior

- Fetches selected sources concurrently with reqwest.
- Uses `User-Agent: cdbgen-rs/<version>`.
- Enables transparent `gzip`, `brotli`, `zstd`, and `deflate` decoding.
- Uses `ETag` and `If-Modified-Since` cache validators.
- Uses stale cache when fetch retries fail, unless `--force-refresh` is set.
- Skips only outputs that depend on unavailable uncached sources, then exits `3`.
- Parses plain domains, hosts-format rows, Unbound `local-zone`/`local-data`
  rows, RPZ `CNAME`/`A`/`AAAA` rows, and exact-domain AGH/Adblock rules.
- Treats exact `@@...` rules as allowlist removals.
- Writes sorted, deduplicated CDB keys with empty values.
- Writes via same-directory temp file, fsync, atomic rename, and parent fsync.
- Refuses to replace an output with an empty CDB.

## Exit Codes

```text
0  success
1  generic/runtime failure or empty-output guard
2  config parse/validation failure
3  fetch failure affecting at least one output
4  output write/atomic replace failure
```

## V1 Limits

- Adblock extraction is exact-domain only; regex, wildcard, path, and
  element-hiding rules are skipped.
- Cache directory is fixed at `/var/cache/cdbgen-rs`.
- YAML and named output selection are not implemented.
