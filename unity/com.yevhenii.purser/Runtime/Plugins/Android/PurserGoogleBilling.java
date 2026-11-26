package com.yevhenii.purser;

import android.app.Activity;
import android.os.Handler;
import android.os.Looper;

import com.android.billingclient.api.BillingClient;
import com.android.billingclient.api.BillingClientStateListener;
import com.android.billingclient.api.BillingFlowParams;
import com.android.billingclient.api.BillingResult;
import com.android.billingclient.api.PendingPurchasesParams;
import com.android.billingclient.api.ProductDetails;
import com.android.billingclient.api.ProductDetailsResponseListener;
import com.android.billingclient.api.Purchase;
import com.android.billingclient.api.PurchasesResponseListener;
import com.android.billingclient.api.PurchasesUpdatedListener;
import com.android.billingclient.api.QueryProductDetailsParams;
import com.android.billingclient.api.QueryProductDetailsResult;
import com.android.billingclient.api.QueryPurchasesParams;

import org.json.JSONArray;
import org.json.JSONObject;

import java.util.Collections;
import java.util.List;

/**
 * Google Play subscription offers for the Purser Unity package: lists the offers of a subscription and buys one by its
 * offer token, which Unity IAP 5 cannot do. Built against Play Billing Library 8 / 9 (the library Unity IAP 5.4+ adds
 * to the Gradle project; the package's editor code leaves this file out of builds without Unity IAP 5.4+).
 *
 * Every call connects a BillingClient of its own and ends it when the call is done, so Unity IAP's client is left
 * alone. The plugin never acknowledges or consumes: the purchase goes to the Purser server (/verify/google), which
 * acknowledges it. Results go back to C# as JSON through {@link Callback#onResult}, on the main (UI) thread:
 * {"code": BillingResponseCode or -100 for a plugin error, "message": "...", "offers": [...] | "purchases": [...]}.
 */
public final class PurserGoogleBilling {
    /** Implemented in C# (AndroidJavaProxy). */
    public interface Callback {
        void onResult(String json);
    }

    /** A failure outside the Billing Library (not a BillingResponseCode). */
    private static final int PLUGIN_ERROR = -100;

    private static final Handler MAIN = new Handler(Looper.getMainLooper());

    private PurserGoogleBilling() {
    }

    /** Lists the offers of a subscription the player can buy (base plans and the offers they are eligible for). */
    public static void queryOffers(final Activity activity, final String productId, final Callback callback) {
        MAIN.post(new Runnable() {
            @Override
            public void run() {
                new OffersQuery(productId, callback).start(activity);
            }
        });
    }

    /**
     * Buys a subscription with the offer of offerToken. oldProductId (empty: none) is a subscription of the player it
     * replaces, with replacementMode (BillingFlowParams.SubscriptionUpdateParams.ReplacementMode). obfuscatedAccountId
     * (empty: none) binds the purchase to the player.
     */
    public static void purchase(final Activity activity, final String productId, final String offerToken,
                                final String oldProductId, final int replacementMode, final String obfuscatedAccountId,
                                final Callback callback) {
        MAIN.post(new Runnable() {
            @Override
            public void run() {
                new OfferPurchase(activity, productId, offerToken, oldProductId, replacementMode, obfuscatedAccountId,
                        callback).start(activity);
            }
        });
    }

    /** One call: its own client, connected until the result is sent. State is touched on the main thread only. */
    private abstract static class Operation implements PurchasesUpdatedListener, BillingClientStateListener {
        private final Callback callback;
        BillingClient client;
        boolean done;

        Operation(Callback callback) {
            this.callback = callback;
        }

        void start(Activity activity) {
            try {
                client = BillingClient.newBuilder(activity.getApplicationContext())
                        .setListener(this)
                        .enablePendingPurchases(PendingPurchasesParams.newBuilder().enableOneTimeProducts().build())
                        .build();
                client.startConnection(this);
            } catch (Throwable e) {
                fail(PLUGIN_ERROR, e.toString());
            }
        }

