# Telegram Mini App

A WebGL game running as a Telegram Mini App sells for Telegram Stars with `TelegramStoreAdapter`. The billing server
creates an invoice link of the game's bot; this sample opens it with `Telegram.WebApp.openInvoice` (a jslib) and tells
the adapter how Telegram closed it.

1. Load Telegram's script in the WebGL template (`index.html`, before the Unity loader):
   `<script src="https://telegram.org/js/telegram-web-app.js"></script>`.
2. Import the sample (it contains `TelegramWebApp.jslib`, a WebGL plugin, and `TelegramWebAppBridge`).
3. Create the billing service with the adapter:

```csharp
var deps = new BillingDependencies { UserIdProvider = myUserIds };
deps.Store = new TelegramStoreAdapter(config, TelegramWebAppBridge.Create());
var billing = Billing.Create(config, deps);
```

Add a `telegram` build with its app id to `BillingConfig.builds`. On the billing server it is a web build with store
Telegram Stars: the bot token, a webhook secret and the bot's webhook (Setup tab); products are priced in `XTR`
(Stars × 1,000,000). See docs/store-setup.md.

The player id (`UserIdProvider`) is yours: Telegram's `initDataUnsafe.user.id` works if your backend checks `initData`
first; the server grants to the id the checkout named.
