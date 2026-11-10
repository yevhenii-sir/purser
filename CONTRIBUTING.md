# Contributing

Issues and pull requests are welcome. Please keep changes focused and covered by tests. [AGENTS.md](AGENTS.md) is a compact map of the code, the domain and the SpacetimeDB pitfalls (written for AI agents, useful for people too).

## Layout

- `server/core` — pure billing logic without SpacetimeDB: store APIs, JWT, the setup check rules, revenue and FX. Unit tests run natively.
- `server/spacetimedb` — the SpacetimeDB module: tables, admin reducers and procedures, HTTP routes, ledger, worker. It embeds the built admin page `admin/index.html`.
- `server/admin-ui` — the admin page sources: React, TypeScript, Tailwind and [shadcn/ui](https://ui.shadcn.com), built into that single `index.html`.
- `unity/com.yevhenii.purser` — the Unity package (EditMode tests in `Tests/Editor`).

New logic goes into `core` where possible, as pure functions with tests; the module only does storage and HTTP.

## Tests

```bash
cd server
cargo test -p purser-core
cargo test -p purser      # module tests on spacetimedb::test_utils, mocked HTTP
cargo build -p purser --target wasm32-unknown-unknown --release
```

The module tests build `spacetimedb-core`, which compiles OpenSSL from source. On Windows that needs a full Perl (the one from Git Bash won't do), e.g. [Strawberry Perl](https://strawberryperl.com/):

```bash
OPENSSL_SRC_PERL=C:/Strawberry/perl/bin/perl.exe cargo test -p purser
```

## Admin page

```bash
cd server/admin-ui
npm ci
npm run dev        # http://localhost:5173/v1/database/<db>/route/admin against a local `spacetime start`
npm run build      # writes ../spacetimedb/admin/index.html
```

`npm run dev` proxies the API to `http://127.0.0.1:3000` (set `SPACETIME_URL` for another server). The built `index.html` is committed, so the Rust module builds without Node; CI fails if it does not match the sources, so run `npm run build` and commit it with your change. Add shadcn components with `npx shadcn@latest add <component>`.

The page talks only to its own database (`/sql`, `/call`, `/logs`, the billing routes) under a strict CSP: no external scripts, styles, fonts or requests.

Unity: add the package to `"testables"` in a project's `manifest.json` and run the Test Runner (EditMode). After adding files to the package, run `python tools/generate_unity_meta.py` and commit the `.meta` files.

## Dependencies

`Cargo.lock` follows SpacetimeDB's own lockfile. Don't run a full `cargo update`: newer `icu_*` crates break `temporal_rs` in `spacetimedb-core`. Upgrade SpacetimeDB with `cargo update -p spacetimedb --precise <version>` and bump the version in `spacetimedb/Cargo.toml`.

## Schema changes

Published databases are upgraded in place, so:

- new tables are fine;
- a new column goes at the end of its table with `#[default(...)]`;
- never rename, reorder or retype existing columns;
- one-time data migrations run from the worker (`process_pending`), since `init` does not run on republish.

## Releases

Bump the version in `server/core/Cargo.toml`, `server/spacetimedb/Cargo.toml` and `unity/.../package.json` (plus `CHANGELOG.md`), then push a `vX.Y.Z` tag: the release workflow builds `purser.wasm` and attaches it to the GitHub release.
