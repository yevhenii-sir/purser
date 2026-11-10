//! Country codes. Google reports the billing country as ISO 3166-1 alpha-2 (`regionCode`), Apple as
//! the alpha-3 code of the App Store storefront (`storefront`). The ledger keeps alpha-2.

/// ISO 3166-1 alpha-3 → alpha-2, sorted by alpha-3 (binary search).
const ALPHA3_TO_ALPHA2: &[(&str, &str)] = &[
    ("ABW", "AW"), ("AFG", "AF"), ("AGO", "AO"), ("AIA", "AI"), ("ALA", "AX"), ("ALB", "AL"), ("AND", "AD"),
    ("ARE", "AE"), ("ARG", "AR"), ("ARM", "AM"), ("ASM", "AS"), ("ATA", "AQ"), ("ATF", "TF"), ("ATG", "AG"),
    ("AUS", "AU"), ("AUT", "AT"), ("AZE", "AZ"), ("BDI", "BI"), ("BEL", "BE"), ("BEN", "BJ"), ("BES", "BQ"),
    ("BFA", "BF"), ("BGD", "BD"), ("BGR", "BG"), ("BHR", "BH"), ("BHS", "BS"), ("BIH", "BA"), ("BLM", "BL"),
    ("BLR", "BY"), ("BLZ", "BZ"), ("BMU", "BM"), ("BOL", "BO"), ("BRA", "BR"), ("BRB", "BB"), ("BRN", "BN"),
    ("BTN", "BT"), ("BVT", "BV"), ("BWA", "BW"), ("CAF", "CF"), ("CAN", "CA"), ("CCK", "CC"), ("CHE", "CH"),
    ("CHL", "CL"), ("CHN", "CN"), ("CIV", "CI"), ("CMR", "CM"), ("COD", "CD"), ("COG", "CG"), ("COK", "CK"),
    ("COL", "CO"), ("COM", "KM"), ("CPV", "CV"), ("CRI", "CR"), ("CUB", "CU"), ("CUW", "CW"), ("CXR", "CX"),
    ("CYM", "KY"), ("CYP", "CY"), ("CZE", "CZ"), ("DEU", "DE"), ("DJI", "DJ"), ("DMA", "DM"), ("DNK", "DK"),
    ("DOM", "DO"), ("DZA", "DZ"), ("ECU", "EC"), ("EGY", "EG"), ("ERI", "ER"), ("ESH", "EH"), ("ESP", "ES"),
    ("EST", "EE"), ("ETH", "ET"), ("FIN", "FI"), ("FJI", "FJ"), ("FLK", "FK"), ("FRA", "FR"), ("FRO", "FO"),
    ("FSM", "FM"), ("GAB", "GA"), ("GBR", "GB"), ("GEO", "GE"), ("GGY", "GG"), ("GHA", "GH"), ("GIB", "GI"),
    ("GIN", "GN"), ("GLP", "GP"), ("GMB", "GM"), ("GNB", "GW"), ("GNQ", "GQ"), ("GRC", "GR"), ("GRD", "GD"),
    ("GRL", "GL"), ("GTM", "GT"), ("GUF", "GF"), ("GUM", "GU"), ("GUY", "GY"), ("HKG", "HK"), ("HMD", "HM"),
    ("HND", "HN"), ("HRV", "HR"), ("HTI", "HT"), ("HUN", "HU"), ("IDN", "ID"), ("IMN", "IM"), ("IND", "IN"),
    ("IOT", "IO"), ("IRL", "IE"), ("IRN", "IR"), ("IRQ", "IQ"), ("ISL", "IS"), ("ISR", "IL"), ("ITA", "IT"),
    ("JAM", "JM"), ("JEY", "JE"), ("JOR", "JO"), ("JPN", "JP"), ("KAZ", "KZ"), ("KEN", "KE"), ("KGZ", "KG"),
    ("KHM", "KH"), ("KIR", "KI"), ("KNA", "KN"), ("KOR", "KR"), ("KWT", "KW"), ("LAO", "LA"), ("LBN", "LB"),
    ("LBR", "LR"), ("LBY", "LY"), ("LCA", "LC"), ("LIE", "LI"), ("LKA", "LK"), ("LSO", "LS"), ("LTU", "LT"),
    ("LUX", "LU"), ("LVA", "LV"), ("MAC", "MO"), ("MAF", "MF"), ("MAR", "MA"), ("MCO", "MC"), ("MDA", "MD"),
    ("MDG", "MG"), ("MDV", "MV"), ("MEX", "MX"), ("MHL", "MH"), ("MKD", "MK"), ("MLI", "ML"), ("MLT", "MT"),
    ("MMR", "MM"), ("MNE", "ME"), ("MNG", "MN"), ("MNP", "MP"), ("MOZ", "MZ"), ("MRT", "MR"), ("MSR", "MS"),
    ("MTQ", "MQ"), ("MUS", "MU"), ("MWI", "MW"), ("MYS", "MY"), ("MYT", "YT"), ("NAM", "NA"), ("NCL", "NC"),
    ("NER", "NE"), ("NFK", "NF"), ("NGA", "NG"), ("NIC", "NI"), ("NIU", "NU"), ("NLD", "NL"), ("NOR", "NO"),
    ("NPL", "NP"), ("NRU", "NR"), ("NZL", "NZ"), ("OMN", "OM"), ("PAK", "PK"), ("PAN", "PA"), ("PCN", "PN"),
    ("PER", "PE"), ("PHL", "PH"), ("PLW", "PW"), ("PNG", "PG"), ("POL", "PL"), ("PRI", "PR"), ("PRK", "KP"),
    ("PRT", "PT"), ("PRY", "PY"), ("PSE", "PS"), ("PYF", "PF"), ("QAT", "QA"), ("REU", "RE"), ("ROU", "RO"),
    ("RUS", "RU"), ("RWA", "RW"), ("SAU", "SA"), ("SDN", "SD"), ("SEN", "SN"), ("SGP", "SG"), ("SGS", "GS"),
    ("SHN", "SH"), ("SJM", "SJ"), ("SLB", "SB"), ("SLE", "SL"), ("SLV", "SV"), ("SMR", "SM"), ("SOM", "SO"),
    ("SPM", "PM"), ("SRB", "RS"), ("SSD", "SS"), ("STP", "ST"), ("SUR", "SR"), ("SVK", "SK"), ("SVN", "SI"),
    ("SWE", "SE"), ("SWZ", "SZ"), ("SXM", "SX"), ("SYC", "SC"), ("SYR", "SY"), ("TCA", "TC"), ("TCD", "TD"),
    ("TGO", "TG"), ("THA", "TH"), ("TJK", "TJ"), ("TKL", "TK"), ("TKM", "TM"), ("TLS", "TL"), ("TON", "TO"),
    ("TTO", "TT"), ("TUN", "TN"), ("TUR", "TR"), ("TUV", "TV"), ("TWN", "TW"), ("TZA", "TZ"), ("UGA", "UG"),
    ("UKR", "UA"), ("UMI", "UM"), ("URY", "UY"), ("USA", "US"), ("UZB", "UZ"), ("VAT", "VA"), ("VCT", "VC"),
    ("VEN", "VE"), ("VGB", "VG"), ("VIR", "VI"), ("VNM", "VN"), ("VUT", "VU"), ("WLF", "WF"), ("WSM", "WS"),
    ("XKX", "XK"), ("YEM", "YE"), ("ZAF", "ZA"), ("ZMB", "ZM"), ("ZWE", "ZW"),
];

