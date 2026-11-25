//! Promo codes: a code of an app gives a product of its catalog without a payment (`POST /redeem`).
//! Each player redeems a code once; a code may be limited to a number of players and to a date.

/// Shortest and longest code.
pub const MIN_LEN: usize = 4;
pub const MAX_LEN: usize = 32;

/// The code as stored and compared: trimmed, upper case, `A-Z 0-9 -` only, [`MIN_LEN`]..[`MAX_LEN`]
/// characters. `None`: not a code.
pub fn normalize(code: &str) -> Option<String> {
    let c = code.trim().to_ascii_uppercase();
    let ok = (MIN_LEN..=MAX_LEN).contains(&c.len()) && c.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '-');
    ok.then_some(c)
}

/// What a code allows now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rules {
    pub enabled: bool,
    /// 0: never expires.
    pub expires_at_ms: i64,
    /// Players who may redeem it; 0: no limit.
    pub max_redemptions: u32,
    pub redeemed: u32,
}

/// Why a code cannot be redeemed: `(error code, message for the player)`.
pub type Refusal = (&'static str, &'static str);

pub const ERR_INVALID: &str = "ERR_CODE_INVALID";
pub const ERR_EXPIRED: &str = "ERR_CODE_EXPIRED";
pub const ERR_USED_UP: &str = "ERR_CODE_USED_UP";
pub const ERR_ALREADY: &str = "ERR_CODE_ALREADY_REDEEMED";

/// Whether a player may redeem a code now (`None`: yes). An unknown and a disabled code look the
/// same to the player: a disabled code does not confirm that it exists.
pub fn refusal(rules: Option<&Rules>, now_ms: i64, redeemed_by_player: bool) -> Option<Refusal> {
    let Some(r) = rules.filter(|r| r.enabled) else {
        return Some((ERR_INVALID, "this code does not exist"));
    };
    if redeemed_by_player {
        return Some((ERR_ALREADY, "this code was already redeemed by this player"));
    }
    if r.expires_at_ms > 0 && now_ms >= r.expires_at_ms {
        return Some((ERR_EXPIRED, "this code has expired"));
    }
    if r.max_redemptions > 0 && r.redeemed >= r.max_redemptions {
        return Some((ERR_USED_UP, "this code has been used up"));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_normalized() {
        assert_eq!(normalize("  summer-2026 ").as_deref(), Some("SUMMER-2026"));
        assert_eq!(normalize("abc"), None, "too short");
        assert_eq!(normalize(&"A".repeat(33)), None, "too long");
        assert_eq!(normalize("NO SPACES"), None);
        assert_eq!(normalize("ÜBER1"), None);
    }

    #[test]
    fn a_code_is_refused_when_it_cannot_be_redeemed() {
        let ok = Rules { enabled: true, expires_at_ms: 0, max_redemptions: 0, redeemed: 0 };
        assert_eq!(refusal(Some(&ok), 10, false), None);
        assert_eq!(refusal(None, 10, false).unwrap().0, ERR_INVALID);
        assert_eq!(refusal(Some(&Rules { enabled: false, ..ok }), 10, false).unwrap().0, ERR_INVALID);
        assert_eq!(refusal(Some(&ok), 10, true).unwrap().0, ERR_ALREADY);
        assert_eq!(refusal(Some(&Rules { expires_at_ms: 10, ..ok }), 10, false).unwrap().0, ERR_EXPIRED);
        assert_eq!(refusal(Some(&Rules { expires_at_ms: 11, ..ok }), 10, false), None);
        assert_eq!(refusal(Some(&Rules { max_redemptions: 2, redeemed: 2, ..ok }), 10, false).unwrap().0, ERR_USED_UP);
        assert_eq!(refusal(Some(&Rules { max_redemptions: 2, redeemed: 1, ..ok }), 10, false), None);
    }
}
