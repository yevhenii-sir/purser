//! Virtual currency wallets: a player's balance of each currency of an app (gems, coins), credited
//! when a product that gives currency is bought, taken back when it is refunded, spent by the game.
//! The server is the record: every change is a wallet transaction with a reference, applied once.

/// Currency ids: 1-32 of `A-Z a-z 0-9 _ -`.
pub fn is_valid_currency(id: &str) -> bool {
    !id.is_empty() && id.len() <= 32 && id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
}

/// References of wallet transactions (a spend's id in the game, an order id): 1-128 of
/// `A-Z a-z 0-9 _ - . :`.
pub fn is_valid_reference(r: &str) -> bool {
    !r.is_empty() && r.len() <= 128 && r.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | ':'))
}

/// The most one transaction moves, and the most a balance holds: far from `i64` overflow.
pub const MAX_AMOUNT: i64 = 1_000_000_000_000;

/// Why a balance changed (`wallet_tx.reason`).
pub mod reason {
    /// A purchase of a product that gives currency was granted.
    pub const PURCHASE: &str = "purchase";
    /// That purchase was refunded or revoked: its currency is taken back.
    pub const REFUND: &str = "refund";
    /// A refunded purchase is valid again.
    pub const REINSTATE: &str = "reinstate";
    /// The game spent currency (an item bought with it).
    pub const SPEND: &str = "spend";
    /// The game's backend gave currency (a reward).
    pub const GRANT: &str = "grant";
    /// An admin changed the balance from the admin page.
    pub const ADMIN: &str = "admin";
}

/// The currency a product gives (`product.wallet`): `gems:100` or `gems:100,coins:5000`, per unit
/// bought. Normalized: sorted by currency, no spaces.
pub fn parse_grants(text: &str) -> Result<Vec<(String, i64)>, String> {
    let mut out: Vec<(String, i64)> = Vec::new();
    for part in text.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let (currency, amount) = part.split_once(':').ok_or_else(|| format!("'{part}': write currency:amount, e.g. gems:100"))?;
        let currency = currency.trim();
        if !is_valid_currency(currency) {
            return Err(format!("'{currency}': currency ids are 1-32 of A-Z a-z 0-9 _ -"));
        }
        let amount: i64 = amount.trim().parse().map_err(|_| format!("'{part}': the amount is a whole number"))?;
        if !(1..=MAX_AMOUNT).contains(&amount) {
            return Err(format!("'{part}': the amount is 1..{MAX_AMOUNT}"));
        }
        if out.iter().any(|(c, _)| c == currency) {
            return Err(format!("'{currency}' is listed twice"));
        }
        out.push((currency.to_string(), amount));
    }
    out.sort();
    Ok(out)
}

/// [`parse_grants`] written back (`gems:100,coins:5000` sorted).
pub fn format_grants(grants: &[(String, i64)]) -> String {
    grants.iter().map(|(c, a)| format!("{c}:{a}")).collect::<Vec<_>>().join(",")
}

/// Insufficient funds for a spend.
pub const ERR_INSUFFICIENT_FUNDS: &str = "ERR_INSUFFICIENT_FUNDS";

/// The balance after `delta`. A spend (`may_go_negative` false) needs the funds; taking back the
/// currency of a refunded purchase may leave a debt (the player spent it already), which later
/// purchases pay off.
pub fn apply(balance: i64, delta: i64, may_go_negative: bool) -> Result<i64, String> {
    if delta == 0 || delta.abs() > MAX_AMOUNT {
        return Err(format!("[ERR_BAD_AMOUNT] the amount is 1..{MAX_AMOUNT}"));
    }
    let after = balance.saturating_add(delta);
    if after < 0 && delta < 0 && !may_go_negative {
        return Err(format!("[{ERR_INSUFFICIENT_FUNDS}] the balance is {balance}"));
    }
    if after > MAX_AMOUNT {
        return Err(format!("[ERR_BAD_AMOUNT] a balance holds at most {MAX_AMOUNT}"));
    }
    Ok(after)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grants_are_parsed_and_normalized() {
        assert_eq!(parse_grants(" gems:100 , coins:5000").unwrap(), vec![("coins".to_string(), 5000), ("gems".to_string(), 100)]);
        assert_eq!(format_grants(&parse_grants("gems:100,coins:5000").unwrap()), "coins:5000,gems:100");
        assert_eq!(parse_grants("").unwrap(), vec![]);
        for bad in ["gems", "gems:x", "gems:0", "gems:-5", "ge ms:1", "gems:1,gems:2", "gems:10000000000000"] {
            assert!(parse_grants(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn spends_need_funds_and_refunds_may_leave_a_debt() {
        assert_eq!(apply(100, -40, false), Ok(60));
        assert!(apply(30, -40, false).unwrap_err().contains(ERR_INSUFFICIENT_FUNDS));
        assert_eq!(apply(30, -40, true), Ok(-10));
        assert_eq!(apply(-10, 100, false), Ok(90));
        assert!(apply(0, 0, false).is_err());
        assert!(apply(MAX_AMOUNT, 1, false).is_err());
        assert!(is_valid_reference("order:42-a.b") && !is_valid_reference("a b"));
    }
}
