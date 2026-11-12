using System;
using UnityEngine;

namespace Purser
{
    /// <summary>Logging with a level from the config; the sink can be replaced (tests, custom loggers).</summary>
    public sealed class BillingLog
    {
        private const string Prefix = "[Purser] ";

        /// <summary>Messages above this level are dropped; <see cref="BillingLogLevel.None"/> drops all.</summary>
        public BillingLogLevel Level { get; set; }

        private readonly Action<BillingLogLevel, string> m_Sink;

        /// <param name="sink">Receives the messages that pass <paramref name="level"/>; default: <c>Debug.Log*</c>
        /// with a <c>[Purser]</c> prefix.</param>
        public BillingLog(BillingLogLevel level, Action<BillingLogLevel, string> sink = null)
        {
            Level = level;
            m_Sink = sink ?? DefaultSink;
        }

        /// <summary>Something failed and needs attention.</summary>
        public void Error(string message) => Write(BillingLogLevel.Error, message);

        /// <summary>Something went wrong but was handled (retried, cached copy used, skipped).</summary>
        public void Warning(string message) => Write(BillingLogLevel.Warning, message);

        /// <summary>Normal progress: initialization, purchase results.</summary>
        public void Info(string message) => Write(BillingLogLevel.Info, message);

        /// <summary>Details for debugging.</summary>
        public void Verbose(string message) => Write(BillingLogLevel.Verbose, message);

        private void Write(BillingLogLevel level, string message)
        {
            if (level <= Level && Level != BillingLogLevel.None)
                m_Sink(level, message);
        }

        private static void DefaultSink(BillingLogLevel level, string message)
        {
            switch (level)
            {
                case BillingLogLevel.Error:
                    Debug.LogError(Prefix + message);
                    break;
                case BillingLogLevel.Warning:
                    Debug.LogWarning(Prefix + message);
                    break;
                default:
                    Debug.Log(Prefix + message);
                    break;
            }
        }
    }
}
