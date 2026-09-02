use anyhow::Result;

use crate::application::LocalStateService;

#[derive(Debug, Clone, serde::Serialize)]
pub struct Pattern {
    pub kind: String,
    pub description: String,
    pub signal_refs: Vec<String>,
    pub count: i64,
    pub severity: String,
}

pub fn detect_patterns(db: &impl LocalStateService, json: bool) -> Result<()> {
    let mut patterns: Vec<Pattern> = Vec::new();

    patterns.extend(find_repeat_failures(db)?);
    patterns.extend(find_label_success_imbalance(db)?);
    patterns.extend(find_escalation_heavy_signals(db)?);

    if patterns.is_empty() {
        if json {
            println!("[]");
        } else {
            println!("No patterns detected yet. Need more dispatch history.");
        }
        return Ok(());
    }

    if json {
        println!("{}", serde_json::to_string_pretty(&patterns)?);
        return Ok(());
    }

    for pattern in &patterns {
        let icon = match pattern.severity.as_str() {
            "high" => "!!",
            "medium" => " !",
            _ => "  ",
        };
        println!(
            "[{}] {} ({}x): {}",
            icon, pattern.kind, pattern.count, pattern.description
        );
        if !pattern.signal_refs.is_empty() {
            let refs = pattern
                .signal_refs
                .iter()
                .take(5)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ");
            println!("     signals: {refs}");
        }
    }

    Ok(())
}

fn find_repeat_failures(db: &impl LocalStateService) -> Result<Vec<Pattern>> {
    let rows = db.get_repeat_failure_counts()?;

    if rows.is_empty() {
        return Ok(Vec::new());
    }

    Ok(vec![Pattern {
        kind: "repeat-failures".to_string(),
        description: format!(
            "{} signal(s) have failed multiple times despite escalation",
            rows.len()
        ),
        signal_refs: rows.iter().map(|(r, _)| r.clone()).collect(),
        count: rows.iter().map(|(_, c)| c).sum(),
        severity: "high".to_string(),
    }])
}

fn find_label_success_imbalance(db: &impl LocalStateService) -> Result<Vec<Pattern>> {
    let metrics = db.get_dispatch_metrics()?;

    let mut patterns = Vec::new();
    for m in &metrics {
        let completed = m.total - m.pending;
        if completed < 3 {
            continue;
        }
        if m.success_rate < 30.0 {
            patterns.push(Pattern {
                kind: "low-success-rate".to_string(),
                description: format!(
                    "'{}' with {} has only {:.0}% success rate ({}/{} completed)",
                    m.label, m.model, m.success_rate, m.successes, completed
                ),
                signal_refs: Vec::new(),
                count: completed,
                severity: if m.success_rate < 10.0 {
                    "high".to_string()
                } else {
                    "medium".to_string()
                },
            });
        }
    }

    Ok(patterns)
}

fn find_escalation_heavy_signals(db: &impl LocalStateService) -> Result<Vec<Pattern>> {
    let rows = db.get_escalation_heavy_counts()?;

    let mut patterns = Vec::new();
    for (label, standard_fails, advanced_attempts, _total) in &rows {
        patterns.push(Pattern {
            kind: "escalation-heavy".to_string(),
            description: format!(
                "'{label}': the standard tier failed {standard_fails}x and the advanced tier ran {advanced_attempts}x; consider selecting the advanced tier by default"
            ),
            signal_refs: Vec::new(),
            count: *standard_fails,
            severity: "medium".to_string(),
        });
    }

    Ok(patterns)
}