        /** The client is connected. */
        abstract void connected();

        /** A purchase result arrived (this client launched a flow, or Google Play reports a purchase made elsewhere). */
        void purchasesUpdated(BillingResult result, List<Purchase> purchases) {
        }

        @Override
        public void onBillingSetupFinished(final BillingResult result) {
            onMain(new Runnable() {
                @Override
                public void run() {
                    if (result.getResponseCode() != BillingClient.BillingResponseCode.OK) {
                        fail(result);
                    } else {
                        connected();
                    }
                }
            });
        }

        @Override
        public void onBillingServiceDisconnected() {
            onMain(new Runnable() {
                @Override
                public void run() {
                    fail(BillingClient.BillingResponseCode.SERVICE_DISCONNECTED, "Google Play billing service disconnected");
                }
            });
        }

        @Override
        public void onPurchasesUpdated(final BillingResult result, final List<Purchase> purchases) {
            onMain(new Runnable() {
                @Override
                public void run() {
                    purchasesUpdated(result, purchases);
                }
            });
        }

        /** Runs a step on the main thread unless the call is done; a step that throws ends the call. */
        void onMain(final Runnable step) {
            MAIN.post(new Runnable() {
                @Override
                public void run() {
                    if (done) {
                        return;
                    }
                    try {
                        step.run();
                    } catch (Throwable e) {
                        fail(PLUGIN_ERROR, e.toString());
                    }
                }
            });
        }

        void fail(BillingResult result) {
            fail(result.getResponseCode(), result.getDebugMessage());
        }

        void fail(int code, String message) {
            JSONObject json = new JSONObject();
            try {
                json.put("code", code);
                json.put("message", message == null ? "" : message);
            } catch (Throwable ignored) {
                // put() throws only for a null key or a non-finite number.
            }
            finish(json);
        }

        /** Sends the result once and ends the connection. */
        void finish(JSONObject json) {
            if (done) {
                return;
            }
            done = true;
            if (client != null) {
                try {
                    client.endConnection();
                } catch (Throwable ignored) {
                    // Already disconnected.
                }
            }
            try {
                callback.onResult(json.toString());
            } catch (Throwable ignored) {
                // An exception of the C# side; it logs its own.
            }
        }

        void queryDetails(String productId, ProductDetailsResponseListener listener) {
            QueryProductDetailsParams.Product product = QueryProductDetailsParams.Product.newBuilder()
                    .setProductId(productId)
                    .setProductType(BillingClient.ProductType.SUBS)
                    .build();
            client.queryProductDetailsAsync(QueryProductDetailsParams.newBuilder()
                    .setProductList(Collections.singletonList(product))
                    .build(), listener);
        }

        static ProductDetails find(QueryProductDetailsResult result, String productId) {
            List<ProductDetails> list = result == null ? null : result.getProductDetailsList();
            if (list == null) {
                return null;
            }
            for (ProductDetails details : list) {
                if (productId.equals(details.getProductId())) {
                    return details;
                }
            }
            return null;
        }
    }

    private static final class OffersQuery extends Operation {
        private final String productId;

        OffersQuery(String productId, Callback callback) {
            super(callback);
            this.productId = productId;
        }

        @Override
        void connected() {
            queryDetails(productId, new ProductDetailsResponseListener() {
                @Override
                public void onProductDetailsResponse(final BillingResult result, final QueryProductDetailsResult details) {
                    onMain(new Runnable() {
                        @Override
                        public void run() {
                            answer(result, details);
                        }
                    });
                }
            });
        }

