//! Minimal ClickHouse client over its HTTP interface: batched inserts of
//! fleet metrics/events and a couple of history queries. ClickHouse runs
//! next to the master (plain HTTP on loopback), so a few lines of HTTP/1.0
//! over a TcpStream replace an HTTP client dependency.

use std::collections::HashMap;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

use crate::api::system::HistoryPoint;
use crate::config::ClickhouseConfig;

const FLUSH_EVERY: Duration = Duration::from_secs(30);
const FLUSH_AT_ROWS: usize = 5_000;
/// While ClickHouse is down, keep at most this many rows (oldest dropped).
const MAX_BUFFERED: usize = 50_000;
const IO_TIMEOUT: Duration = Duration::from_secs(30);

pub enum Row {
    /// One sample; `extra` adds memory total, disk usage and swap when known.
    Metric { node: String, point: HistoryPoint, extra: Option<super::Summary> },
    Event { node: String, ts: i64, kind: String, detail: String },
    Container { node: String, ts: i64, stat: super::containers::ContainerStat },
}

#[derive(Clone)]
pub struct Clickhouse {
    cfg: ClickhouseConfig,
    /// `host:port`
    addr: String,
}

impl Clickhouse {
    pub fn new(cfg: &ClickhouseConfig) -> Self {
        let addr = cfg
            .url
            .trim_start_matches("http://")
            .split('/')
            .next()
            .unwrap_or_default()
            .to_string();
        Self { cfg: cfg.clone(), addr }
    }

    /// Run one statement; `params` become `{name:Type}` query parameters, so
    /// values never get spliced into SQL.
    pub async fn query(&self, sql: &str, params: &[(&str, &str)], body: &[u8]) -> anyhow::Result<String> {
        let mut path = format!("/?query={}", urlencode(sql));
        for (k, v) in params {
            path.push_str(&format!("&param_{k}={}", urlencode(v)));
        }
        let head = format!(
            "POST {path} HTTP/1.0\r\nHost: {}\r\nX-ClickHouse-User: {}\r\nX-ClickHouse-Key: {}\r\n\
             Content-Length: {}\r\n\r\n",
            self.addr,
            self.cfg.user,
            self.cfg.password,
            body.len()
        );
        let exchange = async {
            let mut s = TcpStream::connect(&self.addr).await?;
            s.write_all(head.as_bytes()).await?;
            s.write_all(body).await?;
            let mut out = Vec::new();
            s.read_to_end(&mut out).await?;
            anyhow::Ok(out)
        };
        let raw = tokio::time::timeout(IO_TIMEOUT, exchange)
            .await
            .map_err(|_| anyhow::anyhow!("clickhouse: timed out"))??;
        let text = String::from_utf8_lossy(&raw);
        let (head, body) = text
            .split_once("\r\n\r\n")
            .ok_or_else(|| anyhow::anyhow!("clickhouse: malformed response"))?;
        let status = head.split_whitespace().nth(1).unwrap_or("");
        anyhow::ensure!(status == "200", "clickhouse {status}: {}", body.trim());
        Ok(body.to_string())
    }

