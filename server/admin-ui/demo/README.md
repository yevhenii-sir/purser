# Admin panel demo

A read-only build of the admin page with made-up data: two apps (Galaxy Miner on Google Play, App Store and Amazon; Word Garden on Google Play, App Store and Stripe), a year of purchases with subscriptions, trials and refunds, integrations and a paywall A/B test. No server and no sign-in: reads are answered from `snapshot.json`, every change is refused ("read-only demo"), and the clock is set back to when the snapshot was taken.

- **Build:** `npm run build:demo` in `server/admin-ui` writes `dist-demo/index.html` (one file; it also works opened from disk). `npm run dev:demo` serves it with hot reload.
- **Hosting:** the Docs workflow publishes it at `<docs site>/demo/` together with the documentation.
- **Data:** `snapshot.json` is made by `tools/demo_snapshot.py`, which publishes the module to a local SpacetimeDB, fills it through the admin API and saves the tables (secret names only, never values) and the answers of the admin procedures. Run it again when the admin procedures or tables change:

```bash
spacetime start --in-memory -l 127.0.0.1:3013
cd server && cargo build -p purser --target wasm32-unknown-unknown --release --locked && cd ..
python tools/demo_snapshot.py server/target/wasm32-unknown-unknown/release/purser.wasm
```

The demo code lives in `src/lib/demo.ts`; `__DEMO__` is a build constant, so none of it (nor the snapshot) reaches the page served by the module.
