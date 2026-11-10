# Images

Screenshots for the README and the docs site, taken from the [admin panel demo](../../server/admin-ui/demo/README.md) (made-up data), 1600 px wide, light and dark (`*-dark.png`).

| File | Page |
|---|---|
| `admin-revenue.png` | **Revenue** with the revenue tree (`#revenue`) |
| `admin-setup.png` | An app's **Setup** tab with a green setup check (`#app/galaxy-miner/setup`) |
| `admin-integrations.png` | An app's **Integrations** tab (`#app/galaxy-miner/integrations`) |
| `admin-player.png` | A **player**: spending, entitlements, purchases (`#player/<user_id>`) |

To retake them, build the demo and serve it, then for each page and each scheme (`1` light, `0` dark):

```bash
cd server/admin-ui && npm run build:demo && python -m http.server 5190 -d dist-demo
chrome --headless=new --hide-scrollbars --lang=en-US --force-device-scale-factor=1   --blink-settings=preferredColorScheme=1 --window-size=1600,1000 --virtual-time-budget=8000   --screenshot=docs/images/admin-setup.png "http://127.0.0.1:5190/#app/galaxy-miner/setup"
```

Keep each file under 500 KB and use demo data only: no real app ids, players, tokens, order ids or revenue.
