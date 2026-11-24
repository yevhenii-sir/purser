package com.yevhenii.purser;

import android.app.Activity;
import android.content.Context;
import android.content.Intent;
import android.net.Uri;

/**
 * Opens a payment page (Xsolla Pay Station) for the Purser Unity package: in Chrome Custom Tabs when the app has
 * androidx.browser (Editor/PurserDependencies.xml asks the External Dependency Manager for it), otherwise in the
 * default browser. The page sends the player back to the game with an app link (the Xsolla return URL).
 * androidx.browser is called through reflection so the plugin compiles without it.
 */
public final class PurserBrowser {
    private PurserBrowser() {
    }

    public static void open(final Activity activity, final String url) {
        if (activity == null || url == null) {
            return;
        }
        activity.runOnUiThread(new Runnable() {
            @Override
            public void run() {
                Uri uri = Uri.parse(url);
                if (!openCustomTab(activity, uri)) {
                    Intent view = new Intent(Intent.ACTION_VIEW, uri);
                    view.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK);
                    activity.startActivity(view);
                }
            }
        });
    }

    private static boolean openCustomTab(Activity activity, Uri uri) {
        try {
            Class<?> builderClass = Class.forName("androidx.browser.customtabs.CustomTabsIntent$Builder");
            Object builder = builderClass.getConstructor().newInstance();
            Object customTabs = builderClass.getMethod("build").invoke(builder);
            customTabs.getClass().getMethod("launchUrl", Context.class, Uri.class).invoke(customTabs, activity, uri);
            return true;
        } catch (Throwable e) {
            return false;
        }
    }
}