        private void answer(BillingResult result, QueryProductDetailsResult details) {
            if (result.getResponseCode() != BillingClient.BillingResponseCode.OK) {
                fail(result);
                return;
            }
            ProductDetails product = find(details, productId);
            if (product == null) {
                fail(BillingClient.BillingResponseCode.ITEM_UNAVAILABLE, "subscription " + productId + " not found in Google Play");
                return;
            }
            JSONArray offers = new JSONArray();
            List<ProductDetails.SubscriptionOfferDetails> list = product.getSubscriptionOfferDetails();
            if (list != null) {
                for (ProductDetails.SubscriptionOfferDetails offer : list) {
                    offers.put(offerJson(offer));
                }
            }
            JSONObject json = new JSONObject();
            try {
                json.put("code", BillingClient.BillingResponseCode.OK);
                json.put("offers", offers);
            } catch (Throwable ignored) {
                // put() throws only for a null key or a non-finite number.
            }
            finish(json);
        }

        private static JSONObject offerJson(ProductDetails.SubscriptionOfferDetails offer) {
            JSONObject json = new JSONObject();
            try {
                json.put("basePlanId", offer.getBasePlanId());
                json.put("offerId", offer.getOfferId());
                json.put("offerToken", offer.getOfferToken());
                List<String> tags = offer.getOfferTags();
                json.put("tags", new JSONArray(tags == null ? Collections.<String>emptyList() : tags));
                JSONArray phases = new JSONArray();
                for (ProductDetails.PricingPhase phase : offer.getPricingPhases().getPricingPhaseList()) {
                    JSONObject p = new JSONObject();
                    p.put("priceMicros", phase.getPriceAmountMicros());
                    p.put("formattedPrice", phase.getFormattedPrice());
                    p.put("currency", phase.getPriceCurrencyCode());
                    p.put("period", phase.getBillingPeriod());
                    p.put("cycles", phase.getBillingCycleCount());
                    p.put("recurrenceMode", phase.getRecurrenceMode());
                    phases.put(p);
                }
                json.put("phases", phases);
            } catch (Throwable ignored) {
                // put() throws only for a null key or a non-finite number.
            }
            return json;
        }
    }

    private static final class OfferPurchase extends Operation {
        private final Activity activity;
        private final String productId;
        private final String offerToken;
        private final String oldProductId;
        private final int replacementMode;
        private final String accountId;
        private boolean launched;

        OfferPurchase(Activity activity, String productId, String offerToken, String oldProductId, int replacementMode,
                      String accountId, Callback callback) {
            super(callback);
            this.activity = activity;
            this.productId = productId;
            this.offerToken = offerToken;
            this.oldProductId = oldProductId;
            this.replacementMode = replacementMode;
            this.accountId = accountId;
        }

        @Override
        void connected() {
            queryDetails(productId, new ProductDetailsResponseListener() {
                @Override
                public void onProductDetailsResponse(final BillingResult result, final QueryProductDetailsResult details) {
                    onMain(new Runnable() {
                        @Override
                        public void run() {
                            detailsReceived(result, details);
                        }
                    });
                }
            });
        }

        private void detailsReceived(BillingResult result, QueryProductDetailsResult details) {
            if (result.getResponseCode() != BillingClient.BillingResponseCode.OK) {
                fail(result);
                return;
            }
            final ProductDetails product = find(details, productId);
            if (product == null) {
                fail(BillingClient.BillingResponseCode.ITEM_UNAVAILABLE, "subscription " + productId + " not found in Google Play");
                return;
            }
            if (!hasOffer(product)) {
                // Google Play lists only the offers the player is eligible for: never buy another one instead.
                fail(BillingClient.BillingResponseCode.ITEM_UNAVAILABLE, "the offer is not available to this player");
                return;
            }
            if (isEmpty(oldProductId)) {
                launch(product, null);
                return;
            }
            client.queryPurchasesAsync(QueryPurchasesParams.newBuilder()
                    .setProductType(BillingClient.ProductType.SUBS)
                    .build(), new PurchasesResponseListener() {
                @Override
                public void onQueryPurchasesResponse(final BillingResult owned, final List<Purchase> purchases) {
                    onMain(new Runnable() {
                        @Override
                        public void run() {
                            if (owned.getResponseCode() != BillingClient.BillingResponseCode.OK) {
                                fail(owned);
                                return;
                            }
                            String oldToken = ownedToken(purchases, oldProductId);
                            if (oldToken == null) {
                                fail(BillingClient.BillingResponseCode.ITEM_NOT_OWNED,
                                        "the subscription " + oldProductId + " to replace is not owned on this Google account");
                                return;
                            }
                            launch(product, oldToken);
                        }
                    });
                }
            });
        }