    /// Tables and the 1-minute rollup. Idempotent; run at startup and retried
    /// before a flush until it succeeds.
    pub async fn init_schema(&self) -> anyhow::Result<()> {
        let db = &self.cfg.database;
        anyhow::ensure!(
            db.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'),
            "SS_CLICKHOUSE_DB must be alphanumeric"
        );
        let statements = [
            format!("CREATE DATABASE IF NOT EXISTS {db}"),
            format!(
                "CREATE TABLE IF NOT EXISTS {db}.node_metrics (
                    node LowCardinality(String),
                    ts DateTime CODEC(DoubleDelta, ZSTD),
                    cpu Float32 CODEC(Gorilla, ZSTD),
                    mem_used UInt64 CODEC(T64, ZSTD),
                    rx Float32 CODEC(Gorilla, ZSTD),
                    tx Float32 CODEC(Gorilla, ZSTD),
                    disk_r Float32 CODEC(Gorilla, ZSTD),
                    disk_w Float32 CODEC(Gorilla, ZSTD),
                    load1 Float32 CODEC(Gorilla, ZSTD)
                ) ENGINE = MergeTree ORDER BY (node, ts)
                TTL ts + INTERVAL 90 DAY"
            ),
            format!(
                "CREATE TABLE IF NOT EXISTS {db}.node_metrics_1m (
                    node LowCardinality(String),
                    t DateTime,
                    n SimpleAggregateFunction(sum, UInt64),
                    cpu_sum SimpleAggregateFunction(sum, Float64),
                    cpu_max SimpleAggregateFunction(max, Float32),
                    mem_sum SimpleAggregateFunction(sum, Float64),
                    rx_sum SimpleAggregateFunction(sum, Float64),
                    tx_sum SimpleAggregateFunction(sum, Float64),
                    load_sum SimpleAggregateFunction(sum, Float64)
                ) ENGINE = AggregatingMergeTree ORDER BY (node, t)
                TTL t + INTERVAL 2 YEAR"
            ),
            // Columns added after the first release.
            format!(
                "ALTER TABLE {db}.node_metrics
                   ADD COLUMN IF NOT EXISTS mem_total UInt64 DEFAULT 0 CODEC(T64, ZSTD),
                   ADD COLUMN IF NOT EXISTS disk_used UInt64 DEFAULT 0 CODEC(T64, ZSTD),
                   ADD COLUMN IF NOT EXISTS disk_total UInt64 DEFAULT 0 CODEC(T64, ZSTD),
                   ADD COLUMN IF NOT EXISTS swap_used UInt64 DEFAULT 0 CODEC(T64, ZSTD),
                   ADD COLUMN IF NOT EXISTS swap_total UInt64 DEFAULT 0 CODEC(T64, ZSTD)"
            ),
            format!(
                "ALTER TABLE {db}.node_metrics_1m
                   ADD COLUMN IF NOT EXISTS mem_max SimpleAggregateFunction(max, UInt64) DEFAULT 0,
                   ADD COLUMN IF NOT EXISTS mem_total_max SimpleAggregateFunction(max, UInt64) DEFAULT 0,
                   ADD COLUMN IF NOT EXISTS disk_used_max SimpleAggregateFunction(max, UInt64) DEFAULT 0,
                   ADD COLUMN IF NOT EXISTS disk_total_max SimpleAggregateFunction(max, UInt64) DEFAULT 0,
                   ADD COLUMN IF NOT EXISTS swap_used_max SimpleAggregateFunction(max, UInt64) DEFAULT 0,
                   ADD COLUMN IF NOT EXISTS swap_total_max SimpleAggregateFunction(max, UInt64) DEFAULT 0,
                   ADD COLUMN IF NOT EXISTS rx_max SimpleAggregateFunction(max, Float32) DEFAULT 0,
                   ADD COLUMN IF NOT EXISTS tx_max SimpleAggregateFunction(max, Float32) DEFAULT 0,
                   ADD COLUMN IF NOT EXISTS disk_r_sum SimpleAggregateFunction(sum, Float64) DEFAULT 0,
                   ADD COLUMN IF NOT EXISTS disk_w_sum SimpleAggregateFunction(sum, Float64) DEFAULT 0"
            ),
            // The rollup view with the new columns replaces the first one.
            format!(
                "CREATE MATERIALIZED VIEW IF NOT EXISTS {db}.node_metrics_1m_mv2
                 TO {db}.node_metrics_1m AS
                 SELECT node, toStartOfMinute(ts) AS t, count() AS n,
                        sum(cpu) AS cpu_sum, max(cpu) AS cpu_max, sum(mem_used) AS mem_sum,
                        sum(rx) AS rx_sum, sum(tx) AS tx_sum, sum(load1) AS load_sum,
                        max(mem_used) AS mem_max, max(mem_total) AS mem_total_max,
                        max(disk_used) AS disk_used_max, max(disk_total) AS disk_total_max,
                        max(swap_used) AS swap_used_max, max(swap_total) AS swap_total_max,
                        max(rx) AS rx_max, max(tx) AS tx_max,
                        sum(disk_r) AS disk_r_sum, sum(disk_w) AS disk_w_sum
                 FROM {db}.node_metrics GROUP BY node, t"
            ),
            format!("DROP VIEW IF EXISTS {db}.node_metrics_1m_mv"),
            format!(
                "CREATE TABLE IF NOT EXISTS {db}.container_metrics (
                    node LowCardinality(String),
                    ts DateTime CODEC(DoubleDelta, ZSTD),
                    project LowCardinality(String),
                    service LowCardinality(String),
                    container LowCardinality(String),
                    cpu Float32 CODEC(Gorilla, ZSTD),
                    mem UInt64 CODEC(T64, ZSTD),
                    mem_limit UInt64 CODEC(T64, ZSTD),
                    rx Float32 CODEC(Gorilla, ZSTD),
                    tx Float32 CODEC(Gorilla, ZSTD)
                ) ENGINE = MergeTree ORDER BY (node, container, ts)
                TTL ts + INTERVAL 90 DAY"
            ),
            format!(
                "CREATE TABLE IF NOT EXISTS {db}.fleet_events (
                    node LowCardinality(String),
                    ts DateTime,
                    kind LowCardinality(String),
                    detail String
                ) ENGINE = MergeTree ORDER BY (node, ts)
                TTL ts + INTERVAL 1 YEAR"
            ),
        ];
        for sql in statements {
            self.query(&sql, &[], b"").await?;
        }
        Ok(())
    }

    /// Newest stored point per node, so agents only replay what is missing.
    pub async fn last_ts_per_node(&self) -> anyhow::Result<HashMap<String, f64>> {
        let db = &self.cfg.database;
        let body = self
            .query(
                &format!("SELECT node, toUnixTimestamp(max(ts)) FROM {db}.node_metrics GROUP BY node FORMAT TSV"),
                &[],
                b"",
            )
            .await?;
        Ok(body
            .lines()
            .filter_map(|l| {
                let (node, ts) = l.split_once('\t')?;
                Some((node.to_string(), ts.parse().ok()?))
            })
            .collect())
    }

    async fn rows(&self, sql: &str, params: &[(&str, &str)]) -> anyhow::Result<Vec<serde_json::Value>> {
        let body = self
            .query(
                &format!("{sql} SETTINGS output_format_json_quote_64bit_integers = 0 FORMAT JSONCompact"),
                params,
                b"",
            )
            .await?;
        let v: serde_json::Value = serde_json::from_str(&body)?;
        Ok(v["data"].as_array().cloned().unwrap_or_default())
    }

    /// Bucket size giving ~240 points over `hours` (never under a minute).
    pub fn step(hours: u32) -> u64 {
        ((hours as u64 * 3600) / 240).max(60)
    }

    /// Downsampled history for one node over `hours`. Each row:
    /// `[x, cpu_avg, cpu_max, mem_avg, mem_max, rx_avg, tx_avg, load_avg,
    ///   disk_r_avg, disk_w_avg, disk_used, disk_total, swap_used, swap_total,
    ///   mem_total, rx_max, tx_max]`.
    pub async fn history(&self, node: &str, hours: u32) -> anyhow::Result<serde_json::Value> {
        let db = &self.cfg.database;
        let step = Self::step(hours).to_string();
        let hours = hours.to_string();
        let rows = self
            .rows(
                &format!(
                    "SELECT toUnixTimestamp(toStartOfInterval(t, toIntervalSecond({{step:UInt32}}))) AS x,
                            sum(cpu_sum) / sum(n), max(cpu_max), sum(mem_sum) / sum(n), toFloat64(max(mem_max)),
                            sum(rx_sum) / sum(n), sum(tx_sum) / sum(n), sum(load_sum) / sum(n),
                            sum(disk_r_sum) / sum(n), sum(disk_w_sum) / sum(n),
                            toFloat64(max(disk_used_max)), toFloat64(max(disk_total_max)),
                            toFloat64(max(swap_used_max)), toFloat64(max(swap_total_max)),
                            toFloat64(max(mem_total_max)), max(rx_max), max(tx_max)
                     FROM {db}.node_metrics_1m
                     WHERE node = {{node:String}} AND t >= now() - toIntervalHour({{hours:UInt32}})
                     GROUP BY x ORDER BY x"
                ),
                &[("node", node), ("step", &step), ("hours", &hours)],
            )
            .await?;
        Ok(serde_json::Value::Array(rows))
    }

    /// One metric for several nodes, bucketed: rows `[node, x, value]`.
    /// `metric`: `cpu` (avg %) or `mem` (avg % of total).
    pub async fn compare(&self, nodes: &[String], hours: u32, metric: &str) -> anyhow::Result<Vec<serde_json::Value>> {
        let db = &self.cfg.database;
        let value = match metric {
            "mem" => "sum(mem_sum) / sum(n) / nullIf(max(mem_total_max), 0) * 100",
            _ => "sum(cpu_sum) / sum(n)",
        };
        let step = Self::step(hours).to_string();
        let hours = hours.to_string();
        let list = array_param(nodes);
        self.rows(
            &format!(
                "SELECT node, toUnixTimestamp(toStartOfInterval(t, toIntervalSecond({{step:UInt32}}))) AS x, {value}
                 FROM {db}.node_metrics_1m
                 WHERE has({{nodes:Array(String)}}, node) AND t >= now() - toIntervalHour({{hours:UInt32}})
                 GROUP BY node, x ORDER BY x"
            ),
            &[("nodes", &list), ("step", &step), ("hours", &hours)],
        )
        .await
    }

    /// Events, newest first: rows `[node, unix_secs, kind, detail_json]`.
    pub async fn events(&self, nodes: &[String], hours: u32, kind: Option<&str>) -> anyhow::Result<Vec<serde_json::Value>> {
        let db = &self.cfg.database;
        let hours = hours.to_string();
        let list = array_param(nodes);
        let kind = kind.unwrap_or("");
        self.rows(
            &format!(
                "SELECT node, toUnixTimestamp(ts), kind, detail FROM {db}.fleet_events
                 WHERE has({{nodes:Array(String)}}, node) AND ts >= now() - toIntervalHour({{hours:UInt32}})
                   AND ({{kind:String}} = '' OR kind = {{kind:String}})
                 ORDER BY ts DESC LIMIT 2000"
            ),
            &[("nodes", &list), ("hours", &hours), ("kind", kind)],
        )
        .await
    }

    /// Minutes with at least one sample, and the first such minute, over `days`.
    pub async fn coverage(&self, node: &str, days: u32) -> anyhow::Result<(u64, i64)> {
        let db = &self.cfg.database;
        let days = days.to_string();
        let rows = self
            .rows(
                &format!(
                    "SELECT uniqExact(t), toUnixTimestamp(min(t)) FROM {db}.node_metrics_1m
                     WHERE node = {{node:String}} AND t >= now() - toIntervalDay({{days:UInt32}})"
                ),
                &[("node", node), ("days", &days)],
            )
            .await?;
        let r = rows.first().cloned().unwrap_or_default();
        Ok((r[0].as_u64().unwrap_or(0), r[1].as_i64().unwrap_or(0)))
    }

    /// Container lifecycle counts over `days`: rows `[container, restarts, dies, ooms]`.
    pub async fn container_events(&self, node: &str, days: u32) -> anyhow::Result<Vec<serde_json::Value>> {
        let db = &self.cfg.database;
        let days = days.to_string();
        self.rows(
            &format!(
                "SELECT JSONExtractString(detail, 'name') AS c,
                        countIf(JSONExtractString(detail, 'action') = 'restart'),
                        countIf(JSONExtractString(detail, 'action') = 'die'),
                        countIf(JSONExtractString(detail, 'action') = 'oom')
                 FROM {db}.fleet_events
                 WHERE node = {{node:String}} AND kind = 'docker' AND ts >= now() - toIntervalDay({{days:UInt32}})
                 GROUP BY c HAVING c != '' ORDER BY 3 DESC, 2 DESC"
            ),
            &[("node", node), ("days", &days)],
        )
        .await
    }

    /// Per-container series over `hours`: rows `[project, service, container, x, cpu_avg, mem_max]`.
    pub async fn containers(&self, node: &str, hours: u32) -> anyhow::Result<Vec<serde_json::Value>> {
        let db = &self.cfg.database;
        let step = ((hours as u64 * 3600) / 60).max(60).to_string();
        let hours = hours.to_string();
        self.rows(
            &format!(
                "SELECT project, service, container,
                        toUnixTimestamp(toStartOfInterval(ts, toIntervalSecond({{step:UInt32}}))) AS x,
                        avg(cpu), toFloat64(max(mem))
                 FROM {db}.container_metrics
                 WHERE node = {{node:String}} AND ts >= now() - toIntervalHour({{hours:UInt32}})
                 GROUP BY project, service, container, x ORDER BY container, x"
            ),
            &[("node", node), ("step", &step), ("hours", &hours)],
        )
        .await
    }
}

