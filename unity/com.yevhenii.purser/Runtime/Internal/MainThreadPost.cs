using System;
using System.Threading;

namespace Purser
{
    /// <summary>Runs a callback that touches Unity objects on the thread that owns them (cancellation callbacks run
    /// on whatever thread cancels the token).</summary>
    internal static class MainThreadPost
    {
        /// <summary>
        /// Runs <paramref name="action"/> right away when the caller is on the thread of <paramref name="context"/>
        /// (or there is no context to post to), otherwise posts it there.
        /// </summary>
        /// <param name="context">The <see cref="SynchronizationContext"/> captured on the thread that owns the objects
        /// (Unity's main thread); may be null.</param>
        public static void Run(SynchronizationContext context, Action action)
        {
            if (context == null || SynchronizationContext.Current == context)
                action();
            else
                context.Post(_ => action(), null);
        }
    }
}
