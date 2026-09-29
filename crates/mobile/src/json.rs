//! JSON forms of the reports, so the iOS and Android harnesses write the same
//! result files.

use crate::conformance::ConformanceReport;
use crate::fixtures::FixtureReport;
use crate::metrics::ProcessMetrics;
use crate::node::{NodeInfo, NodeStatus};

fn to_json(value: &impl serde::Serialize) -> String {
    serde_json::to_string(value).unwrap_or_else(|e| format!("{{\"error\":{:?}}}", e.to_string()))
}

#[uniffi::export]
pub fn build_info_json() -> String {
    to_json(&crate::build_info())
}

#[uniffi::export]
pub fn conformance_report_json(report: ConformanceReport) -> String {
    to_json(&report)
}

#[uniffi::export]
pub fn fixture_report_json(report: FixtureReport) -> String {
    to_json(&report)
}

#[uniffi::export]
pub fn process_metrics_json(metrics: ProcessMetrics) -> String {
    to_json(&metrics)
}

#[uniffi::export]
pub fn node_info_json(info: NodeInfo) -> String {
    to_json(&info)
}

#[uniffi::export]
pub fn node_status_json(status: NodeStatus) -> String {
    to_json(&status)
}
