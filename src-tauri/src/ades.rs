use serde::{Deserialize, Serialize};

use crate::measurement::normalize_tracklet_designation;
use crate::report::{RenderedReport, MPC_ACCEPTED_BANDS};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AdesContext {
    pub observatory_code: String,
    pub submitter: String,
    pub observers: Vec<String>,
    pub measurers: Vec<String>,
    pub telescope: Option<String>,
    pub telescope_aperture_m: Option<f64>,
    pub detector: Option<String>,
    pub software_version: String,
    pub position_precision_1e6_deg: bool,
    pub magnitude_precision_hundredth: bool,
    pub mpcorb_sha256: Option<String>,
    pub refcat2_sha256: Option<String>,
}
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AdesObservation {
    pub perm_id: Option<String>,
    pub prov_id: Option<String>,
    pub trk_sub: Option<String>,
    pub mode: String,
    pub obs_time: String,
    pub ra_deg: f64,
    pub dec_deg: f64,
    pub rms_ra_arcsec: Option<f64>,
    pub rms_dec_arcsec: Option<f64>,
    pub ast_cat: String,
    pub mag: Option<f64>,
    pub rms_mag: Option<f64>,
    pub band: Option<String>,
    pub filter: Option<String>,
    pub phot_cat: Option<String>,
    pub phot_ap_arcsec: Option<f64>,
    pub snr: Option<f64>,
    pub seeing_arcsec: Option<f64>,
    pub exposure_seconds: Option<f64>,
    pub rms_fit_arcsec: Option<f64>,
    pub astrometric_reference_stars: Option<usize>,
    pub accepted_wcs: bool,
}
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AdesRequest {
    pub context: AdesContext,
    pub observations: Vec<AdesObservation>,
}

