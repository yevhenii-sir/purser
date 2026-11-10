using System;
using UnityEngine;

namespace Purser
{
    /// <summary>Logging with a level from the config; the sink can be replaced (tests, custom loggers).</summary>
    public sealed class BillingLog
    {
        private const string Prefix = "[Purser] ";

        public BillingLogLevel Level { get; set; }

        private readonly Action<BillingLogLevel, string> m_Sink;

        public BillingLog(BillingLogLevel level, Action<BillingLogLevel, string> sink = null)
        {
            Level = level;
            m_Sink = sink ?? DefaultSink;
        }

        public void Error(string message) => Write(BillingLogLevel.Error, message);
        public void Warning(string message) => Write(BillingLogLevel.Warning, message);
        public void Info(string message) => Write(BillingLogLevel.Info, message);
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
