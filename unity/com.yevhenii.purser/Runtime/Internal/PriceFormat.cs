using System;
using System.Globalization;
using System.Text;

namespace Purser
{
    /// <summary>
    /// Formats an amount like a price the store already formatted (<see cref="StoreProduct.LocalizedPrice"/>): the same
    /// currency sign and its place, decimal and grouping separators and number of decimals. The store's locale is not
    /// known on the device, so the store's own string is the pattern.
    /// </summary>
    internal static class PriceFormat
    {
        private const string Separators = ".,'    ";

        /// <param name="pattern">A price as the store formats it, e.g. <c>$49.99</c>, <c>49,99 €</c>, <c>¥1,200</c>.</param>
        /// <param name="patternAmount">The amount <paramref name="pattern"/> shows, when known (&gt; 0): it tells a decimal
        /// separator from a grouping one (<c>1,200</c> yen or 1.20).</param>
        /// <param name="amount">What to format.</param>
        /// <param name="currency">ISO 4217 code for the fallback when the pattern has no number.</param>
        public static string Format(string pattern, decimal patternAmount, decimal amount, string currency)
        {
            if (amount < 0)
                amount = 0;
            if (!FindNumber(pattern, out var start, out var length))
                return Plain(amount, currency);
            var number = pattern.Substring(start, length);
            ReadSeparators(number, patternAmount, out var decimalSep, out var decimals, out var groupSep);
            if (decimals == 0 && amount != decimal.Round(amount) && amount < 10)
            {
                // A pattern without decimals ($5, ¥120) cannot show a small fractional amount as anything but 0 or 1.
                decimals = 2;
                decimalSep = groupSep == '.' ? ',' : '.';
            }
            return pattern.Substring(0, start) + Number(amount, decimals, decimalSep, groupSep) + pattern.Substring(start + length);
        }

        private static string Plain(decimal amount, string currency) =>
            amount.ToString("0.00", CultureInfo.InvariantCulture) + (string.IsNullOrEmpty(currency) ? "" : " " + currency);

        /// <summary>The first run of digits with separators between them.</summary>
        private static bool FindNumber(string s, out int start, out int length)
        {
            start = length = 0;
            if (string.IsNullOrEmpty(s))
                return false;
            var first = -1;
            for (var i = 0; i < s.Length; i++)
                if (char.IsDigit(s[i]))
                {
                    first = i;
                    break;
                }
            if (first < 0)
                return false;
            var last = first;
            for (var i = first + 1; i < s.Length; i++)
            {
                if (char.IsDigit(s[i]))
                    last = i;
                else if (Separators.IndexOf(s[i]) < 0)
                    break;
            }
            start = first;
            length = last - first + 1;
            return true;
        }

        private static void ReadSeparators(string number, decimal patternAmount, out char decimalSep, out int decimals, out char groupSep)
        {
            decimalSep = '.';
            decimals = 0;
            groupSep = '\0';
            var lastSep = -1;
            for (var i = number.Length - 1; i >= 0; i--)
                if (!char.IsDigit(number[i]))
                {
                    lastSep = i;
                    break;
                }
            if (lastSep < 0)
                return;
            var sep = number[lastSep];
            var after = number.Length - lastSep - 1;
            var seps = 0;
            foreach (var c in number)
                if (!char.IsDigit(c))
                    seps++;
            var repeated = number.IndexOf(sep) != lastSep;
            // The last separator is the decimal one unless it repeats (1,000,000) or groups three digits of an amount
            // without a fractional part (1,200 yen); the known amount decides 1.200 (3 decimals) against 1200.
            bool isDecimal;
            if (repeated || sep == ' ' || sep == ' ' || sep == ' ' || sep == ' ' || sep == '\'')
                isDecimal = false;
            else if (after != 3)
                isDecimal = true;
            else if (patternAmount > 0)
                isDecimal = Matches(number, sep, patternAmount) && !Matches(number, '\0', patternAmount);
            else
                isDecimal = false;
            if (isDecimal)
            {
                decimalSep = sep;
                decimals = after;
                if (seps > 1)
                    groupSep = number[FirstIndex(number, c => !char.IsDigit(c))];
            }
            else
            {
                groupSep = sep;
                decimalSep = sep == '.' ? ',' : '.';
            }
        }

        /// <summary>The digits of <paramref name="number"/> read with <paramref name="decimalSep"/> as the decimal
        /// separator ('\0': none) equal <paramref name="expected"/>.</summary>
        private static bool Matches(string number, char decimalSep, decimal expected)
        {
            var digits = new StringBuilder();
            foreach (var c in number)
            {
                if (char.IsDigit(c))
                    digits.Append(c);
                else if (c == decimalSep)
                    digits.Append('.');
            }
            return decimal.TryParse(digits.ToString(), NumberStyles.AllowDecimalPoint, CultureInfo.InvariantCulture, out var v)
                && Math.Abs(v - expected) < 0.005m;
        }

        private static string Number(decimal amount, int decimals, char decimalSep, char groupSep)
        {
            var rounded = decimal.Round(amount, decimals, MidpointRounding.AwayFromZero);
            var text = rounded.ToString("F" + decimals, CultureInfo.InvariantCulture);
            var point = text.IndexOf('.');
            var whole = point < 0 ? text : text.Substring(0, point);
            var fraction = point < 0 ? "" : text.Substring(point + 1);
            var sb = new StringBuilder();
            for (var i = 0; i < whole.Length; i++)
            {
                if (groupSep != '\0' && i > 0 && (whole.Length - i) % 3 == 0)
                    sb.Append(groupSep);
                sb.Append(whole[i]);
            }
            if (decimals > 0)
                sb.Append(decimalSep).Append(fraction);
            return sb.ToString();
        }

        private static int FirstIndex(string s, Func<char, bool> match)
        {
            for (var i = 0; i < s.Length; i++)
                if (match(s[i]))
                    return i;
            return -1;
        }
    }
}
