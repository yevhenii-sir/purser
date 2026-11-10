// Apple Ads attribution token for AppleAdsAttributionProvider (C#).
// AdServices is loaded at run time, so the app needs no framework setting and still runs below iOS 14.3.
#import <Foundation/Foundation.h>
#include <dlfcn.h>
#include <objc/message.h>
#include <string.h>

extern "C" char *PurserAdServicesToken(void)
{
    @autoreleasepool {
        dlopen("/System/Library/Frameworks/AdServices.framework/AdServices", RTLD_LAZY);
        Class cls = NSClassFromString(@"AAAttribution");
        SEL sel = NSSelectorFromString(@"attributionTokenWithError:");
        if (cls == nil || ![cls respondsToSelector:sel])
            return NULL;
        NSError *error = nil;
        NSString *(*call)(id, SEL, NSError **) = (NSString * (*)(id, SEL, NSError **))objc_msgSend;
        NSString *token = call(cls, sel, &error);
        if (token == nil || error != nil)
            return NULL;
        // IL2CPP frees returned strings with free().
        return strdup(token.UTF8String);
    }
}
