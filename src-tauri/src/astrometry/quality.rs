use super::matcher::AstrometricMatch;
use crate::reduction::SourceMeasurement;
use serde::Serialize;
use std::collections::HashSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReductionStatus {
    Accepted,
    ReviewRequired,
    Rejected,
}

impl ReductionStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Accepted => "accepted",
            Self::ReviewRequired => "review_required",
            Self::Rejected => "rejected",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct AstrometricQuality {
    pub status: ReductionStatus,
    pub matched: usize,
    pub occupied_grid_cells: usize,
    pub residual_rms_arcsec: f64,
    pub residual_median_arcsec: f64,
    pub residual_p68_arcsec: f64,
    pub residual_p95_arcsec: f64,
    pub rms_ra_arcsec: f64,
    pub rms_dec_arcsec: f64,
    pub mean_ra_arcsec: f64,
    pub mean_dec_arcsec: f64,
    pub spatial_trend_arcsec: f64,
    /// Escalation trigger, not a rejection reason by itself. A SIP order is
    /// fitted when the spatial residual trend exceeds the 0.30 arcsec
    /// threshold; the solution is only rejected when the trend persists at the
    /// requested maximum order (or the fitted polynomial is implausible).
    pub distortion_suspected: bool,
    pub plate_model_requested: String,
    pub plate_model_applied: String,
    pub sip_order: Option<usize>,
    pub sip_escalation_attempted: bool,
    pub sip_escalation_succeeded: bool,
    /// `None` when no SIP distortion was fitted, otherwise whether the
    /// alternating CD/polynomial fit converged. A non-converged fit is never
    /// accepted, so `Some(false)` always accompanies a rejection.
    pub sip_fit_converged: Option<bool>,
    pub plate_model_downgraded: bool,
    pub plate_model_downgrade_reason: Option<String>,
    pub reasons: Vec<String>,
}