        private boolean hasOffer(ProductDetails product) {
            List<ProductDetails.SubscriptionOfferDetails> offers = product.getSubscriptionOfferDetails();
            if (offers == null) {
                return false;
            }
            for (ProductDetails.SubscriptionOfferDetails offer : offers) {
                if (offerToken.equals(offer.getOfferToken())) {
                    return true;
                }
            }
            return false;
        }

        private static String ownedToken(List<Purchase> purchases, String productId) {
            if (purchases == null) {
                return null;
            }
            for (Purchase purchase : purchases) {
                if (purchase.getPurchaseState() == Purchase.PurchaseState.PURCHASED && purchase.getProducts().contains(productId)) {
                    return purchase.getPurchaseToken();
                }
            }
            return null;
        }

        private void launch(ProductDetails product, String oldToken) {
            BillingFlowParams.ProductDetailsParams item = BillingFlowParams.ProductDetailsParams.newBuilder()
                    .setProductDetails(product)
                    .setOfferToken(offerToken)
                    .build();
            BillingFlowParams.Builder flow = BillingFlowParams.newBuilder()
                    .setProductDetailsParamsList(Collections.singletonList(item));
            if (!isEmpty(accountId)) {
                flow.setObfuscatedAccountId(accountId);
            }
            if (oldToken != null) {
                flow.setSubscriptionUpdateParams(BillingFlowParams.SubscriptionUpdateParams.newBuilder()
                        .setOldPurchaseToken(oldToken)
                        .setSubscriptionReplacementMode(replacementMode)
                        .build());
            }
            launched = true;
            BillingResult result = client.launchBillingFlow(activity, flow.build());
            if (result.getResponseCode() != BillingClient.BillingResponseCode.OK) {
                fail(result);
            }
        }

        @Override
        void purchasesUpdated(BillingResult result, List<Purchase> purchases) {
            if (!launched) {
                // A purchase this call did not start (made in the Play Store): Unity IAP reports it.
                return;
            }
            if (result.getResponseCode() != BillingClient.BillingResponseCode.OK) {
                fail(result);
                return;
            }
            JSONArray list = new JSONArray();
            boolean related = purchases == null || purchases.isEmpty();
            if (purchases != null) {
                for (Purchase purchase : purchases) {
                    if (purchase.getProducts().contains(productId)) {
                        list.put(purchaseJson(purchase));
                    }
                    related |= purchase.getProducts().contains(productId) ||
                            (!isEmpty(oldProductId) && purchase.getProducts().contains(oldProductId));
                }
            }
            if (!related) {
                // Only other products (a purchase made in the Play Store meanwhile): Unity IAP reports it; keep waiting.
                return;
            }
            // An empty list with OK: a deferred replacement (nothing is bought now; Google Play reports the old
            // subscription or nothing); the C# side decides.
            JSONObject json = new JSONObject();
            try {
                json.put("code", BillingClient.BillingResponseCode.OK);
                json.put("purchases", list);
            } catch (Throwable ignored) {
                // put() throws only for a null key or a non-finite number.
            }
            finish(json);
        }

        private static JSONObject purchaseJson(Purchase purchase) {
            JSONObject json = new JSONObject();
            try {
                json.put("purchaseToken", purchase.getPurchaseToken());
                json.put("orderId", purchase.getOrderId());
                json.put("products", new JSONArray(purchase.getProducts()));
                json.put("state", purchase.getPurchaseState());
                json.put("acknowledged", purchase.isAcknowledged());
            } catch (Throwable ignored) {
                // put() throws only for a null key or a non-finite number.
            }
            return json;
        }

        private static boolean isEmpty(String s) {
            return s == null || s.length() == 0;
        }
    }
}