/// Alpha-2 code of an ISO 3166-1 alpha-3 code (`USA` → `US`), case-insensitive.
pub fn alpha3_to_alpha2(alpha3: &str) -> Option<&'static str> {
    let code = alpha3.trim().to_ascii_uppercase();
    ALPHA3_TO_ALPHA2.binary_search_by(|(a3, _)| (*a3).cmp(code.as_str())).ok().map(|i| ALPHA3_TO_ALPHA2[i].1)
}

/// Normalizes a country reported by a store to alpha-2 uppercase: an alpha-2 code is kept, an
/// alpha-3 code is converted, anything else becomes empty (unknown).
pub fn normalize(code: &str) -> String {
    let c = code.trim();
    if c.len() == 2 && c.chars().all(|ch| ch.is_ascii_alphabetic()) {
        c.to_ascii_uppercase()
    } else if c.len() == 3 {
        alpha3_to_alpha2(c).unwrap_or("").to_string()
    } else {
        String::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_is_sorted_and_unique() {
        assert!(ALPHA3_TO_ALPHA2.windows(2).all(|w| w[0].0 < w[1].0), "sorted by alpha-3 for binary search");
        let mut a2: Vec<&str> = ALPHA3_TO_ALPHA2.iter().map(|(_, a)| *a).collect();
        a2.sort();
        a2.dedup();
        assert_eq!(a2.len(), ALPHA3_TO_ALPHA2.len(), "alpha-2 codes are unique");
        assert!(ALPHA3_TO_ALPHA2.iter().all(|(a3, a2)| a3.len() == 3 && a2.len() == 2));
    }

    #[test]
    fn storefronts_convert_to_alpha2() {
        assert_eq!(alpha3_to_alpha2("USA"), Some("US"));
        assert_eq!(alpha3_to_alpha2("gbr"), Some("GB"));
        assert_eq!(alpha3_to_alpha2("UKR"), Some("UA"));
        assert_eq!(alpha3_to_alpha2("DEU"), Some("DE"));
        assert_eq!(alpha3_to_alpha2("KOR"), Some("KR"));
        assert_eq!(alpha3_to_alpha2("ZZZ"), None);
    }

    #[test]
    fn store_codes_are_normalized() {
        assert_eq!(normalize("us"), "US");
        assert_eq!(normalize(" JPN "), "JP");
        assert_eq!(normalize("XXX"), "");
        assert_eq!(normalize(""), "");
        assert_eq!(normalize("U1"), "");
        assert_eq!(normalize("United States"), "");
    }
}
