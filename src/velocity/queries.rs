use super::db::VelocityDb;
use serde::Serialize;

// ============================================================================
// Result types
// ============================================================================

#[derive(Debug, Serialize)]
pub struct ServiceSummary {
    pub service: String,
    pub total_requests: i64,
    pub avg_duration_ms: f64,
    pub p50_duration_ms: f64,
    pub p95_duration_ms: f64,
    pub p99_duration_ms: f64,
    pub error_count: i64,
    pub error_rate: f64,
}

#[derive(Debug, Serialize)]
pub struct EndpointSummary {
    pub service: String,
    pub http_method: String,
    pub http_route: String,
    pub request_count: i64,
    pub avg_duration_ms: f64,
    pub p50_duration_ms: f64,
    pub p95_duration_ms: f64,
    pub p99_duration_ms: f64,
    pub error_count: i64,
}

#[derive(Debug, Serialize)]
pub struct SlowRequest {
    pub id: i64,
    pub service: String,
    pub http_method: String,
    pub http_route: String,
    pub duration_ms: f64,
    pub http_status_code: Option<i64>,
    pub start_ts: String,
    pub request_id: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct TimelineBucket {
    pub bucket: String,
    pub service: String,
    pub request_count: i64,
    pub avg_duration_ms: f64,
    pub p95_duration_ms: f64,
    pub error_count: i64,
}

#[derive(Debug, Serialize)]
pub struct CompareResult {
    pub http_method: String,
    pub http_route: String,
    pub before_count: i64,
    pub before_p50: f64,
    pub before_p95: f64,
    pub after_count: i64,
    pub after_p50: f64,
    pub after_p95: f64,
    pub p50_change_pct: f64,
    pub p95_change_pct: f64,
}

#[derive(Debug, Serialize)]
pub struct TraceSpan {
    pub id: i64,
    pub service: String,
    pub name: String,
    pub start_ts: String,
    pub end_ts: Option<String>,
    pub duration_ms: Option<f64>,
    pub http_method: Option<String>,
    pub http_route: Option<String>,
    pub http_status_code: Option<i64>,
    pub success: bool,
    pub error: Option<String>,
    pub attributes: Option<String>,
}

// ============================================================================
// Query parameters
// ============================================================================

#[derive(Debug, Default)]
pub struct QueryFilter {
    pub since: Option<String>,
    pub until: Option<String>,
    pub service: Option<String>,
}

// ============================================================================
// Helpers
// ============================================================================

fn percentile(sorted_values: &[f64], p: f64) -> f64 {
    if sorted_values.is_empty() {
        return 0.0;
    }
    let idx = ((p / 100.0) * (sorted_values.len() as f64 - 1.0)).round() as usize;
    sorted_values[idx.min(sorted_values.len() - 1)]
}

fn avg(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.iter().sum::<f64>() / values.len() as f64
}

/// Build a WHERE clause fragment and corresponding parameter values from a QueryFilter.
/// Returns (clause_string, params_vec) where clause_string starts with " WHERE " or is empty.
fn build_where_clause(filter: &QueryFilter) -> (String, Vec<String>) {
    let mut conditions = Vec::new();
    let mut params = Vec::new();

    if let Some(ref since) = filter.since {
        params.push(since.clone());
        conditions.push(format!("start_ts >= ?{}", params.len()));
    }
    if let Some(ref until) = filter.until {
        params.push(until.clone());
        conditions.push(format!("start_ts <= ?{}", params.len()));
    }
    if let Some(ref service) = filter.service {
        params.push(service.clone());
        conditions.push(format!("service = ?{}", params.len()));
    }

    if conditions.is_empty() {
        (String::new(), params)
    } else {
        (format!(" WHERE {}", conditions.join(" AND ")), params)
    }
}

/// Bind string params to a rusqlite statement. This is a convenience helper
/// since we build dynamic WHERE clauses with variable param counts.
fn bind_params(stmt: &mut rusqlite::Statement<'_>, params: &[String]) -> rusqlite::Result<()> {
    for (i, param) in params.iter().enumerate() {
        stmt.raw_bind_parameter(i + 1, param)?;
    }
    Ok(())
}

// ============================================================================
// Queries
// ============================================================================

/// Per-service summary with percentile breakdowns.
pub fn get_summary(db: &VelocityDb, filter: &QueryFilter) -> anyhow::Result<Vec<ServiceSummary>> {
    let conn = db.conn();

    let (where_clause, params) = build_where_clause(filter);

    // First, get the distinct services
    let services_sql = format!(
        "SELECT DISTINCT service FROM velocity_spans{}",
        where_clause
    );
    let mut services_stmt = conn.prepare(&services_sql)?;
    bind_params(&mut services_stmt, &params)?;
    let services: Vec<String> = services_stmt
        .raw_query()
        .mapped(|row| row.get(0))
        .filter_map(|r| r.ok())
        .collect();

    let mut results = Vec::new();

    for service in &services {
        // Get all durations for this service
        let dur_sql = format!(
            "SELECT duration_ms FROM velocity_spans{} AND service = ?{}",
            if where_clause.is_empty() {
                " WHERE 1=1"
            } else {
                &where_clause
            },
            params.len() + 1
        );
        let mut dur_stmt = conn.prepare(&dur_sql)?;
        let mut all_params = params.clone();
        all_params.push(service.clone());
        bind_params(&mut dur_stmt, &all_params)?;

        let mut durations: Vec<f64> = dur_stmt
            .raw_query()
            .mapped(|row| row.get::<_, Option<f64>>(0))
            .filter_map(|r| r.ok())
            .flatten()
            .collect();
        durations.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

        // Get error count
        let err_sql = format!(
            "SELECT COUNT(*) FROM velocity_spans{} AND service = ?{} AND success = 0",
            if where_clause.is_empty() {
                " WHERE 1=1"
            } else {
                &where_clause
            },
            params.len() + 1
        );
        let mut err_stmt = conn.prepare(&err_sql)?;
        bind_params(&mut err_stmt, &all_params)?;
        let error_count: i64 = err_stmt
            .raw_query()
            .mapped(|row| row.get(0))
            .filter_map(|r| r.ok())
            .next()
            .unwrap_or(0);

        let total = durations.len() as i64;
        let error_rate = if total > 0 {
            error_count as f64 / total as f64
        } else {
            0.0
        };

        results.push(ServiceSummary {
            service: service.clone(),
            total_requests: total,
            avg_duration_ms: avg(&durations),
            p50_duration_ms: percentile(&durations, 50.0),
            p95_duration_ms: percentile(&durations, 95.0),
            p99_duration_ms: percentile(&durations, 99.0),
            error_count,
            error_rate,
        });
    }

    Ok(results)
}

/// Per-endpoint summary grouped by (service, method, route).
pub fn get_endpoints(
    db: &VelocityDb,
    filter: &QueryFilter,
) -> anyhow::Result<Vec<EndpointSummary>> {
    let conn = db.conn();

    let (where_clause, params) = build_where_clause(filter);
    let base_where = if where_clause.is_empty() {
        " WHERE http_method IS NOT NULL AND http_route IS NOT NULL".to_string()
    } else {
        format!(
            "{} AND http_method IS NOT NULL AND http_route IS NOT NULL",
            where_clause
        )
    };

    // Get distinct endpoint groups
    let groups_sql = format!(
        "SELECT DISTINCT service, http_method, http_route FROM velocity_spans{}",
        base_where
    );
    let mut groups_stmt = conn.prepare(&groups_sql)?;
    bind_params(&mut groups_stmt, &params)?;

    let groups: Vec<(String, String, String)> = groups_stmt
        .raw_query()
        .mapped(|row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .filter_map(|r| r.ok())
        .collect();

    let mut results = Vec::new();

    for (service, method, route) in &groups {
        let dur_sql = format!(
            "SELECT duration_ms FROM velocity_spans{} AND service = ?{} AND http_method = ?{} AND http_route = ?{}",
            if where_clause.is_empty() { " WHERE 1=1" } else { &where_clause },
            params.len() + 1,
            params.len() + 2,
            params.len() + 3,
        );
        let mut dur_stmt = conn.prepare(&dur_sql)?;
        let mut all_params = params.clone();
        all_params.push(service.clone());
        all_params.push(method.clone());
        all_params.push(route.clone());
        bind_params(&mut dur_stmt, &all_params)?;

        let mut durations: Vec<f64> = dur_stmt
            .raw_query()
            .mapped(|row| row.get::<_, Option<f64>>(0))
            .filter_map(|r| r.ok())
            .flatten()
            .collect();
        durations.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

        // Error count for this endpoint
        let err_sql = format!(
            "SELECT COUNT(*) FROM velocity_spans{} AND service = ?{} AND http_method = ?{} AND http_route = ?{} AND success = 0",
            if where_clause.is_empty() { " WHERE 1=1" } else { &where_clause },
            params.len() + 1,
            params.len() + 2,
            params.len() + 3,
        );
        let mut err_stmt = conn.prepare(&err_sql)?;
        bind_params(&mut err_stmt, &all_params)?;
        let error_count: i64 = err_stmt
            .raw_query()
            .mapped(|row| row.get(0))
            .filter_map(|r| r.ok())
            .next()
            .unwrap_or(0);

        results.push(EndpointSummary {
            service: service.clone(),
            http_method: method.clone(),
            http_route: route.clone(),
            request_count: durations.len() as i64,
            avg_duration_ms: avg(&durations),
            p50_duration_ms: percentile(&durations, 50.0),
            p95_duration_ms: percentile(&durations, 95.0),
            p99_duration_ms: percentile(&durations, 99.0),
            error_count,
        });
    }

    // Sort by request count descending for convenience
    results.sort_by(|a, b| b.request_count.cmp(&a.request_count));
    Ok(results)
}

/// Slowest requests above a given threshold (ms), ordered by duration descending.
pub fn get_slow_requests(
    db: &VelocityDb,
    filter: &QueryFilter,
    threshold_ms: f64,
    limit: usize,
) -> anyhow::Result<Vec<SlowRequest>> {
    let conn = db.conn();

    let (where_clause, params) = build_where_clause(filter);
    let threshold_condition = format!(" AND duration_ms > ?{}", params.len() + 1);

    let sql = format!(
        "SELECT id, service, COALESCE(http_method, ''), COALESCE(http_route, ''), duration_ms, http_status_code, start_ts, request_id, error \
         FROM velocity_spans{}{} ORDER BY duration_ms DESC LIMIT ?{}",
        if where_clause.is_empty() { " WHERE 1=1" } else { &where_clause },
        threshold_condition,
        params.len() + 2,
    );

    let mut stmt = conn.prepare(&sql)?;

    // Bind filter params
    for (i, param) in params.iter().enumerate() {
        stmt.raw_bind_parameter(i + 1, param)?;
    }
    // Bind threshold
    stmt.raw_bind_parameter(params.len() + 1, threshold_ms)?;
    // Bind limit
    stmt.raw_bind_parameter(params.len() + 2, limit as i64)?;

    let results: Vec<SlowRequest> = stmt
        .raw_query()
        .mapped(|row| {
            Ok(SlowRequest {
                id: row.get(0)?,
                service: row.get(1)?,
                http_method: row.get(2)?,
                http_route: row.get(3)?,
                duration_ms: row.get(4)?,
                http_status_code: row.get(5)?,
                start_ts: row.get(6)?,
                request_id: row.get(7)?,
                error: row.get(8)?,
            })
        })
        .filter_map(|r| r.ok())
        .collect();

    Ok(results)
}

/// Timeline bucketed by 1-minute intervals.
pub fn get_timeline(db: &VelocityDb, filter: &QueryFilter) -> anyhow::Result<Vec<TimelineBucket>> {
    let conn = db.conn();

    let (where_clause, params) = build_where_clause(filter);

    // Get distinct (bucket, service) groups
    let groups_sql = format!(
        "SELECT DISTINCT substr(start_ts, 1, 16) AS bucket, service \
         FROM velocity_spans{} \
         ORDER BY bucket, service",
        if where_clause.is_empty() {
            " WHERE 1=1"
        } else {
            &where_clause
        },
    );
    let mut groups_stmt = conn.prepare(&groups_sql)?;
    bind_params(&mut groups_stmt, &params)?;

    let groups: Vec<(String, String)> = groups_stmt
        .raw_query()
        .mapped(|row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))
        .filter_map(|r| r.ok())
        .collect();

    let mut results = Vec::new();

    for (bucket, service) in &groups {
        // Get durations for this bucket+service
        let dur_sql = format!(
            "SELECT duration_ms FROM velocity_spans{} AND substr(start_ts, 1, 16) = ?{} AND service = ?{}",
            if where_clause.is_empty() { " WHERE 1=1" } else { &where_clause },
            params.len() + 1,
            params.len() + 2,
        );
        let mut dur_stmt = conn.prepare(&dur_sql)?;
        let mut all_params = params.clone();
        all_params.push(bucket.clone());
        all_params.push(service.clone());
        bind_params(&mut dur_stmt, &all_params)?;

        let mut durations: Vec<f64> = dur_stmt
            .raw_query()
            .mapped(|row| row.get::<_, Option<f64>>(0))
            .filter_map(|r| r.ok())
            .flatten()
            .collect();
        durations.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

        // Error count for this bucket+service
        let err_sql = format!(
            "SELECT COUNT(*) FROM velocity_spans{} AND substr(start_ts, 1, 16) = ?{} AND service = ?{} AND success = 0",
            if where_clause.is_empty() { " WHERE 1=1" } else { &where_clause },
            params.len() + 1,
            params.len() + 2,
        );
        let mut err_stmt = conn.prepare(&err_sql)?;
        bind_params(&mut err_stmt, &all_params)?;
        let error_count: i64 = err_stmt
            .raw_query()
            .mapped(|row| row.get(0))
            .filter_map(|r| r.ok())
            .next()
            .unwrap_or(0);

        results.push(TimelineBucket {
            bucket: bucket.clone(),
            service: service.clone(),
            request_count: durations.len() as i64,
            avg_duration_ms: avg(&durations),
            p95_duration_ms: percentile(&durations, 95.0),
            error_count,
        });
    }

    Ok(results)
}

/// Compare two time windows per-endpoint to detect regressions.
pub fn get_compare(
    db: &VelocityDb,
    before_start: &str,
    before_end: &str,
    after_start: &str,
    after_end: &str,
    service: Option<&str>,
) -> anyhow::Result<Vec<CompareResult>> {
    let conn = db.conn();

    // Build optional service filter
    let service_clause = if service.is_some() {
        " AND service = ?5"
    } else {
        ""
    };

    // Get all distinct endpoints that appear in either window
    let endpoints_sql = format!(
        "SELECT DISTINCT http_method, http_route FROM velocity_spans \
         WHERE http_method IS NOT NULL AND http_route IS NOT NULL \
         AND ((start_ts >= ?1 AND start_ts <= ?2) OR (start_ts >= ?3 AND start_ts <= ?4)){} \
         ORDER BY http_method, http_route",
        service_clause
    );
    let mut endpoints_stmt = conn.prepare(&endpoints_sql)?;
    endpoints_stmt.raw_bind_parameter(1, before_start)?;
    endpoints_stmt.raw_bind_parameter(2, before_end)?;
    endpoints_stmt.raw_bind_parameter(3, after_start)?;
    endpoints_stmt.raw_bind_parameter(4, after_end)?;
    if let Some(svc) = service {
        endpoints_stmt.raw_bind_parameter(5, svc)?;
    }

    let endpoints: Vec<(String, String)> = endpoints_stmt
        .raw_query()
        .mapped(|row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))
        .filter_map(|r| r.ok())
        .collect();

