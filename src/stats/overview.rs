//! Overview 视图共享数据模型。

use std::collections::HashMap;

use super::MATRIX_AGENTS;
use super::aggregate::{top_models_covering, totals_by_agent_model, totals_by_model};
use super::date::date_offset;
use super::types::{Period, UsageRecord, UsageTotals};

const OVERVIEW_CHART_MODEL_COVERAGE: f64 = 0.80;

#[derive(Debug, Clone)]
pub(super) struct OverviewChartSeries {
    pub(super) model: String,
    pub(super) values: Vec<f64>,
    pub(super) color_index: usize,
}

#[derive(Debug, Clone)]
pub(super) struct OverviewChartData {
    pub(super) min_date: String,
    pub(super) max_date: String,
    pub(super) dates: Vec<String>,
    pub(super) max_y: f64,
    pub(super) series: Vec<OverviewChartSeries>,
    pub(super) has_records: bool,
}

#[derive(Debug, Clone)]
pub(super) struct OverviewTableRow {
    pub(super) model: String,
    pub(super) usage: UsageTotals,
    pub(super) color_index: usize,
}

#[derive(Debug, Clone)]
pub(super) struct OverviewTableData {
    pub(super) rows: Vec<OverviewTableRow>,
    pub(super) total_all: u64,
    pub(super) cells: HashMap<(String, String), UsageTotals>,
    pub(super) agent_columns: Vec<(&'static str, &'static str)>,
}

#[derive(Debug, Clone)]
pub(super) struct OverviewData {
    pub(super) chart: OverviewChartData,
    pub(super) table: OverviewTableData,
}

pub(super) fn build_overview_data(
    records: &[UsageRecord],
    today: &str,
    period: Period,
) -> OverviewData {
    let filtered: Vec<&UsageRecord> = records
        .iter()
        .filter(|record| period.includes(&record.date, today))
        .collect();
    build_overview_data_from_refs(&filtered, today, period)
}

pub(super) fn build_overview_data_from_refs(
    records: &[&UsageRecord],
    today: &str,
    period: Period,
) -> OverviewData {
    let cells = totals_by_agent_model(records);
    let totals = totals_by_model(records);
    let rows = sort_model_rows(&totals);
    let total_all: u64 = rows.iter().map(|row| row.usage.total_tokens()).sum();
    let agent_columns = sorted_agents_by_usage(&cells, true);

    let color_indices: HashMap<String, usize> = rows
        .iter()
        .map(|row| (row.model.clone(), row.color_index))
        .collect();
    let chart_models = top_models_covering(&totals, OVERVIEW_CHART_MODEL_COVERAGE);
    let (min_date, max_date) = chart_date_range(period, today, records)
        .unwrap_or_else(|| (today.to_string(), today.to_string()));
    let dates = date_span(&min_date, &max_date);

    let mut per_model_date: HashMap<String, HashMap<String, u64>> = HashMap::new();
    for model in &chart_models {
        per_model_date.insert(model.clone(), HashMap::new());
    }
    for record in records {
        if let Some(daily) = per_model_date.get_mut(&record.model) {
            let entry = daily.entry(record.date.clone()).or_insert(0);
            *entry = entry.saturating_add(record.in_tokens + record.out_tokens);
        }
    }

    let mut max_y: f64 = 0.0;
    let mut series = Vec::with_capacity(chart_models.len());
    for model in chart_models {
        let daily = per_model_date.get(&model);
        let values: Vec<f64> = dates
            .iter()
            .map(|date| {
                let value = daily
                    .and_then(|model_daily| model_daily.get(date))
                    .copied()
                    .unwrap_or(0) as f64;
                max_y = max_y.max(value);
                value
            })
            .collect();
        series.push(OverviewChartSeries {
            color_index: color_indices.get(&model).copied().unwrap_or(0),
            model,
            values,
        });
    }

    OverviewData {
        chart: OverviewChartData {
            min_date,
            max_date,
            dates,
            max_y,
            series,
            has_records: !records.is_empty(),
        },
        table: OverviewTableData {
            rows,
            total_all,
            cells,
            agent_columns,
        },
    }
}

pub(super) fn sort_model_rows(totals: &HashMap<String, UsageTotals>) -> Vec<OverviewTableRow> {
    let mut rows: Vec<OverviewTableRow> = totals
        .iter()
        .filter(|(_, usage)| usage.total_tokens() > 0)
        .map(|(model, usage)| OverviewTableRow {
            model: model.clone(),
            usage: *usage,
            color_index: 0,
        })
        .collect();
    rows.sort_by(|left, right| {
        right
            .usage
            .total_tokens()
            .cmp(&left.usage.total_tokens())
            .then_with(|| left.model.cmp(&right.model))
    });
    for (index, row) in rows.iter_mut().enumerate() {
        row.color_index = index;
    }
    rows
}