pub fn render(req: &AdesRequest) -> Result<RenderedReport, Vec<String>> {
    let errors = validate(req);
    if !errors.is_empty() {
        return Err(errors);
    }
    let cols = [
        "permID", "provID", "trkSub", "mode", "stn", "obsTime", "ra", "dec", "rmsRA", "rmsDec",
        "astCat", "mag", "rmsMag", "band", "fltr", "photCat", "photAp", "logSNR", "seeing", "exp",
        "rmsFit", "nStars",
    ];
    let mut out = format!(
        "# version=2022\n# observatory\n! mpcCode {}\n# submitter\n! name {}\n# observers\n",
        req.context.observatory_code, req.context.submitter
    );
    for x in &req.context.observers {
        out.push_str(&format!("! name {x}\n"));
    }
    out.push_str("# measurers\n");
    for x in &req.context.measurers {
        out.push_str(&format!("! name {x}\n"));
    }
    if req.context.telescope.is_some()
        || req.context.telescope_aperture_m.is_some()
        || req.context.detector.is_some()
    {
        out.push_str("# telescope\n");
        if let Some(t) = &req.context.telescope {
            out.push_str(&format!("! design {t}\n"));
        }
        if let Some(aperture) = req
            .context
            .telescope_aperture_m
            .filter(|value| *value > 0.0)
        {
            out.push_str(&format!("! aperture {aperture}\n"));
        }
        if let Some(detector) = req
            .context
            .detector
            .as_deref()
            .filter(|value| !value.is_empty())
        {
            out.push_str(&format!("! detector {detector}\n"));
        }
    }
    out.push_str(&format!(
        "# software\n! astrometry {}\n! photometry {}\n",
        req.context.software_version, req.context.software_version
    ));
    out.push_str(&cols.join("|"));
    out.push('\n');
    for o in &req.observations {
        let position_digits = if req.context.position_precision_1e6_deg {
            6
        } else {
            5
        };
        let magnitude_digits = if req.context.magnitude_precision_hundredth {
            2
        } else {
            1
        };
        let values = vec![
            s(&o.perm_id),
            s(&o.prov_id),
            s(&o.trk_sub),
            o.mode.clone(),
            req.context.observatory_code.clone(),
            o.obs_time.clone(),
            format!("{:.position_digits$}", o.ra_deg),
            format!("{:+.position_digits$}", o.dec_deg),
            f(o.rms_ra_arcsec, 3),
            f(o.rms_dec_arcsec, 3),
            o.ast_cat.clone(),
            f(o.mag, magnitude_digits),
            f(o.rms_mag, 3),
            s(&o.band),
            s(&o.filter),
            s(&o.phot_cat),
            f(o.phot_ap_arcsec, 2),
            o.snr
                .map(|v| format!("{:.3}", v.log10()))
                .unwrap_or_default(),
            f(o.seeing_arcsec, 2),
            f(o.exposure_seconds, 2),
            f(o.rms_fit_arcsec, 3),
            o.astrometric_reference_stars
                .map(|value| value.to_string())
                .unwrap_or_default(),
        ];
        out.push_str(&values.join("|"));
        out.push('\n');
    }
    Ok(RenderedReport {
        content: out,
        warnings: warnings(req),
    })
}
fn s(v: &Option<String>) -> String {
    v.clone().unwrap_or_default()
}
fn f(v: Option<f64>, p: usize) -> String {
    v.map(|x| format!("{x:.p$}")).unwrap_or_default()
}
pub fn validate(req: &AdesRequest) -> Vec<String> {
    let mut e = Vec::new();
    let observatory_code = req.context.observatory_code.trim();
    if observatory_code.is_empty() {
        e.push("缺少 MPC 台站代码".into());
    } else if observatory_code.eq_ignore_ascii_case("XXX") {
        e.push("MPC 台站代码仍为占位值 XXX".into());
    } else if !(3..=4).contains(&observatory_code.len())
        || !observatory_code.chars().all(|c| c.is_ascii_alphanumeric())
    {
        e.push("ADES 台站代码必须是 3-4 位字母数字".into());
    }
    if req.context.submitter.trim().is_empty() {
        e.push("缺少提交者".into());
    }
    // ADES `obsContext` requires `measurers` and `telescope` (design + aperture
    // + detector). `observers` is deliberately optional there, unlike the MPC
    // 80-column header, so its absence is not an error in this format.
    if req.context.measurers.is_empty() {
        e.push("ADES 要求至少一位测量者（measurers）".into());
    }
    if req
        .context
        .telescope
        .as_deref()
        .map(str::trim)
        .unwrap_or_default()
        .is_empty()
    {
        e.push("ADES 要求填写望远镜（telescope design）".into());
    }
    if !req
        .context
        .telescope_aperture_m
        .is_some_and(|aperture| aperture > 0.0)
    {
        e.push("ADES 要求填写望远镜口径（aperture > 0）".into());
    }
    if req
        .context
        .detector
        .as_deref()
        .map(str::trim)
        .unwrap_or_default()
        .is_empty()
    {
        e.push("ADES 要求填写探测器（detector）".into());
    }
    if req.observations.is_empty() {
        e.push("没有可导出的观测".into());
    }
    for (i, o) in req.observations.iter().enumerate() {
        let n = i + 1;
        if [&o.perm_id, &o.prov_id, &o.trk_sub]
            .iter()
            .filter(|v| v.as_ref().is_some_and(|s| !s.trim().is_empty()))
            .count()
            != 1
        {
            e.push(format!("第 {n} 条必须且只能设置 permID/provID/trkSub 之一"));
        }
        if let Some(tracklet) = o.trk_sub.as_deref() {
            if let Err(error) = normalize_tracklet_designation(tracklet) {
                e.push(format!("第 {n} 条 trkSub 不合法：{error}"));
            }
        }
        if o.mode.trim().is_empty() {
            e.push(format!("第 {n} 条缺少 mode"));
        }
        if o.obs_time.trim().is_empty() {
            e.push(format!("第 {n} 条缺少 UTC 曝光中点"));
        }
        if !o.ra_deg.is_finite() || !(0.0..360.0).contains(&o.ra_deg) {
            e.push(format!("第 {n} 条 RA 超出 [0, 360)"));
        }
        if !o.dec_deg.is_finite() || !(-90.0..=90.0).contains(&o.dec_deg) {
            e.push(format!("第 {n} 条 Dec 超出 [-90, 90]"));
        }
        if !o.accepted_wcs {
            e.push(format!("第 {n} 条 WCS 未接受"));
        }
        if o.ast_cat != "Gaia3" {
            e.push(format!("第 {n} 条 astCat 必须为 Gaia3"));
        }
        if let Some(magnitude) = o.mag {
            if !(-5.0..=35.0).contains(&magnitude) {
                e.push(format!("第 {n} 条星等超出 ADES 允许的 -5.0..35.0"));
            }
            // The ADES photometry group requires `band` whenever `mag` is set.
            if o.band
                .as_deref()
                .map(str::trim)
                .unwrap_or_default()
                .is_empty()
            {
                e.push(format!("第 {n} 条星等缺少 band"));
            }
        }
    }
    e
}