/// Batching writer: rows arrive on a channel and are flushed every 30 s (or
/// at 5 000 rows) as one JSONEachRow insert per table.
pub fn spawn_writer(ch: Clickhouse, mut rx: mpsc::Receiver<Row>) {
    tokio::spawn(async move {
        let mut schema_ready = match ch.init_schema().await {
            Ok(()) => true,
            Err(e) => {
                tracing::warn!("clickhouse schema init failed (will retry): {e}");
                false
            }
        };
        let mut buf: Vec<Row> = Vec::new();
        let mut tick = tokio::time::interval(FLUSH_EVERY);
        let mut failing = false;
        let mut closed = false;
        loop {
            let flush_now = tokio::select! {
                row = rx.recv() => match row {
                    Some(row) => {
                        buf.push(row);
                        buf.len() >= FLUSH_AT_ROWS
                    }
                    None => {
                        closed = true; // shutting down: final flush
                        true
                    }
                },
                _ = tick.tick() => true,
            };
            if !flush_now || buf.is_empty() {
                if closed {
                    return;
                }
                continue;
            }
            if !schema_ready {
                schema_ready = ch.init_schema().await.is_ok();
            }
            match flush(&ch, &buf).await {
                Ok(()) => {
                    if failing {
                        tracing::info!("clickhouse writes recovered");
                    }
                    failing = false;
                    buf.clear();
                }
                Err(e) => {
                    if !failing {
                        tracing::warn!("clickhouse insert failed, buffering: {e}");
                    }
                    failing = true;
                    if buf.len() > MAX_BUFFERED {
                        let drop = buf.len() - MAX_BUFFERED;
                        buf.drain(..drop);
                    }
                }
            }
            if closed {
                return;
            }
        }
    });
}