pub(super) fn chart_date_range(
    period: Period,
    today: &str,
    records: &[&UsageRecord],
) -> Option<(String, String)> {
    match period {
        Period::Today => Some((today.to_string(), today.to_string())),
        Period::Lastday => Some((date_offset(today, -1).ok()?, date_offset(today, -1).ok()?)),
        Period::LastDays(days) => {
            let days = i64::from(days);
            Some((date_offset(today, -(days - 1)).ok()?, today.to_string()))
        }
        Period::LastMonthDays => {
            let days = super::date::previous_month_days(today) as i64;
            Some((date_offset(today, -(days - 1)).ok()?, today.to_string()))
        }
        Period::All => {
            let first = records.first()?;
            let mut min_date = first.date.clone();
            let mut max_date = first.date.clone();
            for record in records.iter().skip(1) {
                if record.date.as_str() < min_date.as_str() {
                    min_date = record.date.clone();
                }
                if record.date.as_str() > max_date.as_str() {
                    max_date = record.date.clone();
                }
            }
            Some((min_date, max_date))
        }
    }
}

pub(super) fn sorted_agents_by_usage(
    cells: &HashMap<(String, String), UsageTotals>,
    hide_empty_agents: bool,
) -> Vec<(&'static str, &'static str)> {
    let mut agents: Vec<(usize, &'static str, &'static str, u64)> = MATRIX_AGENTS
        .iter()
        .enumerate()
        .map(|(idx, (agent, label))| {
            let total = cells
                .iter()
                .filter(|((cell_agent, _), _)| cell_agent == agent)
                .map(|(_, usage)| usage.total_tokens())
                .sum();
            (idx, *agent, *label, total)
        })
        .collect();

    if hide_empty_agents {
        agents.retain(|(_, _, _, total)| *total > 0);
    }
    agents.sort_by(|left, right| right.3.cmp(&left.3).then(left.0.cmp(&right.0)));
    agents
        .into_iter()
        .map(|(_, agent, label, _)| (agent, label))
        .collect()
}

fn date_span(min_date: &str, max_date: &str) -> Vec<String> {
    let mut dates = vec![min_date.to_string()];
    let mut current = min_date.to_string();
    while current.as_str() < max_date {
        let next = date_offset(&current, 1).unwrap_or_else(|_| max_date.to_string());
        if next == current {
            break;
        }
        dates.push(next.clone());
        current = next;
    }
    dates
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(
        agent: &str,
        model: &str,
        date: &str,
        in_tokens: u64,
        out_tokens: u64,
    ) -> UsageRecord {
        UsageRecord {
            agent: agent.to_string(),
            model: model.to_string(),
            date: date.to_string(),
            in_tokens,
            total_tokens: in_tokens + out_tokens,
            out_tokens,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 0,
        }
    }

    #[test]
    fn overview_chart_uses_full_relative_window() {
        let records = vec![record("claude", "a", "2026-05-29", 10, 5)];
        let data = build_overview_data(&records, "2026-05-29", Period::LastDays(7));
        assert_eq!(data.chart.min_date, "2026-05-23");
        assert_eq!(data.chart.max_date, "2026-05-29");
        assert_eq!(data.chart.dates.len(), 7);
        assert_eq!(
            data.chart.series[0].values,
            vec![0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 15.0]
        );
    }

    #[test]
    fn overview_table_keeps_all_non_zero_models() {
        let records = vec![
            record("claude", "a", "2026-05-29", 80, 20),
            record("claude", "b", "2026-05-29", 10, 0),
            record("codex", "c", "2026-05-29", 5, 0),
        ];
        let data = build_overview_data(&records, "2026-05-29", Period::Today);
        let models: Vec<&str> = data
            .table
            .rows
            .iter()
            .map(|row| row.model.as_str())
            .collect();
        assert_eq!(models, vec!["a", "b", "c"]);
        assert_eq!(data.chart.series.len(), 1);
        assert_eq!(data.chart.series[0].model, "a");
    }

    #[test]
    fn overview_agents_are_sorted_and_empty_agents_hidden() {
        let records = vec![
            record("codex", "a", "2026-05-29", 10, 0),
            record("claude", "a", "2026-05-29", 50, 0),
        ];
        let data = build_overview_data(&records, "2026-05-29", Period::Today);
        assert_eq!(
            data.table.agent_columns,
            vec![("claude", "Claude Code"), ("codex", "Codex")]
        );
    }
}