/// Non-blocking advisories. The MPC stays the authority on submissions, so the
/// band table and name style are surfaced to the operator instead of blocking.
pub fn warnings(req: &AdesRequest) -> Vec<String> {
    let mut w = Vec::new();
    for (i, o) in req.observations.iter().enumerate() {
        let n = i + 1;
        if o.mag.is_none() {
            continue;
        }
        if let Some(band) = o.band.as_deref().map(str::trim).filter(|b| !b.is_empty()) {
            if !MPC_ACCEPTED_BANDS.contains(&band) {
                w.push(format!(
                    "第 {n} 条：波段 {band} 不在 MPC 当前接受的波段表内（C 已不再接受新提交）"
                ));
            }
        }
        if o.phot_cat
            .as_deref()
            .map(str::trim)
            .unwrap_or_default()
            .is_empty()
        {
            w.push(format!("第 {n} 条：有星等时建议提供 photCat"));
        }
    }
    w
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_official_validation_fixture_exactly() {
        let request = AdesRequest {
            context: AdesContext {
                observatory_code: "500".into(),
                submitter: "Sky Eye Test".into(),
                observers: vec!["Test Observer".into()],
                measurers: vec!["Test Measurer".into()],
                telescope: Some("Reflector".into()),
                telescope_aperture_m: Some(0.4),
                detector: Some("CCD".into()),
                software_version: "Sky Eye 0.2.2".into(),
                position_precision_1e6_deg: true,
                magnitude_precision_hundredth: true,
                mpcorb_sha256: None,
                refcat2_sha256: None,
            },
            observations: vec![AdesObservation {
                perm_id: Some("1".into()),
                prov_id: None,
                trk_sub: None,
                mode: "CCD".into(),
                obs_time: "2026-01-01T00:00:00Z".into(),
                ra_deg: 12.345678,
                dec_deg: -23.456789,
                rms_ra_arcsec: Some(0.12),
                rms_dec_arcsec: Some(0.14),
                ast_cat: "Gaia3".into(),
                mag: Some(18.42),
                rms_mag: Some(0.03),
                band: Some("r".into()),
                filter: Some("r".into()),
                phot_cat: Some("ATLAS2".into()),
                phot_ap_arcsec: Some(3.5),
                snr: Some(50.0),
                seeing_arcsec: Some(2.1),
                exposure_seconds: Some(60.0),
                rms_fit_arcsec: Some(0.25),
                astrometric_reference_stars: Some(42),
                accepted_wcs: true,
            }],
        };
        assert_eq!(
            render(&request).expect("valid ADES fixture").content,
            include_str!("../tests/golden/ades-fixed.psv")
        );
    }

    #[test]
    fn blocks_unaccepted_wcs() {
        let r = AdesRequest {
            context: AdesContext {
                observatory_code: "500".into(),
                submitter: "A".into(),
                observers: vec![],
                measurers: vec![],
                telescope: None,
                telescope_aperture_m: None,
                detector: None,
                software_version: "x".into(),
                position_precision_1e6_deg: true,
                magnitude_precision_hundredth: false,
                mpcorb_sha256: None,
                refcat2_sha256: None,
            },
            observations: vec![AdesObservation {
                perm_id: Some("1".into()),
                prov_id: None,
                trk_sub: None,
                mode: "CCD".into(),
                obs_time: "2026-01-01T00:00:00Z".into(),
                ra_deg: 1.,
                dec_deg: -1.,
                rms_ra_arcsec: None,
                rms_dec_arcsec: None,
                ast_cat: "Gaia3".into(),
                mag: None,
                rms_mag: None,
                band: None,
                filter: None,
                phot_cat: None,
                phot_ap_arcsec: None,
                snr: None,
                seeing_arcsec: None,
                exposure_seconds: None,
                rms_fit_arcsec: None,
                astrometric_reference_stars: None,
                accepted_wcs: false,
            }],
        };
        assert!(render(&r).is_err());
    }
}