pub fn evaluate_astrometric_quality(
    matches: &[AstrometricMatch],
    sources: &[SourceMeasurement],
    width: u32,
    height: u32,
) -> AstrometricQuality {
    let mut residuals: Vec<f64> = matches
        .iter()
        .filter(|item| item.used && item.residual_arcsec.is_finite())
        .map(|item| item.residual_arcsec)
        .collect();
    residuals.sort_by(f64::total_cmp);
    let matched = residuals.len();
    let percentile = |fraction: f64| -> f64 {
        if residuals.is_empty() {
            return f64::INFINITY;
        }
        let index = ((residuals.len() - 1) as f64 * fraction).round() as usize;
        residuals[index]
    };
    let rms = if matched == 0 {
        f64::INFINITY
    } else {
        (residuals.iter().map(|value| value * value).sum::<f64>() / matched as f64).sqrt()
    };
    let used: Vec<_> = matches.iter().filter(|item| item.used).collect();
    let mean_ra =
        used.iter().map(|item| item.residual_x_arcsec).sum::<f64>() / used.len().max(1) as f64;
    let mean_dec =
        used.iter().map(|item| item.residual_y_arcsec).sum::<f64>() / used.len().max(1) as f64;
    let rms_ra = (used
        .iter()
        .map(|item| item.residual_x_arcsec.powi(2))
        .sum::<f64>()
        / used.len().max(1) as f64)
        .sqrt();
    let rms_dec = (used
        .iter()
        .map(|item| item.residual_y_arcsec.powi(2))
        .sum::<f64>()
        / used.len().max(1) as f64)
        .sqrt();
    let mut cells = HashSet::new();
    let mut center_vectors = Vec::new();
    let mut edge_vectors = Vec::new();
    for item in &used {
        let Some(source) = sources.get(item.image_source_index) else {
            continue;
        };
        let column = ((source.x / f64::from(width.max(1))) * 4.0)
            .floor()
            .clamp(0.0, 3.0) as usize;
        let row = ((source.y / f64::from(height.max(1))) * 4.0)
            .floor()
            .clamp(0.0, 3.0) as usize;
        cells.insert(row * 4 + column);
        let nx = source.x / f64::from(width.max(1)) - 0.5;
        let ny = source.y / f64::from(height.max(1)) - 0.5;
        let vector = (item.residual_x_arcsec, item.residual_y_arcsec);
        if nx.abs().max(ny.abs()) >= 0.32 {
            edge_vectors.push(vector);
        } else {
            center_vectors.push(vector);
        }
    }
    let vector_mean = |values: &[(f64, f64)]| -> (f64, f64) {
        if values.is_empty() {
            return (0.0, 0.0);
        }
        (
            values.iter().map(|item| item.0).sum::<f64>() / values.len() as f64,
            values.iter().map(|item| item.1).sum::<f64>() / values.len() as f64,
        )
    };
    let center = vector_mean(&center_vectors);
    let edge = vector_mean(&edge_vectors);
    let spatial_trend = (edge.0 - center.0).hypot(edge.1 - center.1);
    let distortion_suspected =
        center_vectors.len() >= 3 && edge_vectors.len() >= 6 && spatial_trend > 0.30;
    let occupied_grid_cells = cells.len();
    let p95 = percentile(0.95);

    let accepted = matched >= 15
        && occupied_grid_cells >= 8
        && rms <= 0.5
        && p95 <= 1.0
        && !distortion_suspected;
    let review = matched >= 8 && occupied_grid_cells >= 4 && rms <= 1.0 && p95 <= 2.0;
    let status = if accepted {
        ReductionStatus::Accepted
    } else if review {
        ReductionStatus::ReviewRequired
    } else {
        ReductionStatus::Rejected
    };
    let mut reasons = Vec::new();
    if matched < 15 {
        reasons.push(format!("only {matched} validated references"));
    }
    if occupied_grid_cells < 8 {
        reasons.push(format!(
            "references cover {occupied_grid_cells}/16 grid cells"
        ));
    }
    if rms > 0.5 {
        reasons.push(format!("RMS {rms:.3} arcsec exceeds accepted limit"));
    }
    if p95 > 1.0 {
        reasons.push(format!("P95 {p95:.3} arcsec exceeds accepted limit"));
    }
    if distortion_suspected {
        reasons.push("spatial residual trend suggests unmodelled distortion".into());
    }

    AstrometricQuality {
        status,
        matched,
        occupied_grid_cells,
        residual_rms_arcsec: rms,
        residual_median_arcsec: percentile(0.5),
        residual_p68_arcsec: percentile(0.68),
        residual_p95_arcsec: p95,
        rms_ra_arcsec: rms_ra,
        rms_dec_arcsec: rms_dec,
        mean_ra_arcsec: mean_ra,
        mean_dec_arcsec: mean_dec,
        spatial_trend_arcsec: spatial_trend,
        distortion_suspected,
        plate_model_requested: "linear".into(),
        plate_model_applied: "linear".into(),
        sip_order: None,
        sip_escalation_attempted: false,
        sip_escalation_succeeded: false,
        sip_fit_converged: None,
        plate_model_downgraded: false,
        plate_model_downgrade_reason: None,
        reasons,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(x: f64, y: f64) -> SourceMeasurement {
        SourceMeasurement {
            x,
            y,
            peak: 100.0,
            flux: 1_000.0,
            fwhm: 3.0,
            ellipticity: 0.1,
            npix: 20,
            flags: 0,
            saturated: false,
            snr: Some(30.0),
            x_error_px: Some(0.05),
            y_error_px: Some(0.05),
            centroid_refined: true,
        }
    }

    #[test]
    fn accepts_well_distributed_low_residual_solution() {
        let mut sources = Vec::new();
        let mut matches = Vec::new();
        for row in 0..4 {
            for column in 0..4 {
                sources.push(source(
                    80.0 + column as f64 * 250.0,
                    80.0 + row as f64 * 250.0,
                ));
                matches.push(AstrometricMatch {
                    image_source_index: sources.len() - 1,
                    catalog_source_index: sources.len() - 1,
                    residual_arcsec: 0.12,
                    residual_x_arcsec: 0.08,
                    residual_y_arcsec: 0.09,
                    weight: 1.0,
                    used: true,
                    rejection_reason: None,
                });
            }
        }
        let quality = evaluate_astrometric_quality(&matches, &sources, 1_000, 1_000);
        assert_eq!(quality.status, ReductionStatus::Accepted);
    }
}