    let mut results = Vec::new();

    for (method, route) in &endpoints {
        // Before window durations
        let before_durations =
            fetch_durations_for_window(&conn, before_start, before_end, method, route, service)?;

        // After window durations
        let after_durations =
            fetch_durations_for_window(&conn, after_start, after_end, method, route, service)?;

        let before_p50 = percentile(&before_durations, 50.0);
        let before_p95 = percentile(&before_durations, 95.0);
        let after_p50 = percentile(&after_durations, 50.0);
        let after_p95 = percentile(&after_durations, 95.0);

        let p50_change_pct = if before_p50 > 0.0 {
            ((after_p50 - before_p50) / before_p50) * 100.0
        } else {
            0.0
        };
        let p95_change_pct = if before_p95 > 0.0 {
            ((after_p95 - before_p95) / before_p95) * 100.0
        } else {
            0.0
        };

        results.push(CompareResult {
            http_method: method.clone(),
            http_route: route.clone(),
            before_count: before_durations.len() as i64,
            before_p50,
            before_p95,
            after_count: after_durations.len() as i64,
            after_p50,
            after_p95,
            p50_change_pct,
            p95_change_pct,
        });
    }

    // Sort by largest p95 regression first
    results.sort_by(|a, b| {
        b.p95_change_pct
            .partial_cmp(&a.p95_change_pct)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    Ok(results)
}

/// Fetch sorted durations for a specific endpoint within a time window.
fn fetch_durations_for_window(
    conn: &rusqlite::Connection,
    start: &str,
    end: &str,
    method: &str,
    route: &str,
    service: Option<&str>,
) -> anyhow::Result<Vec<f64>> {
    let service_clause = if service.is_some() {
        " AND service = ?5"
    } else {
        ""
    };

    let sql = format!(
        "SELECT duration_ms FROM velocity_spans \
         WHERE start_ts >= ?1 AND start_ts <= ?2 \
         AND http_method = ?3 AND http_route = ?4 \
         AND duration_ms IS NOT NULL{}",
        service_clause
    );

    let mut stmt = conn.prepare(&sql)?;
    stmt.raw_bind_parameter(1, start)?;
    stmt.raw_bind_parameter(2, end)?;
    stmt.raw_bind_parameter(3, method)?;
    stmt.raw_bind_parameter(4, route)?;
    if let Some(svc) = service {
        stmt.raw_bind_parameter(5, svc)?;
    }

    let mut durations: Vec<f64> = stmt
        .raw_query()
        .mapped(|row| row.get::<_, f64>(0))
        .filter_map(|r| r.ok())
        .collect();

    durations.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    Ok(durations)
}

/// Look up all spans sharing the same request_id (trace reconstruction).
pub fn get_trace(db: &VelocityDb, request_id: &str) -> anyhow::Result<Vec<TraceSpan>> {
    let conn = db.conn();

    let mut stmt = conn.prepare(
        "SELECT id, service, name, start_ts, end_ts, duration_ms, http_method, http_route, \
         http_status_code, success, error, attributes \
         FROM velocity_spans WHERE request_id = ?1 ORDER BY start_ts ASC",
    )?;

    let results: Vec<TraceSpan> = stmt
        .query_map([request_id], |row| {
            Ok(TraceSpan {
                id: row.get(0)?,
                service: row.get(1)?,
                name: row.get(2)?,
                start_ts: row.get(3)?,
                end_ts: row.get(4)?,
                duration_ms: row.get(5)?,
                http_method: row.get(6)?,
                http_route: row.get(7)?,
                http_status_code: row.get(8)?,
                success: row.get::<_, i32>(9).map(|v| v != 0)?,
                error: row.get(10)?,
                attributes: row.get(11)?,
            })
        })?
        .filter_map(|r| r.ok())
        .collect();

    Ok(results)
}