async fn flush(ch: &Clickhouse, rows: &[Row]) -> anyhow::Result<()> {
    let db = &ch.cfg.database;
    let mut metrics = String::new();
    let mut events = String::new();
    let mut containers = String::new();
    for row in rows {
        match row {
            Row::Metric { node, point: p, extra } => {
                let x = extra.clone().unwrap_or_default();
                metrics.push_str(
                    &serde_json::json!({
                        "node": node, "ts": p[0] as u64, "cpu": p[1], "mem_used": p[2] as u64,
                        "rx": p[3], "tx": p[4], "disk_r": p[5], "disk_w": p[6], "load1": p[7],
                        "mem_total": x.mem_total, "disk_used": x.disk_used, "disk_total": x.disk_total,
                        "swap_used": x.swap_used, "swap_total": x.swap_total,
                    })
                    .to_string(),
                );
                metrics.push('\n');
            }
            Row::Container { node, ts, stat } => {
                containers.push_str(
                    &serde_json::json!({
                        "node": node, "ts": ts, "project": stat.project, "service": stat.service,
                        "container": stat.container, "cpu": stat.cpu, "mem": stat.mem,
                        "mem_limit": stat.mem_limit, "rx": stat.rx, "tx": stat.tx,
                    })
                    .to_string(),
                );
                containers.push('\n');
            }
            Row::Event { node, ts, kind, detail } => {
                events.push_str(
                    &serde_json::json!({ "node": node, "ts": ts, "kind": kind, "detail": detail })
                        .to_string(),
                );
                events.push('\n');
            }
        }
    }
    if !metrics.is_empty() {
        ch.query(&format!("INSERT INTO {db}.node_metrics FORMAT JSONEachRow"), &[], metrics.as_bytes())
            .await?;
    }
    if !events.is_empty() {
        ch.query(&format!("INSERT INTO {db}.fleet_events FORMAT JSONEachRow"), &[], events.as_bytes())
            .await?;
    }
    if !containers.is_empty() {
        ch.query(&format!("INSERT INTO {db}.container_metrics FORMAT JSONEachRow"), &[], containers.as_bytes())
            .await?;
    }
    Ok(())
}

/// `['a','b']` for an `Array(String)` query parameter.
fn array_param(items: &[String]) -> String {
    let quoted: Vec<String> = items
        .iter()
        .map(|i| format!("'{}'", i.replace('\\', "\\\\").replace('\'', "\\'")))
        .collect();
    format!("[{}]", quoted.join(","))
}

fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}
