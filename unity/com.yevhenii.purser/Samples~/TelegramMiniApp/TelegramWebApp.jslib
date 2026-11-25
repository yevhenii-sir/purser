mergeInto(LibraryManager.library, {
  // Opens a Telegram Stars invoice and answers OnInvoiceClosed("<url>\n<status>") of the bridge's GameObject.
  PurserTelegramOpenInvoice: function (urlPtr, objectPtr) {
    var url = UTF8ToString(urlPtr);
    var target = UTF8ToString(objectPtr);
    var app = window.Telegram && window.Telegram.WebApp;
    if (!app || typeof app.openInvoice !== "function") {
      SendMessage(target, "OnInvoiceClosed", url + "\n" + "failed");
      return;
    }
    app.openInvoice(url, function (status) {
      SendMessage(target, "OnInvoiceClosed", url + "\n" + status);
    });
  },
});
