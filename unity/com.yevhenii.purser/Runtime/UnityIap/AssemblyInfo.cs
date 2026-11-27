using System.Runtime.CompilerServices;

[assembly: InternalsVisibleTo("Purser.Tests.UnityIap")]
// Nothing references this assembly: without this the linker of a stripped IL2CPP build skips it, and with it the
// [RuntimeInitializeOnLoadMethod] that makes Unity IAP the default store (UnityIapRegistration).
[assembly: UnityEngine.Scripting.AlwaysLinkAssembly]
