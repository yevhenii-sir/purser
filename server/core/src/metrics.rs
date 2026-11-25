//! Prometheus text exposition (version 0.0.4) of the server's numbers, for `GET /metrics`.

/// Gauges grouped by metric: `HELP` and `TYPE` once, then every sample of the metric (the format
/// wants a metric's samples together, whatever order they were added in).
#[derive(Default)]
pub struct Exposition {
    families: Vec<(String, String, Vec<String>)>,
}

/// A label value with `\`, `"` and line feeds escaped.
fn escape(v: &str) -> String {
    v.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n")
}

/// `12`, `0.5`, `NaN` / `+Inf` for what is not a finite number.
fn number(v: f64) -> String {
    if v.is_nan() {
        "NaN".into()
    } else if v.is_infinite() {
        if v > 0.0 { "+Inf" } else { "-Inf" }.into()
    } else if v.fract() == 0.0 && v.abs() < 1e15 {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}

impl Exposition {
    pub fn new() -> Self {
        Self::default()
    }

    /// One sample of the gauge `name` (`purser_…`, `[a-z_]` only); `help` is kept from its first
    /// sample.
    pub fn gauge(&mut self, name: &str, help: &str, labels: &[(&str, &str)], value: f64) {
        let labels = if labels.is_empty() {
            String::new()
        } else {
            format!("{{{}}}", labels.iter().map(|(k, v)| format!("{k}=\"{}\"", escape(v))).collect::<Vec<_>>().join(","))
        };
        let line = format!("{name}{labels} {}", number(value));
        match self.families.iter_mut().find(|(n, ..)| n == name) {
            Some((_, _, lines)) => lines.push(line),
            None => self.families.push((name.to_string(), help.to_string(), vec![line])),
        }
    }

    /// The text, `Content-Type: text/plain; version=0.0.4`.
    pub fn finish(self) -> String {
        let mut out = String::new();
        for (name, help, lines) in self.families {
            out.push_str(&format!("# HELP {name} {}\n# TYPE {name} gauge\n", help.replace('\n', " ")));
            for l in lines {
                out.push_str(&l);
                out.push('\n');
            }
        }
        out
    }
}

/// Content type of [`Exposition::finish`].
pub const CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn samples_of_a_metric_are_grouped_and_labels_escaped() {
        let mut e = Exposition::new();
        e.gauge("purser_up", "1 while the server answers.", &[], 1.0);
        e.gauge("purser_purchases", "Ledger rows by build and status.", &[("app_id", "com.a"), ("status", "completed")], 12.0);
        e.gauge("purser_revenue_today_usd", "Gross revenue today.", &[("app_id", "com.a")], 4.5);
        e.gauge("purser_purchases", "ignored", &[("app_id", "we\"ird\\")], 3.0);
        let text = e.finish();
        assert_eq!(
            text,
            "# HELP purser_up 1 while the server answers.\n# TYPE purser_up gauge\npurser_up 1\n\
             # HELP purser_purchases Ledger rows by build and status.\n# TYPE purser_purchases gauge\n\
             purser_purchases{app_id=\"com.a\",status=\"completed\"} 12\npurser_purchases{app_id=\"we\\\"ird\\\\\"} 3\n\
             # HELP purser_revenue_today_usd Gross revenue today.\n# TYPE purser_revenue_today_usd gauge\n\
             purser_revenue_today_usd{app_id=\"com.a\"} 4.5\n"
        );
        assert_eq!((number(f64::NAN), number(f64::INFINITY)), ("NaN".to_string(), "+Inf".to_string()));
    }
}
