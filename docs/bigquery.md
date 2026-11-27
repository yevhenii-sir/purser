# Purchase events in BigQuery

This page shows how to query purchase events in BigQuery. With it, purchases, renewals and refunds can
sit next to the GA4 / Firebase export.

The **Cloud Storage** integration writes every purchase event as a file of one JSON line:
`gs://<bucket>/<folder>/dt=YYYY-MM-DD/<time>-<event>-<transaction>.json`. BigQuery reads these files
directly.

## 1. Export the events

1. Open the admin page → the app → **Integrations** → **Google Cloud Storage**.
2. Enter the bucket and a folder (for example `billing/events`).
3. Add a service account key with the **Storage Object Creator** role on the bucket.
4. Pick the events you want, or leave them all.
5. Click **Send test event** to write a `test` event.

## 2. A table over the files

An external table reads the files where they are, partitioned by the `dt=` folders.

Use the schema of [`bigquery-schema.json`](bigquery-schema.json). Without it, autodetection turns
`attribution` into a sparse record, because its keys differ from player to player. The schema leaves
`attribution` out and reads the same ids from `attribution_json` as a `JSON` column.

```bash
bq mkdef --source_format=NEWLINE_DELIMITED_JSON \
  --hive_partitioning_mode=AUTO \
  --hive_partitioning_source_uri_prefix=gs://<bucket>/billing/events \
  --ignore_unknown_values \
  "gs://<bucket>/billing/events/*" docs/bigquery-schema.json > purser_events.def
bq mk --external_table_definition=purser_events.def <project>:<dataset>.purser_events
```

`--ignore_unknown_values` keeps the table readable when a later server version adds a field.

### Faster queries over a long history

Load the files into a native table instead. For example, load the previous day once a day:

```bash
bq load --source_format=NEWLINE_DELIMITED_JSON --ignore_unknown_values \
  --hive_partitioning_mode=AUTO --hive_partitioning_source_uri_prefix=gs://<bucket>/billing/events \
  <project>:<dataset>.purser_events_native "gs://<bucket>/billing/events/dt=2026-11-23/*" docs/bigquery-schema.json
```

## 3. Queries

Revenue by day and store, refunds negative:

```sql
SELECT dt, store, currency,
  SUM(IF(kind = 'refund', -gross_micros, gross_micros)) / 1e6 AS gross,
  COUNTIF(kind = 'renewal') AS renewals
FROM `<project>.<dataset>.purser_events`
WHERE environment = 'production' AND kind IN ('purchase', 'subscription_start', 'renewal', 'refund')
GROUP BY dt, store, currency
ORDER BY dt DESC
```

Joined with the GA4 export by the Firebase `app_instance_id` (the client sends it with every purchase):

```sql
SELECT e.event_date, p.kind, p.product_id, p.gross_micros / 1e6 AS gross
FROM `<project>.<dataset>.purser_events` p
JOIN `<project>.analytics_<property>.events_*` e
  ON e.user_pseudo_id = p.app_instance_id AND e.event_name = 'first_open'
```

Tenjin installation ids from the attribution:

```sql
SELECT JSON_VALUE(attribution_json, '$.tenjin_id') AS tenjin_id, COUNT(*) AS renewals
FROM `<project>.<dataset>.purser_events`
WHERE kind = 'renewal'
GROUP BY tenjin_id
```

## Notes

- **One file per event.** The storage cost is small, but listing millions of files is slow. Keep the
  external table for recent days and load older ones into a native table.
- **Sandbox purchases** are exported only with **Send sandbox / test purchases** on.
- **Amounts** are in micros of `currency`. The admin page's revenue report converts to USD with ECB
  rates (market rates for the other currencies). To do the same in BigQuery, join a table of daily rates.
